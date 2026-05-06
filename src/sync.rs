//! Single Tidepool data-sync tick.
//!
//! Each type is fetched from its watermark, or from the backfill window
//! when there is no watermark yet. Records are converted to Nightscout
//! documents and posted one at a time.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use nightscout::client::{BOLUS_DEDUP_EPSILON, BOLUS_DEDUP_WINDOW_MS, TREATMENT_DEDUP_WINDOW_MS};
use nightscout::{Entry, NightscoutClient, Treatment};
use rayon::prelude::*;
use serde_json::Value;
use tidepool::TidepoolClient;

use crate::convert_data::{self, Converted};
use crate::watermark;

pub const SYNC_TYPES: &[&str] = &[
    "cbg",
    "smbg",
    "bolus",
    "basal",
    "food",
    "deviceEvent",
    "wizard",
    "physicalActivity",
];

pub struct SyncOptions<'a> {
    pub backfill_days: i64,
    pub dry_run: bool,
    pub watermark_path: &'a Path,
}

#[derive(Default, Debug)]
pub struct SyncStats {
    pub fetched: usize,
    pub entries_ok: usize,
    pub entries_fail: usize,
    pub treatments_ok: usize,
    pub treatments_fail: usize,
    pub skipped: usize,
    /// Treatments skipped because Nightscout already has a fuzzy match.
    pub bolus_dedup_skipped: usize,
    pub unknown_types: BTreeMap<String, usize>,
    pub convert_errors: usize,
    /// Newest fetched `time` per Tidepool type.
    pub latest_per_type: BTreeMap<String, DateTime<Utc>>,
}

pub fn sync_once(
    tp: &TidepoolClient,
    patient_uid: &str,
    ns: Option<&NightscoutClient>,
    opts: &SyncOptions<'_>,
) -> Result<SyncStats> {
    if opts.backfill_days < 0 {
        bail!("backfill_days must be zero or greater");
    }

    let now = Utc::now();
    let mut wm = watermark::load(opts.watermark_path).context("loading watermark")?;
    let mut stats = SyncStats::default();

    for type_ in SYNC_TYPES {
        let start = wm
            .latest_for(type_)
            .map(|t| t + Duration::milliseconds(1))
            .unwrap_or_else(|| now - Duration::days(opts.backfill_days));
        eprintln!(
            "[{type_}] fetching {} .. {}",
            start.to_rfc3339(),
            now.to_rfc3339(),
        );
        let records = tp
            .get_data_raw(patient_uid, &[type_], Some(start), Some(now))
            .with_context(|| format!("fetching {type_} data"))?;
        stats.fetched += records.len();
        eprintln!("[{type_}] got {} record(s)", records.len());

        let outcome = process_records(&records, ns, &mut stats);
        if outcome.had_post_failures {
            eprintln!(
                "[{type_}] leaving watermark unchanged because one or more Nightscout POSTs failed"
            );
            continue;
        }

        if let Some(newest) = outcome.newest {
            wm.advance(type_, newest);
            stats.latest_per_type.insert((*type_).to_string(), newest);
        }
    }

    if !opts.dry_run {
        watermark::save(opts.watermark_path, &wm).context("saving watermark")?;
    } else {
        eprintln!("(dry-run) watermark NOT saved");
    }
    Ok(stats)
}

#[derive(Debug, Default)]
struct ProcessOutcome {
    newest: Option<DateTime<Utc>>,
    had_post_failures: bool,
}

