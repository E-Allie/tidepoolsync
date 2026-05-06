//! Tidepool `pumpSettings` to Nightscout `Profile` conversion.
//!
//! Tidepool stores glucose schedule values in mmol/L. This module
//! converts them to the configured Nightscout profile units.
//!
//! Schedule names come from Tidepool's named schedule maps.

use std::collections::BTreeMap;

use nightscout::{DocumentBase, Profile, ProfileStore, TimeValue};
use rust_decimal::Decimal;
use tidepool::{
    BasalRateStart, BloodGlucoseTargetStart, CarbohydrateRatioStart, InsulinSensitivityStart,
    MMOL_TO_MGDL, PumpSettings,
};

pub struct ConvertOptions {
    pub units: String,
    pub timezone: String,
    pub app: String,
    /// Carbs absorbed per hour. Nightscout default 20 g/h.
    pub carbs_hr: Decimal,
    /// Carb absorption delay in minutes. Nightscout default 20.
    pub delay: Decimal,
}

pub fn pump_settings_to_profile(ps: &PumpSettings, opts: &ConvertOptions) -> Profile {
    let mgdl = opts.units.eq_ignore_ascii_case("mg/dl");
    let bg_mul = if mgdl { MMOL_TO_MGDL } else { 1.0 };
    // Round converted mg/dL values back to whole numbers.
    let round_bg = |v: f64| -> f64 {
        if mgdl {
            v.round()
        } else {
            (v * 10.0).round() / 10.0
        }
    };

    let to_bg = |x: f64| round_bg(x * bg_mul);
    let mut store: BTreeMap<String, ProfileStore> = BTreeMap::new();
    for (name, slots) in &ps.basal_schedules {
        let sens = ps
            .insulin_sensitivities
            .get(name)
            .map(|v| convert_sens(v, &to_bg))
            .unwrap_or_default();
        let carbratio = ps
            .carb_ratios
            .get(name)
            .map(|v| convert_carb(v))
            .unwrap_or_default();
        let (target_low, target_high) = ps
            .bg_targets
            .get(name)
            .map(|v| split_targets(v, &to_bg))
            .unwrap_or_default();
        store.insert(
            name.clone(),
            make_store(
                convert_basal(slots),
                carbratio,
                sens,
                target_low,
                target_high,
                ps,
                opts,
            ),
        );
    }

    // Legacy fallback for singular schedule fields.
    if store.is_empty()
        && let Some(basal) = &ps.basal_schedule
    {
        let sens = ps
            .insulin_sensitivity
            .as_ref()
            .map(|v| convert_sens(v, &to_bg))
            .unwrap_or_default();
        let carbratio = ps
            .carb_ratio
            .as_ref()
            .map(|v| convert_carb(v))
            .unwrap_or_default();
        let (target_low, target_high) = ps
            .bg_target
            .as_ref()
            .map(|v| split_targets(v, &to_bg))
            .unwrap_or_default();
        store.insert(
            ps.active_schedule.clone(),
            make_store(
                convert_basal(basal),
                carbratio,
                sens,
                target_low,
                target_high,
                ps,
                opts,
            ),
        );
    }

    let date_ms = ps.time.timestamp_millis();
    let device = match (&ps.manufacturers, &ps.model) {
        (Some(m), Some(model)) if !m.is_empty() => Some(format!("{}/{}", m[0], model)),
        (_, Some(model)) => Some(model.clone()),
        _ => None,
    };

    Profile {
        base: DocumentBase {
            identifier: Some(format!("tp-pumpSettings-{}", ps.id)),
            date: date_ms,
            utc_offset: ps.schedule_time_zone_offset.or(ps.timezone_offset),
            app: opts.app.clone(),
            device,
            id_internal: None,
            srv_created: None,
            subject: None,
            srv_modified: None,
            modified_by: None,
            is_valid: None,
            is_read_only: None,
        },
        default_profile: ps.active_schedule.clone(),
        store,
        start_date: Some(ps.time.to_rfc3339()),
        mills: Some(date_ms),
        units: Some(opts.units.clone()),
    }
}

fn make_store(
    basal: Vec<TimeValue>,
    carbratio: Vec<TimeValue>,
    sens: Vec<TimeValue>,
    target_low: Vec<TimeValue>,
    target_high: Vec<TimeValue>,
    ps: &PumpSettings,
    opts: &ConvertOptions,
) -> ProfileStore {
    ProfileStore {
        dia: ps
            .insulin_model
            .as_ref()
            .and_then(|m| m.dia_hours())
            .and_then(round_to_decimal),
        carbs_hr: Some(opts.carbs_hr),
        delay: Some(opts.delay),
        timezone: Some(opts.timezone.clone()),
        basal,
        carbratio,
        sens,
        target_low,
        target_high,
        units: Some(opts.units.clone()),
    }
}

fn convert_basal(slots: &[BasalRateStart]) -> Vec<TimeValue> {
    slots.iter().map(|s| tv(s.start, s.rate)).collect()
}

