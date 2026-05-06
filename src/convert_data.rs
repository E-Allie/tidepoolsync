//! Tidepool record to Nightscout document conversion.
//!
//! Emit Schema: `<kind>-<epoch_seconds>`

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use nightscout::{DocumentBase, Entry, Treatment};
use rust_decimal::Decimal;
use serde_json::Value;

use tidepool::MMOL_TO_MGDL;

pub const APP_NAME: &str = "tidepoolsync";

pub enum Converted {
    Entry(Box<Entry>),
    Treatment(Box<Treatment>),
    Skipped(String),
    UnknownType(String),
}

pub fn convert(record: &Value) -> Result<Converted> {
    let type_ = record
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Tidepool record has no `type` field"))?;

    match type_ {
        "cbg" => cbg_to_entry(record).map(|e| Converted::Entry(Box::new(e))),
        "smbg" => smbg_to_entry(record).map(|e| Converted::Entry(Box::new(e))),
        "bolus" => bolus_to_treatment(record).map(|t| Converted::Treatment(Box::new(t))),
        "basal" => basal_to_treatment(record).map(|t| Converted::Treatment(Box::new(t))),
        "food" => food_to_treatment(record).map(|t| Converted::Treatment(Box::new(t))),
        "deviceEvent" => device_event_dispatch(record),
        "wizard" => Ok(Converted::Skipped(
            "wizard: bolus calculator records need bolus-link merging; skipped for now".into(),
        )),
        "physicalActivity" => {
            physical_activity_to_treatment(record).map(|t| Converted::Treatment(Box::new(t)))
        }

        // Handled elsewhere or not mapped.
        "pumpSettings" => Ok(Converted::Skipped(
            "pumpSettings handled by --sync-profile, not data sync".into(),
        )),
        "dosingDecision" => Ok(Converted::Skipped(
            "dosingDecision: Loop algorithm internals are not mapped".into(),
        )),
        "cgmSettings" | "insulin" | "reportedState" | "upload" => {
            Ok(Converted::Skipped(format!("{type_}: Not Mapped")))
        }
        other => Ok(Converted::UnknownType(other.to_string())),
    }
}

// Shared helpers.

fn id(record: &Value) -> Result<&str> {
    record
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Tidepool record missing `id`"))
}

fn time(record: &Value) -> Result<DateTime<Utc>> {
    let t = record
        .get("time")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Tidepool record missing `time`"))?;
    DateTime::parse_from_rfc3339(t)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| anyhow!("bad Tidepool `time` {t:?}: {e}"))
}

fn utc_offset_minutes(record: &Value) -> Option<i32> {
    record
        .get("timezoneOffset")
        .and_then(|v| v.as_i64())
        .and_then(|i| i32::try_from(i).ok())
}

fn device_id(record: &Value) -> Option<String> {
    record
        .get("deviceId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn mmol_to_mgdl_round(mmol: f64) -> f64 {
    (mmol * MMOL_TO_MGDL).round()
}

/// Convert an f64 to a Decimal rounded to 4 decimal places.
fn f64_to_decimal(v: f64) -> Option<Decimal> {
    Decimal::from_f64_retain(v).map(|d| d.round_dp(4))
}

/// Build a Tidepool identifier from the source record id.
fn base_entry(record: &Value, type_suffix: &str) -> Result<DocumentBase> {
    let id = id(record)?;
    let date = time(record)?.timestamp_millis();
    Ok(DocumentBase {
        identifier: Some(format!("tp-{type_suffix}-{id}")),
        date,
        utc_offset: utc_offset_minutes(record),
        app: APP_NAME.to_string(),
        device: device_id(record),
        id_internal: None,
        srv_created: None,
        subject: None,
        srv_modified: None,
        modified_by: None,
        is_valid: None,
        is_read_only: None,
    })
}

/// Build an identifier shared with twiistsync for the same event kind.
fn base_shared(record: &Value, kind: &str) -> Result<DocumentBase> {
    let t = time(record)?;
    Ok(DocumentBase {
        identifier: Some(format!("{kind}-{}", t.timestamp())),
        date: t.timestamp_millis(),
        utc_offset: utc_offset_minutes(record),
        app: APP_NAME.to_string(),
        device: device_id(record),
        id_internal: None,
        srv_created: None,
        subject: None,
        srv_modified: None,
        modified_by: None,
        is_valid: None,
        is_read_only: None,
    })
}

// Glucose.

fn cbg_to_entry(record: &Value) -> Result<Entry> {
    let mmol = record
        .get("value")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("cbg record missing numeric `value`"))?;
    let sgv = mmol_to_mgdl_round(mmol);
    Ok(Entry {
        // Shared with twiistsync for cross-tool dedup.
        base: base_shared(record, "cgm")?,
        type_: Some("sgv".to_string()),
        sgv: f64_to_decimal(sgv),
        direction: None,
        noise: None,
        filtered: None,
        unfiltered: None,
        rssi: None,
        units: Some("mg/dl".to_string()),
    })
}