/// Convert records, post or preview docs, and return the newest record time.
fn process_records(
    records: &[Value],
    ns: Option<&NightscoutClient>,
    stats: &mut SyncStats,
) -> ProcessOutcome {
    let mut entries: Vec<Entry> = Vec::new();
    let mut treatments: Vec<Treatment> = Vec::new();
    let mut newest: Option<DateTime<Utc>> = None;
    let mut had_post_failures = false;

    for rec in records {
        match convert_data::convert(rec) {
            Ok(Converted::Entry(e)) => {
                entries.push(*e);
                advance_from_record_time(rec, &mut newest);
            }
            Ok(Converted::Treatment(t)) => {
                treatments.push(*t);
                advance_from_record_time(rec, &mut newest);
            }
            Ok(Converted::Skipped(_reason)) => {
                stats.skipped += 1;
                advance_from_record_time(rec, &mut newest);
            }
            Ok(Converted::UnknownType(name)) => {
                *stats.unknown_types.entry(name).or_insert(0) += 1;
                advance_from_record_time(rec, &mut newest);
            }
            Err(e) => {
                stats.convert_errors += 1;
                eprintln!(
                    "convert failed ({e:#}) on record id={:?}",
                    rec.get("id").and_then(|v| v.as_str())
                );
                // Do not let one malformed record block later sync ticks.
                advance_from_record_time(rec, &mut newest);
            }
        }
    }

    match ns {
        Some(ns) => {
            let entry_results: Vec<_> = entries
                .par_iter()
                .map(|e| ns.post_document("entries", e))
                .collect();
            for r in entry_results {
                match r {
                    Ok(()) => stats.entries_ok += 1,
                    Err(e) => {
                        stats.entries_fail += 1;
                        had_post_failures = true;
                        eprintln!("entries POST failed: {e:#}");
                    }
                }
            }
            let treatment_results: Vec<_> = treatments
                .par_iter()
                .map(|t| post_treatment_with_dedup(ns, t))
                .collect();
            for r in treatment_results {
                match r {
                    Ok(TreatmentPost::Posted) => stats.treatments_ok += 1,
                    Ok(TreatmentPost::DedupSkipped) => stats.bolus_dedup_skipped += 1,
                    Err(e) => {
                        stats.treatments_fail += 1;
                        had_post_failures = true;
                        eprintln!("treatments POST failed: {e:#}");
                    }
                }
            }
        }
        None => {
            // Dry-run: serial to keep stdout readable.
            for e in &entries {
                println!(
                    "[dry-run] POST /api/v3/entries\n{}",
                    serde_json::to_string_pretty(e).unwrap_or_default()
                );
                stats.entries_ok += 1;
            }
            for t in &treatments {
                println!(
                    "[dry-run] POST /api/v3/treatments\n{}",
                    serde_json::to_string_pretty(t).unwrap_or_default()
                );
                stats.treatments_ok += 1;
            }
        }
    }

    ProcessOutcome {
        newest,
        had_post_failures,
    }
}

enum TreatmentPost {
    Posted,
    DedupSkipped,
}

/// POST a treatment with fuzzy dedup check.
///
/// Dedup-lookup failures fall back to POSTing.
fn post_treatment_with_dedup(ns: &NightscoutClient, t: &Treatment) -> Result<TreatmentPost> {
    if t.is_bolus()
        && let Some(insulin) = t.insulin
    {
        match ns.has_matching_bolus(
            t.base.date,
            insulin,
            BOLUS_DEDUP_WINDOW_MS,
            BOLUS_DEDUP_EPSILON,
        ) {
            Ok(true) => {
                eprintln!(
                    "[bolus dedup] NS already has a match for insulin={insulin} near date={}; skipping identifier={:?}",
                    t.base.date, t.base.identifier
                );
                return Ok(TreatmentPost::DedupSkipped);
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "[bolus dedup] lookup failed ({e:#}); posting identifier={:?} anyway",
                t.base.identifier
            ),
        }
    } else if let Some(event_type) = t.dedup_event_type() {
        match ns.has_matching_treatment(event_type, t.base.date, TREATMENT_DEDUP_WINDOW_MS) {
            Ok(true) => {
                eprintln!(
                    "[event dedup] NS already has a {event_type} near date={}; skipping identifier={:?}",
                    t.base.date, t.base.identifier
                );
                return Ok(TreatmentPost::DedupSkipped);
            }
            Ok(false) => {}
            Err(e) => eprintln!(
                "[event dedup] lookup failed ({e:#}); posting identifier={:?} anyway",
                t.base.identifier
            ),
        }
    }
    ns.post_document("treatments", t)
        .map(|()| TreatmentPost::Posted)
}

fn advance_from_record_time(rec: &Value, newest: &mut Option<DateTime<Utc>>) {
    if let Some(t) = rec.get("time").and_then(|v| v.as_str())
        && let Ok(dt) = DateTime::parse_from_rfc3339(t)
    {
        let utc = dt.with_timezone(&Utc);
        *newest = Some(match *newest {
            Some(prev) if prev > utc => prev,
            _ => utc,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn process_records_marks_post_failures() {
        let ns = NightscoutClient::new("http://127.0.0.1".to_string(), "role".to_string());
        let records = vec![json!({
            "type": "cbg",
            "id": "cbg1",
            "time": "2026-04-21T12:00:00Z",
            "value": 9.6
        })];
        let mut stats = SyncStats::default();

        let outcome = process_records(&records, Some(&ns), &mut stats);

        assert!(outcome.had_post_failures);
        assert_eq!(stats.entries_fail, 1);
        assert_eq!(
            outcome.newest,
            Some(
                DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn process_records_dry_run_allows_watermark_advance() {
        let records = vec![json!({
            "type": "cbg",
            "id": "cbg1",
            "time": "2026-04-21T12:00:00Z",
            "value": 9.6
        })];
        let mut stats = SyncStats::default();

        let outcome = process_records(&records, None, &mut stats);

        assert!(!outcome.had_post_failures);
        assert_eq!(stats.entries_ok, 1);
        assert!(outcome.newest.is_some());
    }
}
