//! `tidepoolsync` CLI.
//!
//! Two modes:
//! - `--dump-settings [FILE]`: print the raw pumpSettings JSON.
//! - `--sync-profile [--dry-run]`: post the latest pumpSettings as a
//!   Nightscout profile.
//! - `--sync-data [--dry-run]`: post Tidepool data as Nightscout docs.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use reqwest::blocking::Client;
use rust_decimal::Decimal;

use tidepoolsync::convert::{ConvertOptions, offset_minutes_to_etc_gmt, pump_settings_to_profile};
use tidepoolsync::{APP_NAME, config, sync};

const DEFAULT_BACKFILL_DAYS: i64 = 7;
const DEFAULT_POLL_SECS: u64 = 1800;
const DEFAULT_WATERMARK_FILE: &str = "state.json";
const DEFAULT_CARBS_HR: i64 = 20;
const DEFAULT_DELAY: i64 = 20;

#[derive(Parser, Debug)]
#[command(
    name = "tidepoolsync",
    author,
    version,
    about = "Sync a Tidepool patient's pumpSettings and data history to Nightscout."
)]
struct Cli {
    #[arg(long, default_value = "tidepoolsync.config.json")]
    config: PathBuf,

    /// Print the raw pumpSettings record.
    #[arg(long, value_name = "FILE", num_args = 0..=1)]
    dump_settings: Option<Option<PathBuf>>,

    /// Post the latest pumpSettings as a Nightscout profile.
    #[arg(long)]
    sync_profile: bool,

    /// Sync Tidepool treatment history and CGM into Nightscout.
    #[arg(long)]
    sync_data: bool,

    /// Days to fetch when a type has no watermark yet.
    #[arg(long, default_value_t = DEFAULT_BACKFILL_DAYS)]
    backfill_days: i64,

    /// Path to the per-type watermark sidecar.
    ///
    /// Defaults to `$XDG_STATE_HOME/tidepoolsync/state.json`.
    #[arg(long)]
    watermark: Option<PathBuf>,

    /// Run `--sync-data` in a polling loop.
    #[arg(long)]
    daemon: bool,

    /// Daemon poll interval in seconds.
    #[arg(long, default_value_t = DEFAULT_POLL_SECS)]
    poll_interval_secs: u64,

    /// Skip Nightscout auth and print what would be sent.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    let any_action = cli.dump_settings.is_some() || cli.sync_profile || cli.sync_data;
    if !any_action {
        eprintln!(
            "no action requested; pass one of --dump-settings [FILE] / --sync-profile / --sync-data (add --dry-run to preview, --daemon to loop sync-data)"
        );
        return Ok(());
    }

    let http = Client::new();
    eprintln!("logging into Tidepool at {}", cfg.tidepool.base_url());
    let tp = tidepool::TidepoolClient::login(
        cfg.tidepool.base_url(),
        &cfg.tidepool.username,
        &cfg.tidepool.password,
        http.clone(),
    )
    .context("Tidepool login failed")?;
    eprintln!(
        "logged in as {} (userid {})",
        cfg.tidepool.username, tp.user_id
    );

    // Build one authenticated client for all Nightscout POSTs.
    let ns_needed = !cli.dry_run && (cli.sync_profile || cli.sync_data);
    let ns = if ns_needed {
        let ns = nightscout::NightscoutClient::new(
            cfg.nightscout.website.clone(),
            cfg.nightscout.permission_role.clone(),
        )
        .with_http(http.clone());
        ns.authenticate()
            .context("failed to obtain Nightscout bearer token")?;
        Some(ns)
    } else {
        None
    };

    if let Some(dump) = cli.dump_settings.as_ref() {
        let raw = tp
            .get_latest_pump_settings_raw(&cfg.tidepool.patient_uuid)
            .context("fetching pumpSettings")?;
        let pretty = serde_json::to_string_pretty(&raw)?;
        match dump {
            None => println!("{pretty}"),
            Some(path) => {
                std::fs::write(path, &pretty)
                    .with_context(|| format!("writing pumpSettings to {}", path.display()))?;
                eprintln!("wrote {} bytes to {}", pretty.len(), path.display());
            }
        }
        return Ok(());
    }

    if cli.sync_profile {
        do_sync_profile(&tp, &cfg, &ns, cli.dry_run).context("--sync-profile")?;
        if !cli.sync_data {
            return Ok(());
        }
    }

