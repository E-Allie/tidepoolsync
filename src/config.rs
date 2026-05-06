//! `tidepoolsync.config.json` loader.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use secrecy::SecretString;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub tidepool: TidepoolSection,
    pub nightscout: NightscoutSection,
    pub sync: Option<SyncSection>,
}

#[derive(Deserialize, Debug)]
pub struct TidepoolSection {
    pub username: String,
    pub password: SecretString,
    pub base_url: Option<String>,
    /// Patient userid from `https://app.tidepool.org/patients/<UUID>/data`.
    pub patient_uuid: String,
}

impl TidepoolSection {
    pub fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or(tidepool::client::DEFAULT_BASE_URL)
    }
}

#[derive(Deserialize, Debug)]
pub struct NightscoutSection {
    pub website: String,
    pub permission_role: String,
    /// IANA zone for profile stores, for example `"America/New_York"`.
    pub timezone: Option<String>,
    /// `"mg/dl"` (default) or `"mmol"`
    pub glucose_unit: Option<String>,
    /// Carbs absorbed per hour, profile default. NS recommends 20.
    #[serde(default)]
    pub carbs_hr: Option<Decimal>,
    /// Carb absorption delay in minutes, profile default. NS recommends 20.
    #[serde(default)]
    pub delay: Option<Decimal>,
}

#[derive(Deserialize, Debug, Default)]
pub struct SyncSection {
    /// Optional custom path for the watermark sidecar.
    pub watermark_path: Option<PathBuf>,
}

pub fn load(path: &Path) -> Result<Config> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let c: Config = serde_json::from_reader(BufReader::new(f))
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(c)
}