fn convert_sens(slots: &[InsulinSensitivityStart], to_bg: &dyn Fn(f64) -> f64) -> Vec<TimeValue> {
    slots.iter().map(|s| tv(s.start, to_bg(s.amount))).collect()
}

fn convert_carb(slots: &[CarbohydrateRatioStart]) -> Vec<TimeValue> {
    slots.iter().map(|s| tv(s.start, s.amount)).collect()
}

fn split_targets(
    slots: &[BloodGlucoseTargetStart],
    to_bg: &dyn Fn(f64) -> f64,
) -> (Vec<TimeValue>, Vec<TimeValue>) {
    let mut lo = Vec::with_capacity(slots.len());
    let mut hi = Vec::with_capacity(slots.len());
    for s in slots {
        if let Some((l, h)) = s.low_high() {
            lo.push(tv(s.start, to_bg(l)));
            hi.push(tv(s.start, to_bg(h)));
        }
    }
    (lo, hi)
}

fn tv(ms_since_midnight: i64, value: f64) -> TimeValue {
    let sec = ms_since_midnight / 1000;
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    TimeValue {
        time: format!("{h:02}:{m:02}"),
        time_as_seconds: sec,
        value: round_to_decimal(value).unwrap_or_default(),
    }
}

/// Round an f64 to 4 decimal places and return it as Decimal.
fn round_to_decimal(v: f64) -> Option<Decimal> {
    Decimal::from_f64_retain(v).map(|d| d.round_dp(4))
}

/// Convert a UTC offset in minutes into a fixed-offset `Etc/GMT` zone.
pub fn offset_minutes_to_etc_gmt(offset_minutes: Option<i32>) -> String {
    match offset_minutes {
        None | Some(0) => "UTC".to_string(),
        Some(m) if m % 60 != 0 => {
            eprintln!(
                "warning: Tidepool offset {m} min is not a whole hour; falling back to UTC. Set nightscout.timezone in config to override."
            );
            "UTC".to_string()
        }
        Some(m) => {
            let h = m / 60;
            format!("Etc/GMT{:+}", -h)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv_formats_hh_mm() {
        let t = tv(0, 0.5);
        assert_eq!(t.time, "00:00");
        assert_eq!(t.time_as_seconds, 0);
        let t = tv(7_200_000, 0.6); // 2h
        assert_eq!(t.time, "02:00");
        assert_eq!(t.time_as_seconds, 7200);
        let t = tv(45_900_000, 0.7); // 12:45
        assert_eq!(t.time, "12:45");
        assert_eq!(t.time_as_seconds, 45_900);
    }

    #[test]
    fn etc_gmt_uses_posix_sign() {
        assert_eq!(offset_minutes_to_etc_gmt(None), "UTC");
        assert_eq!(offset_minutes_to_etc_gmt(Some(0)), "UTC");
        assert_eq!(offset_minutes_to_etc_gmt(Some(-240)), "Etc/GMT+4");
        assert_eq!(offset_minutes_to_etc_gmt(Some(-300)), "Etc/GMT+5");
        assert_eq!(offset_minutes_to_etc_gmt(Some(60)), "Etc/GMT-1");
    }

    #[test]
    fn etc_gmt_falls_back_to_utc_for_sub_hour_offsets() {
        // India: UTC+5:30 = 330 minutes.
        assert_eq!(offset_minutes_to_etc_gmt(Some(330)), "UTC");
        // Nepal: UTC+5:45 = 345 minutes.
        assert_eq!(offset_minutes_to_etc_gmt(Some(345)), "UTC");
        // Newfoundland: UTC-3:30 = -210 minutes.
        assert_eq!(offset_minutes_to_etc_gmt(Some(-210)), "UTC");
    }

    /// Real 100/115 mg/dL targets round back to exact integers.
    #[test]
    fn target_split_rounds_mgdl_to_whole_numbers() {
        let slots = vec![BloodGlucoseTargetStart {
            start: 0,
            low: Some(5.55075),
            high: Some(6.38336),
            target: None,
            range: None,
        }];
        let to_bg_mgdl = |v: f64| (v * MMOL_TO_MGDL).round();
        let (lo, hi) = split_targets(&slots, &to_bg_mgdl);
        assert_eq!(lo[0].value, Decimal::from(100));
        assert_eq!(hi[0].value, Decimal::from(115));
    }

    #[test]
    fn target_split_rounds_mmol_to_one_decimal() {
        let slots = vec![BloodGlucoseTargetStart {
            start: 0,
            low: None,
            high: None,
            target: Some(5.52),
            range: Some(0.33),
        }];
        let to_bg_mmol = |v: f64| (v * 10.0).round() / 10.0;
        let (lo, hi) = split_targets(&slots, &to_bg_mmol);
        // 5.52 - 0.33 = 5.19 rounds to 5.2; 5.52 + 0.33 = 5.85 rounds to 5.9.
        assert_eq!(lo[0].value, Decimal::new(52, 1));
        assert_eq!(hi[0].value, Decimal::new(59, 1));
    }
}
