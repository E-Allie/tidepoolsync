//! Per-type watermark sidecar for incremental data sync.
//!
//! Stores the newest `time` timestamp seen for each Tidepool record type.
//! The next run starts from that timestamp plus 1 ms. Missing entries use
//! the CLI backfill window.
//!
//! Schema: `{"latest": {"<type>": "<rfc3339>", ...}}`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize, Clone)]
pub struct Watermark {
    #[serde(default)]
    pub latest: BTreeMap<String, DateTime<Utc>>,
}

impl Watermark {
    pub fn latest_for(&self, type_: &str) -> Option<DateTime<Utc>> {
        self.latest.get(type_).copied()
    }

    pub fn advance(&mut self, type_: &str, ts: DateTime<Utc>) {
        self.latest
            .entry(type_.to_string())
            .and_modify(|cur| {
                if ts > *cur {
                    *cur = ts;
                }
            })
            .or_insert(ts);
    }
}

pub fn load(path: &Path) -> Result<Watermark> {
    if !path.exists() {
        return Ok(Watermark::default());
    }
    let f = File::open(path).with_context(|| format!("opening watermark {}", path.display()))?;
    serde_json::from_reader(BufReader::new(f))
        .with_context(|| format!("parsing watermark {}", path.display()))
}

pub fn save(path: &Path, wm: &Watermark) -> Result<()> {
    let mut f =
        atomic_writer(path).with_context(|| format!("creating watermark {}", path.display()))?;
    serde_json::to_writer_pretty(&mut f, wm)
        .with_context(|| format!("writing watermark {}", path.display()))?;
    f.commit()
        .with_context(|| format!("committing watermark {}", path.display()))
}

fn atomic_writer(path: &Path) -> std::io::Result<atomic_write_file::AtomicWriteFile> {
    let mut options = atomic_write_file::OpenOptions::new();

    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        options.preserve_mode(false).mode(0o600);
    }

    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_only_moves_forward() {
        let mut wm = Watermark::default();
        let earlier = DateTime::parse_from_rfc3339("2026-04-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = DateTime::parse_from_rfc3339("2026-04-21T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        wm.advance("cbg", later);
        wm.advance("cbg", earlier);
        assert_eq!(wm.latest_for("cbg"), Some(later));
    }

    #[test]
    fn load_accepts_new_nested_shape() {
        let dir = tempdir_for_test();
        let path = dir.join("nested.json");
        let ts = "2026-04-21T12:00:00Z";
        std::fs::write(&path, format!(r#"{{"latest":{{"cbg":"{ts}"}}}}"#)).unwrap();
        let wm = load(&path).unwrap();
        assert_eq!(wm.latest.len(), 1);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir_for_test();
        let path = dir.join("roundtrip.json");
        let mut wm = Watermark::default();
        wm.advance(
            "cbg",
            DateTime::parse_from_rfc3339("2026-04-21T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        save(&path, &wm).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.latest, wm.latest);
    }

    fn tempdir_for_test() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tidepoolsync-watermark-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