fn smbg_to_entry(record: &Value) -> Result<Entry> {
    let mmol = record
        .get("value")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("smbg record missing numeric `value`"))?;
    let sgv = mmol_to_mgdl_round(mmol);
    Ok(Entry {
        // Twiist has no fingerstick channel.
        base: base_entry(record, "smbg")?,
        type_: Some("mbg".to_string()),
        sgv: f64_to_decimal(sgv),
        direction: None,
        noise: None,
        filtered: None,
        unfiltered: None,
        rssi: None,
        units: Some("mg/dl".to_string()),
    })
}

// Bolus.
//
// Tidepool bolus subTypes:
//   - "normal":    { normal, expectedNormal? }
//   - "square":    { extended, duration, expectedExtended?, expectedDuration? }
//   - "dual/square": { normal, extended, duration, ... }
// The "expected" fields are present only when the user interrupted the
// dose before completion; the actual delivered amount is the base field.

fn bolus_to_treatment(record: &Value) -> Result<Treatment> {
    let normal = record.get("normal").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let extended = record
        .get("extended")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let duration_ms = record.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
    let total = normal + extended;

    let (event_type, split_now, split_ext, duration_min) = if extended > 0.0 {
        let ratio_now = if total > 0.0 {
            (normal / total * 100.0).round()
        } else {
            0.0
        };
        (
            "Combo Bolus",
            Some(ratio_now),
            Some(100.0 - ratio_now),
            Some((duration_ms as f64 / 60_000.0).round()),
        )
    } else {
        ("Correction Bolus", None, None, None)
    };

    Ok(Treatment {
        base: base_shared(record, "bolus")?,
        event_type: Some(event_type.to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: f64_to_decimal(total),
        duration: duration_min.and_then(f64_to_decimal),
        pre_bolus: None,
        split_now: split_now.and_then(f64_to_decimal),
        split_ext: split_ext.and_then(f64_to_decimal),
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: None,
        notes: None,
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Basal.
//
// Tidepool deliveryType values include scheduled / temp / automated /
// suspend. All map to NS Temp Basal except "suspend" which becomes
// Suspend Pump.

fn basal_to_treatment(record: &Value) -> Result<Treatment> {
    let delivery = record
        .get("deliveryType")
        .and_then(|v| v.as_str())
        .unwrap_or("scheduled");
    if delivery == "suspend" {
        return Ok(Treatment {
            base: base_shared(record, "suspend")?,
            event_type: Some("Suspend Pump".to_string()),
            glucose: None,
            glucose_type: None,
            units: None,
            carbs: None,
            protein: None,
            fat: None,
            insulin: None,
            duration: record
                .get("duration")
                .and_then(|v| v.as_i64())
                .map(|ms| (ms as f64 / 60_000.0).round())
                .and_then(f64_to_decimal),
            pre_bolus: None,
            split_now: None,
            split_ext: None,
            percent: None,
            absolute: None,
            target_top: None,
            target_bottom: None,
            profile: None,
            reason: None,
            notes: Some(format!("deliveryType={delivery}")),
            entered_by: Some(APP_NAME.to_string()),
        });
    }
    let rate = record.get("rate").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let duration_ms = record.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
    let duration_min = (duration_ms as f64 / 60_000.0).round();
    Ok(Treatment {
        // Keep deliveryType out of the identifier for cross-tool dedup.
        base: base_shared(record, "basal")?,
        event_type: Some("Temp Basal".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: f64_to_decimal(duration_min),
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: f64_to_decimal(rate),
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: Some(delivery.to_string()),
        notes: None,
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Food.

fn food_to_treatment(record: &Value) -> Result<Treatment> {
    let grams = record
        .get("nutrition")
        .and_then(|n| n.get("carbohydrate"))
        .and_then(|c| c.get("net"))
        .and_then(|v| v.as_f64())
        // Some older docs put it at the top level instead.
        .or_else(|| {
            record
                .get("carbohydrate")
                .and_then(|c| c.get("net"))
                .and_then(|v| v.as_f64())
        })
        .unwrap_or(0.0);
    let name = record
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(Treatment {
        base: base_shared(record, "meal")?,
        event_type: Some("Carb Correction".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: f64_to_decimal(grams),
        protein: None,
        fat: None,
        insulin: None,
        duration: None,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: None,
        notes: name,
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Device events.
//
// Mapped deviceEvent subtypes:
//   - prime: Site Change for cannula, Insulin Change for tubing
//   - reservoirChange: Insulin Change
//   - status: Suspend Pump or Resume Pump
//   - alarm: Announcement
//   - calibration: BG Check

fn device_event_dispatch(record: &Value) -> Result<Converted> {
    let sub = record
        .get("subType")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("deviceEvent missing `subType`"))?;
    match sub {
        "prime" => {
            let target = record
                .get("primeTarget")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Cannula prime is the site-change moment.
            if target.eq_ignore_ascii_case("cannula") {
                Ok(Converted::Treatment(Box::new(simple_note_treatment(
                    base_shared(record, "sitechange")?,
                    "Site Change",
                    Some(format!("primeTarget={target}")),
                ))))
            } else {
                Ok(Converted::Treatment(Box::new(simple_note_treatment(
                    base_entry(record, "prime")?,
                    "Insulin Change",
                    Some(format!("primeTarget={target}")),
                ))))
            }
        }
        "reservoirChange" => Ok(Converted::Treatment(Box::new(simple_note_treatment(
            base_entry(record, "reservoir")?,
            "Insulin Change",
            None,
        )))),
        "status" => {
            let status = record.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let (event_type, kind) = match status {
                "suspended" => ("Suspend Pump", "suspend"),
                "resumed" => ("Resume Pump", "resume"),
                _ => {
                    return Ok(Converted::Skipped(format!(
                        "deviceEvent status={status}: no NS mapping"
                    )));
                }
            };
            let reason = record
                .get("reason")
                .and_then(|r| r.get("suspended").or_else(|| r.get("resumed")))
                .and_then(|v| v.as_str())
                .map(|s| format!("reason={s}"));
            Ok(Converted::Treatment(Box::new(simple_note_treatment(
                base_shared(record, kind)?,
                event_type,
                reason,
            ))))
        }
        "alarm" => {
            let alarm_type = record
                .get("alarmType")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            Ok(Converted::Treatment(Box::new(simple_note_treatment(
                base_shared(record, "alarm")?,
                "Announcement",
                Some(format!("pump alarm: {alarm_type}")),
            ))))
        }
        "calibration" => {
            let value_mmol = record.get("value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let glucose_mgdl = mmol_to_mgdl_round(value_mmol);
            let mut tr =
                simple_note_treatment(base_entry(record, "calibration")?, "BG Check", None);
            tr.glucose = f64_to_decimal(glucose_mgdl);
            tr.glucose_type = Some("Finger".to_string());
            tr.units = Some("mg/dl".to_string());
            Ok(Converted::Treatment(Box::new(tr)))
        }
        other => Ok(Converted::Skipped(format!(
            "deviceEvent subType={other}: no NS mapping in v1"
        ))),
    }
}

fn simple_note_treatment(base: DocumentBase, event_type: &str, notes: Option<String>) -> Treatment {
    Treatment {
        base,
        event_type: Some(event_type.to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: None,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: None,
        notes,
        entered_by: Some(APP_NAME.to_string()),
    }
}

// Physical activity.

fn physical_activity_to_treatment(record: &Value) -> Result<Treatment> {
    let activity = record
        .get("activityType")
        .and_then(|v| v.as_str())
        .unwrap_or("exercise")
        .to_string();
    // Tidepool duration shape: {value, units}.
    let duration_min = record.get("duration").and_then(|d| {
        let v = d.get("value").and_then(|v| v.as_f64())?;
        let units = d.get("units").and_then(|u| u.as_str()).unwrap_or("minutes");
        Some(match units {
            "hours" => v * 60.0,
            "seconds" => v / 60.0,
            _ => v,
        })
    });
    Ok(Treatment {
        // Twiist has no exercise log.
        base: base_entry(record, "exercise")?,
        event_type: Some("Exercise".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: duration_min.and_then(f64_to_decimal),
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: None,
        notes: Some(activity),
        entered_by: Some(APP_NAME.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 2026-04-21T12:00:00Z as Unix epoch seconds.
    const TS_EPOCH_S: i64 = 1_776_772_800;
    const TS_RFC3339: &str = "2026-04-21T12:00:00Z";

    #[test]
    fn cbg_uses_shared_cgm_identifier() {
        let record = json!({
            "type": "cbg",
            "id": "abc123",
            "time": TS_RFC3339,
            "value": 9.6,
            "units": "mmol/L",
            "deviceId": "cgm-xyz"
        });
        let Converted::Entry(e) = convert(&record).unwrap() else {
            panic!("expected Entry");
        };
        let e = *e;
        assert_eq!(e.type_.as_deref(), Some("sgv"));
        // 9.6 * 18.01559 = 172.95, rounded to 173.
        assert_eq!(e.sgv, Some(Decimal::from(173)));
        // Shared identifier scheme: `cgm-<epoch_s>`.
        assert_eq!(
            e.base.identifier.as_deref(),
            Some(format!("cgm-{TS_EPOCH_S}").as_str())
        );
        assert_eq!(e.base.device.as_deref(), Some("cgm-xyz"));
    }

    #[test]
    fn smbg_keeps_tp_prefix() {
        let record = json!({
            "type": "smbg",
            "id": "smbg1",
            "time": TS_RFC3339,
            "value": 5.5
        });
        let Converted::Entry(e) = convert(&record).unwrap() else {
            panic!("expected Entry");
        };
        let e = *e;
        // Tidepool-only kind.
        assert_eq!(e.base.identifier.as_deref(), Some("tp-smbg-smbg1"));
        assert_eq!(e.type_.as_deref(), Some("mbg"));
    }

    #[test]
    fn normal_bolus_yields_correction_bolus() {
        let record = json!({
            "type": "bolus",
            "subType": "normal",
            "id": "bolus1",
            "time": TS_RFC3339,
            "normal": 1.2
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Correction Bolus"));
        // 1.2 has float-noise tail; f64_to_decimal rounds to 4dp.
        assert_eq!(t.insulin, Some(Decimal::new(12, 1)));
        assert!(t.split_now.is_none());
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("bolus-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn combo_bolus_has_split_and_duration() {
        let record = json!({
            "type": "bolus",
            "subType": "dual/square",
            "id": "bolus2",
            "time": TS_RFC3339,
            "normal": 0.4,
            "extended": 0.6,
            "duration": 1_800_000
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Combo Bolus"));
        assert_eq!(t.insulin, Some(Decimal::from(1)));
        // 0.4 / 1.0 * 100 = 40.
        assert_eq!(t.split_now, Some(Decimal::from(40)));
        assert_eq!(t.split_ext, Some(Decimal::from(60)));
        // 1,800,000 ms / 60,000 = 30 min.
        assert_eq!(t.duration, Some(Decimal::from(30)));
    }

    #[test]
    fn scheduled_basal_is_shared_basal_kind() {
        let record = json!({
            "type": "basal",
            "deliveryType": "scheduled",
            "id": "basal1",
            "time": TS_RFC3339,
            "rate": 0.7,
            "duration": 300_000
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Temp Basal"));
        assert_eq!(t.absolute, Some(Decimal::new(7, 1)));
        assert_eq!(t.duration, Some(Decimal::from(5)));
        // `basal-<epoch_s>` regardless of deliveryType.
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("basal-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn suspend_basal_uses_shared_suspend_kind() {
        let record = json!({
            "type": "basal",
            "deliveryType": "suspend",
            "id": "s1",
            "time": TS_RFC3339,
            "duration": 600_000
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Suspend Pump"));
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("suspend-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn device_event_prime_cannula_is_sitechange() {
        let record = json!({
            "type": "deviceEvent",
            "subType": "prime",
            "primeTarget": "cannula",
            "id": "e1",
            "time": TS_RFC3339
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Site Change"));
        assert!(t.notes.as_deref().unwrap().contains("cannula"));
        // Shared with twiistsync's `sitechange-*` identifier.
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("sitechange-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn device_event_prime_tubing_stays_tp_prefixed() {
        let record = json!({
            "type": "deviceEvent",
            "subType": "prime",
            "primeTarget": "tubing",
            "id": "e2",
            "time": TS_RFC3339
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Insulin Change"));
        // Tubing prime has no twiistsync counterpart.
        assert_eq!(t.base.identifier.as_deref(), Some("tp-prime-e2"));
    }

    #[test]
    fn device_event_status_suspended_uses_shared_suspend_kind() {
        let record = json!({
            "type": "deviceEvent",
            "subType": "status",
            "status": "suspended",
            "id": "e3",
            "time": TS_RFC3339,
            "reason": {"suspended": "manual"}
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Suspend Pump"));
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("suspend-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn device_event_alarm_uses_shared_alarm_kind() {
        let record = json!({
            "type": "deviceEvent",
            "subType": "alarm",
            "alarmType": "low_insulin",
            "id": "e4",
            "time": TS_RFC3339
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Announcement"));
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("alarm-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn food_uses_shared_meal_kind() {
        let record = json!({
            "type": "food",
            "id": "f1",
            "time": TS_RFC3339,
            "nutrition": {"carbohydrate": {"net": 42.0}}
        });
        let Converted::Treatment(t) = convert(&record).unwrap() else {
            panic!("expected Treatment");
        };
        let t = *t;
        assert_eq!(t.event_type.as_deref(), Some("Carb Correction"));
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("meal-{TS_EPOCH_S}").as_str())
        );
    }

    #[test]
    fn wizard_is_skipped_for_now() {
        let record = json!({
            "type": "wizard",
            "id": "w1",
            "time": TS_RFC3339,
            "carbInput": 42,
            "recommended": {"net": 1.2}
        });
        match convert(&record).unwrap() {
            Converted::Skipped(reason) => assert!(reason.contains("wizard")),
            _ => panic!("expected wizard to be skipped"),
        }
    }

    #[test]
    fn unknown_type_does_not_error() {
        let record = json!({"type": "someNewType", "id": "x"});
        match convert(&record).unwrap() {
            Converted::UnknownType(t) => assert_eq!(t, "someNewType"),
            _ => panic!("expected UnknownType"),
        }
    }
}