    if cli.sync_data {
        if cli.poll_interval_secs == 0 {
            bail!("--poll-interval-secs must be greater than zero");
        }
        let watermark = cli
            .watermark
            .clone()
            .map(Ok)
            .or_else(|| {
                cfg.sync
                    .as_ref()
                    .and_then(|sync| sync.watermark_path.clone())
                    .map(Ok)
            })
            .unwrap_or_else(default_watermark_path)?;
        let opts = sync::SyncOptions {
            backfill_days: cli.backfill_days,
            dry_run: cli.dry_run,
            watermark_path: &watermark,
        };
        if cli.daemon {
            run_daemon(
                &tp,
                &cfg.tidepool.patient_uuid,
                ns.as_ref(),
                &opts,
                cli.poll_interval_secs,
            )?;
        } else {
            let stats = sync::sync_once(&tp, &cfg.tidepool.patient_uuid, ns.as_ref(), &opts)
                .context("--sync-data")?;
            print_stats(&stats);
        }
        return Ok(());
    }

    bail!("unreachable: no action matched")
}

fn default_watermark_path() -> Result<PathBuf> {
    let dir = default_state_dir("tidepoolsync")?;
    Ok(dir.join(DEFAULT_WATERMARK_FILE))
}

fn default_state_dir(app: &str) -> Result<PathBuf> {
    let state_home = match std::env::var_os("XDG_STATE_HOME") {
        Some(raw) if !raw.is_empty() => {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                fallback_state_home()?
            }
        }
        _ => fallback_state_home()?,
    };
    let dir = state_home.join(app);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating state directory {}", dir.display()))?;
    Ok(dir)
}

fn fallback_state_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set and XDG_STATE_HOME is unavailable"))?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

fn do_sync_profile(
    tp: &tidepool::TidepoolClient,
    cfg: &config::Config,
    ns: &Option<nightscout::NightscoutClient>,
    dry_run: bool,
) -> Result<()> {
    let ps = tp
        .get_latest_pump_settings(&cfg.tidepool.patient_uuid)
        .context("fetching pumpSettings")?
        .ok_or_else(|| anyhow::anyhow!("no pumpSettings record returned"))?;

    let units = cfg
        .nightscout
        .glucose_unit
        .clone()
        .unwrap_or_else(|| "mg/dl".to_string());
    let timezone = cfg
        .nightscout
        .timezone
        .clone()
        .unwrap_or_else(|| offset_minutes_to_etc_gmt(ps.schedule_time_zone_offset));
    let carbs_hr = cfg
        .nightscout
        .carbs_hr
        .unwrap_or_else(|| Decimal::from(DEFAULT_CARBS_HR));
    let delay = cfg
        .nightscout
        .delay
        .unwrap_or_else(|| Decimal::from(DEFAULT_DELAY));
    let opts = ConvertOptions {
        units,
        timezone,
        app: APP_NAME.to_string(),
        carbs_hr,
        delay,
    };
    let profile = pump_settings_to_profile(&ps, &opts);

    if dry_run {
        println!("{}", serde_json::to_string_pretty(&profile)?);
        eprintln!(
            "(dry-run) would POST profile {} ({} store(s), defaultProfile={:?})",
            profile.base.identifier.as_deref().unwrap_or("?"),
            profile.store.len(),
            profile.default_profile,
        );
        return Ok(());
    }

    let ns = ns.as_ref().expect("ns is Some when dry_run is false");
    ns.post_document("profile", &profile)
        .context("POST /api/v3/profile failed")?;
    eprintln!(
        "posted profile {} ({} store(s), defaultProfile={:?})",
        profile.base.identifier.as_deref().unwrap_or("?"),
        profile.store.len(),
        profile.default_profile,
    );
    Ok(())
}

fn run_daemon(
    tp: &tidepool::TidepoolClient,
    patient_uid: &str,
    ns: Option<&nightscout::NightscoutClient>,
    opts: &sync::SyncOptions<'_>,
    poll_secs: u64,
) -> Result<()> {
    let interval = Duration::from_secs(poll_secs);
    eprintln!(
        "entering daemon loop, tick every {poll_secs}s (~{:.1} min)",
        poll_secs as f64 / 60.0
    );
    loop {
        let tick_start = Instant::now();
        match sync::sync_once(tp, patient_uid, ns, opts) {
            Ok(stats) => print_stats(&stats),
            Err(e) => eprintln!("sync tick failed (will retry next interval): {e:#}"),
        }
        // Stable cadence: subtract elapsed from interval. If a tick
        // took longer than the interval, fire the next one immediately.
        let next = tick_start + interval;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
}

fn print_stats(s: &sync::SyncStats) {
    eprintln!(
        "sync: fetched={} entries={}/{} treatments={}/{} skipped={} bolus_dedup_skipped={} convert_errors={}",
        s.fetched,
        s.entries_ok,
        s.entries_ok + s.entries_fail,
        s.treatments_ok,
        s.treatments_ok + s.treatments_fail,
        s.skipped,
        s.bolus_dedup_skipped,
        s.convert_errors,
    );
    if !s.unknown_types.is_empty() {
        let mut kinds: Vec<String> = s
            .unknown_types
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        kinds.sort();
        eprintln!("  unknown types: {}", kinds.join(", "));
    }
}
