//! hwmon chip resolution.
//!
//! Walks `/sys/class/hwmon/*/name` to build a `chip_name -> hwmon_path` map
//! and resolves each `[fan:*]` config block to a concrete hwmon directory.
//! The `scan_at(path)` form is the public testable API — production calls
//! `scan()` which is a thin wrapper around it (PLAN.md L757).

use crate::config::FanConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

const HWMON_ROOT: &str = "/sys/class/hwmon";

#[derive(Debug, Error)]
pub enum ChipError {
    #[error("failed to read hwmon root {root:?}: {source}")]
    ReadDir {
        root: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read {path:?}/name: {source}")]
    ReadName {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("chip {chip:?} not found for fan {fan_id:?}; available chips: {available:?}")]
    ChipNotFound {
        fan_id: String,
        chip: String,
        available: Vec<String>,
    },
    #[error("fan {fan_id:?}: at least one of chip/device_path/hwmon_path must be set")]
    NoSelector { fan_id: String },
    #[error("fan {fan_id:?}: hwmon_path {path:?} does not exist")]
    HwmonPathMissing { fan_id: String, path: PathBuf },
}

/// Scan `/sys/class/hwmon` for chips. Production entry point.
pub fn scan() -> Result<HashMap<String, PathBuf>, ChipError> {
    scan_at(Path::new(HWMON_ROOT))
}

/// Scan an arbitrary hwmon-style root for chips. Public (not test-gated)
/// because tempfile-backed tests share this surface with production
/// (PLAN.md L757).
///
/// For every entry in `root` that is a directory and contains a readable
/// `name` file, inserts `(name.trim(), entry_path)` into the returned map.
/// Entries without a `name` file are skipped silently — some hwmon entries
/// (e.g. virtual sensors) genuinely lack one.
pub fn scan_at(root: &Path) -> Result<HashMap<String, PathBuf>, ChipError> {
    let mut map = HashMap::new();
    let entries = std::fs::read_dir(root).map_err(|source| ChipError::ReadDir {
        root: root.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ChipError::ReadDir {
            root: root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name_path = path.join("name");
        let raw = match std::fs::read_to_string(&name_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ChipError::ReadName { path, source }),
        };
        let name = raw.trim().to_string();
        if !name.is_empty() {
            map.insert(name, path);
        }
    }
    Ok(map)
}

/// Resolve a fan's config block to a concrete hwmon directory.
///
/// Precedence:
/// 1. `hwmon_path` set → escape hatch (PLAN.md L151); the path must exist on disk.
/// 2. Else `chip` set → look up in `map`.
/// 3. Else (`hwmon_path` and `chip` both `None`) → `NoSelector`.
///
/// On a chip-name miss the error lists every available chip name — this is
/// load-bearing for operator triage (PLAN.md L270 R1).
///
/// `device_path` disambiguation is intentionally deferred. Most real configs
/// use `chip` alone, and the `hwmon_path` escape hatch already covers the
/// duplicate-chip case by letting the operator pin an exact `hwmonN` path.
pub fn resolve(map: &HashMap<String, PathBuf>, fan_cfg: &FanConfig) -> Result<PathBuf, ChipError> {
    if let Some(p) = &fan_cfg.hwmon_path {
        if !p.exists() {
            return Err(ChipError::HwmonPathMissing {
                fan_id: fan_cfg.id.clone(),
                path: p.clone(),
            });
        }
        return Ok(p.clone());
    }
    if let Some(chip) = &fan_cfg.chip {
        return map.get(chip).cloned().ok_or_else(|| {
            let mut available: Vec<String> = map.keys().cloned().collect();
            available.sort();
            ChipError::ChipNotFound {
                fan_id: fan_cfg.id.clone(),
                chip: chip.clone(),
                available,
            }
        });
    }
    Err(ChipError::NoSelector {
        fan_id: fan_cfg.id.clone(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_hwmon(dir: &Path, idx: u32, name: &str) -> PathBuf {
        let p = dir.join(format!("hwmon{idx}"));
        fs::create_dir(&p).unwrap();
        fs::write(p.join("name"), format!("{name}\n")).unwrap();
        p
    }

    fn fan(id: &str, chip: Option<&str>, hwmon_path: Option<PathBuf>) -> FanConfig {
        FanConfig {
            id: id.to_string(),
            chip: chip.map(str::to_string),
            device_path: None,
            hwmon_path,
            pwm_channel: 1,
            min_rpm: 200,
            max_rpm: 3000,
            fan_fail_threshold: 3,
            spin_up_grace_s: 10,
        }
    }

    #[test]
    fn scan_at_picks_up_chip_names() {
        let tmp = tempdir().unwrap();
        let p0 = make_hwmon(tmp.path(), 0, "nct6798");
        let p1 = make_hwmon(tmp.path(), 1, "coretemp");
        let map = scan_at(tmp.path()).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("nct6798"), Some(&p0));
        assert_eq!(map.get("coretemp"), Some(&p1));
    }

    #[test]
    fn scan_at_skips_dirs_without_name_file() {
        let tmp = tempdir().unwrap();
        // Real hwmon entry.
        make_hwmon(tmp.path(), 0, "nct6798");
        // Bogus dir with no `name` — scan should silently skip it.
        fs::create_dir(tmp.path().join("hwmon1")).unwrap();
        let map = scan_at(tmp.path()).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("nct6798"));
    }

    #[test]
    fn scan_at_missing_root_errors() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = scan_at(&missing).unwrap_err();
        assert!(matches!(err, ChipError::ReadDir { .. }));
    }

    #[test]
    fn resolve_by_chip_name_returns_path() {
        let tmp = tempdir().unwrap();
        let p0 = make_hwmon(tmp.path(), 0, "nct6798");
        let map = scan_at(tmp.path()).unwrap();
        let cfg = fan("f0", Some("nct6798"), None);
        assert_eq!(resolve(&map, &cfg).unwrap(), p0);
    }

    #[test]
    fn resolve_chip_not_found_lists_available() {
        let tmp = tempdir().unwrap();
        make_hwmon(tmp.path(), 0, "nct6798");
        make_hwmon(tmp.path(), 1, "coretemp");
        let map = scan_at(tmp.path()).unwrap();
        let cfg = fan("f0", Some("it8772"), None);
        match resolve(&map, &cfg).unwrap_err() {
            ChipError::ChipNotFound {
                fan_id,
                chip,
                available,
            } => {
                assert_eq!(fan_id, "f0");
                assert_eq!(chip, "it8772");
                assert!(available.contains(&"nct6798".to_string()));
                assert!(available.contains(&"coretemp".to_string()));
            }
            other => panic!("expected ChipNotFound, got {other:?}"),
        }
    }

    #[test]
    fn resolve_hwmon_path_escape_hatch_wins() {
        let tmp = tempdir().unwrap();
        // Map contains a chip — it should be ignored when hwmon_path is set.
        let _p0 = make_hwmon(tmp.path(), 0, "nct6798");
        let pinned = make_hwmon(tmp.path(), 1, "nct6798"); // duplicate chip name
        let map = scan_at(tmp.path()).unwrap();
        let cfg = fan("f0", Some("nct6798"), Some(pinned.clone()));
        assert_eq!(resolve(&map, &cfg).unwrap(), pinned);
    }

    #[test]
    fn resolve_hwmon_path_missing_errors() {
        let tmp = tempdir().unwrap();
        let map = scan_at(tmp.path()).unwrap();
        let bogus = tmp.path().join("nonexistent-hwmon");
        let cfg = fan("f0", None, Some(bogus.clone()));
        match resolve(&map, &cfg).unwrap_err() {
            ChipError::HwmonPathMissing { fan_id, path } => {
                assert_eq!(fan_id, "f0");
                assert_eq!(path, bogus);
            }
            other => panic!("expected HwmonPathMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolve_no_selector_errors() {
        let map: HashMap<String, PathBuf> = HashMap::new();
        let cfg = fan("f0", None, None);
        match resolve(&map, &cfg).unwrap_err() {
            ChipError::NoSelector { fan_id } => assert_eq!(fan_id, "f0"),
            other => panic!("expected NoSelector, got {other:?}"),
        }
    }
}
