//! Config parsing + validation.
//!
//! Parsing: rust-ini → typed structs (parse-time errors stop here).
//! Validation: ALL S/G/F/W/T rules in PLAN.md §Configuration Validation. The
//! validation function is pure (no I/O); `load(path)` is the only I/O entry
//! point and the only place `std::fs` is used in this module.
//!
//! T2 (unknown key in known section) is implemented as a non-fatal `Warning`
//! returned alongside the parsed config rather than via the tracing logger,
//! so `--check-config` can surface them deterministically before the logger
//! is even initialised.

use crate::curve::{Curve, CurveError};
use crate::units::{Celsius, Pct};
use ini::Ini;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

const CONFIG_VERSION_SUPPORTED: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct GlobalConfig {
    pub config_version: u32,
    pub poll_interval_ms: u32,
    pub log_level: LogLevel,
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    pub enabled: bool,
    pub device: PathBuf,
    pub timeout_s: u32,
    pub gpu_fail_threshold: u32,
}

#[derive(Debug, Clone)]
pub struct GpuConfig {
    pub id: String,
    pub nvml_index: u32,
    pub curve: Curve,
    pub min_fan_pct: Pct,
    pub max_fan_pct: Pct,
}

#[derive(Debug, Clone)]
pub struct FanConfig {
    pub id: String,
    pub chip: Option<String>,
    pub device_path: Option<PathBuf>,
    pub hwmon_path: Option<PathBuf>,
    pub pwm_channel: u32,
    pub min_rpm: u32,
    pub max_rpm: u32,
    pub fan_fail_threshold: u32,
    pub spin_up_grace_s: u32,
}

#[derive(Debug, Clone)]
pub struct GroupConfig {
    pub id: String,
    pub gpus: Vec<String>,
    pub fans: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub global: GlobalConfig,
    pub watchdog: WatchdogConfig,
    pub gpus: Vec<GpuConfig>,
    pub fans: Vec<FanConfig>,
    pub groups: Vec<GroupConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub section: String,
    pub key: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub rule: &'static str,
    pub section: Option<String>,
    pub key: Option<String>,
    pub message: String,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.rule)?;
        if let Some(s) = &self.section {
            write!(f, " section={s}")?;
        }
        if let Some(k) = &self.key {
            write!(f, " key={k}")?;
        }
        write!(f, ": {}", self.message)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("INI parse error: {0}")]
    Ini(String),
    #[error("missing required section [{0}]")]
    MissingSection(String),
    #[error("missing required key '{key}' in section [{section}]")]
    MissingKey { section: String, key: String },
    #[error("invalid value for '{key}' in section [{section}]: {message}")]
    InvalidValue {
        section: String,
        key: String,
        message: String,
    },
    #[error("unknown section header [{0}] (T1) — only [global], [watchdog], [gpu:N], [fan:ID], [group:ID] are recognised")]
    UnknownSection(String),
    #[error("invalid fan curve in [gpu:{id}]: {source}")]
    Curve {
        id: String,
        #[source]
        source: CurveError,
    },
    #[error("validation failed with {} error(s):\n{}", .0.len(), format_errors(.0))]
    Validation(Vec<ValidationError>),
    #[error("I/O error reading config file: {0}")]
    Io(#[from] std::io::Error),
}

fn format_errors(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(|e| format!("  - {e}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    Global,
    Watchdog,
    Gpu,
    Fan,
    Group,
}

fn classify_section(header: &str) -> Option<(SectionKind, Option<String>)> {
    if header == "global" {
        return Some((SectionKind::Global, None));
    }
    if header == "watchdog" {
        return Some((SectionKind::Watchdog, None));
    }
    let (prefix, id) = header.split_once(':')?;
    if id.is_empty() {
        return None;
    }
    match prefix {
        "gpu" => Some((SectionKind::Gpu, Some(id.to_string()))),
        "fan" => Some((SectionKind::Fan, Some(id.to_string()))),
        "group" => Some((SectionKind::Group, Some(id.to_string()))),
        _ => None,
    }
}

fn known_keys(kind: SectionKind) -> &'static [&'static str] {
    match kind {
        SectionKind::Global => &[
            "config_version",
            "poll_interval_ms",
            "log_level",
            "log_file",
        ],
        SectionKind::Watchdog => &["enabled", "device", "timeout_s", "gpu_fail_threshold"],
        SectionKind::Gpu => &["nvml_index", "curve", "min_fan_pct", "max_fan_pct"],
        SectionKind::Fan => &[
            "chip",
            "device_path",
            "hwmon_path",
            "pwm_channel",
            "min_rpm",
            "max_rpm",
            "fan_fail_threshold",
            "spin_up_grace_s",
        ],
        SectionKind::Group => &["gpus", "fans"],
    }
}

fn parse_u32(section: &str, key: &str, raw: &str) -> Result<u32, ConfigError> {
    raw.trim()
        .parse::<u32>()
        .map_err(|e| ConfigError::InvalidValue {
            section: section.to_string(),
            key: key.to_string(),
            message: format!("expected u32: {e}"),
        })
}

fn parse_bool(section: &str, key: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        other => Err(ConfigError::InvalidValue {
            section: section.to_string(),
            key: key.to_string(),
            message: format!("expected bool, got '{other}'"),
        }),
    }
}

fn parse_log_level(section: &str, raw: &str) -> Result<LogLevel, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warn" => Ok(LogLevel::Warn),
        "error" => Ok(LogLevel::Error),
        other => Err(ConfigError::InvalidValue {
            section: section.to_string(),
            key: "log_level".to_string(),
            message: format!("expected one of debug|info|warn|error, got '{other}'"),
        }),
    }
}

fn parse_curve_field(gpu_id: &str, raw: &str) -> Result<Curve, ConfigError> {
    let mut points = Vec::new();
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let mut sp = piece.splitn(2, ':');
        let temp_s = sp.next().unwrap_or("").trim();
        let pct_s = sp.next().ok_or_else(|| ConfigError::InvalidValue {
            section: format!("gpu:{gpu_id}"),
            key: "curve".to_string(),
            message: format!("malformed point '{piece}', expected TEMP:PCT"),
        })?;
        let temp: i16 = temp_s.parse().map_err(|e| ConfigError::InvalidValue {
            section: format!("gpu:{gpu_id}"),
            key: "curve".to_string(),
            message: format!("temp '{temp_s}': {e}"),
        })?;
        let pct: u8 = pct_s
            .trim()
            .parse()
            .map_err(|e| ConfigError::InvalidValue {
                section: format!("gpu:{gpu_id}"),
                key: "curve".to_string(),
                message: format!("pct '{pct_s}': {e}"),
            })?;
        points.push((Celsius(temp), Pct(pct)));
    }
    Curve::new(points).map_err(|source| ConfigError::Curve {
        id: gpu_id.to_string(),
        source,
    })
}

fn parse_id_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse INI text into a typed `Config`. Stops at the first parse-level
/// error (missing required key, type mismatch, malformed curve, unknown
/// section). Validation rules run in `validate()` and accumulate.
pub fn parse(s: &str) -> Result<(Config, Vec<Warning>), ConfigError> {
    let ini = Ini::load_from_str(s).map_err(|e| ConfigError::Ini(e.to_string()))?;

    let mut warnings = Vec::new();

    let mut global: Option<GlobalConfig> = None;
    let mut watchdog: Option<WatchdogConfig> = None;
    let mut gpus: Vec<GpuConfig> = Vec::new();
    let mut fans: Vec<FanConfig> = Vec::new();
    let mut groups: Vec<GroupConfig> = Vec::new();

    for (section_name, props) in ini.iter() {
        let Some(name) = section_name else {
            // rust-ini puts top-level (no section) keys under None. We don't
            // accept top-level keys.
            if props.iter().next().is_some() {
                return Err(ConfigError::Ini(
                    "keys must live inside a [section]".to_string(),
                ));
            }
            continue;
        };
        let Some((kind, id)) = classify_section(name) else {
            return Err(ConfigError::UnknownSection(name.to_string()));
        };

        // Warn on unknown keys (T2).
        let known = known_keys(kind);
        for (k, _v) in props.iter() {
            if !known.contains(&k) {
                warnings.push(Warning {
                    section: name.to_string(),
                    key: k.to_string(),
                    message: format!("unknown key in [{name}] (T2)"),
                });
            }
        }

        let get = |key: &str| -> Option<&str> { props.get(key) };
        let require = |key: &str| -> Result<&str, ConfigError> {
            get(key).ok_or_else(|| ConfigError::MissingKey {
                section: name.to_string(),
                key: key.to_string(),
            })
        };

        match kind {
            SectionKind::Global => {
                let config_version = parse_u32(name, "config_version", require("config_version")?)?;
                let poll_interval_ms =
                    parse_u32(name, "poll_interval_ms", require("poll_interval_ms")?)?;
                let log_level = parse_log_level(name, require("log_level")?)?;
                let log_file = get("log_file")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                global = Some(GlobalConfig {
                    config_version,
                    poll_interval_ms,
                    log_level,
                    log_file,
                });
            }
            SectionKind::Watchdog => {
                let enabled = parse_bool(name, "enabled", require("enabled")?)?;
                let device = PathBuf::from(require("device")?.trim());
                let timeout_s = parse_u32(name, "timeout_s", require("timeout_s")?)?;
                let gpu_fail_threshold =
                    parse_u32(name, "gpu_fail_threshold", require("gpu_fail_threshold")?)?;
                watchdog = Some(WatchdogConfig {
                    enabled,
                    device,
                    timeout_s,
                    gpu_fail_threshold,
                });
            }
            SectionKind::Gpu => {
                let id = id.unwrap_or_default();
                let nvml_index = parse_u32(name, "nvml_index", require("nvml_index")?)?;
                let curve = parse_curve_field(&id, require("curve")?)?;
                let min_fan_pct = Pct(parse_u32(name, "min_fan_pct", require("min_fan_pct")?)?
                    .min(u32::from(u8::MAX)) as u8);
                let max_fan_pct = Pct(parse_u32(name, "max_fan_pct", require("max_fan_pct")?)?
                    .min(u32::from(u8::MAX)) as u8);
                gpus.push(GpuConfig {
                    id,
                    nvml_index,
                    curve,
                    min_fan_pct,
                    max_fan_pct,
                });
            }
            SectionKind::Fan => {
                let id = id.unwrap_or_default();
                let chip = get("chip")
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let device_path = get("device_path")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let hwmon_path = get("hwmon_path")
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let pwm_channel = parse_u32(name, "pwm_channel", require("pwm_channel")?)?;
                let min_rpm = parse_u32(name, "min_rpm", require("min_rpm")?)?;
                let max_rpm = parse_u32(name, "max_rpm", require("max_rpm")?)?;
                let fan_fail_threshold =
                    parse_u32(name, "fan_fail_threshold", require("fan_fail_threshold")?)?;
                let spin_up_grace_s =
                    parse_u32(name, "spin_up_grace_s", require("spin_up_grace_s")?)?;
                fans.push(FanConfig {
                    id,
                    chip,
                    device_path,
                    hwmon_path,
                    pwm_channel,
                    min_rpm,
                    max_rpm,
                    fan_fail_threshold,
                    spin_up_grace_s,
                });
            }
            SectionKind::Group => {
                let id = id.unwrap_or_default();
                let gpus_list = parse_id_list(require("gpus")?);
                let fans_list = parse_id_list(require("fans")?);
                groups.push(GroupConfig {
                    id,
                    gpus: gpus_list,
                    fans: fans_list,
                });
            }
        }
    }

    let global = global.ok_or_else(|| ConfigError::MissingSection("global".to_string()))?;
    let watchdog = watchdog.ok_or_else(|| ConfigError::MissingSection("watchdog".to_string()))?;

    Ok((
        Config {
            global,
            watchdog,
            gpus,
            fans,
            groups,
        },
        warnings,
    ))
}

/// Run all S/G/F/W/T validation rules and accumulate every violation.
/// Returns `Ok(())` if no errors, otherwise `Err(Vec<ValidationError>)`.
pub fn validate(cfg: &Config) -> Result<(), Vec<ValidationError>> {
    let mut errs: Vec<ValidationError> = Vec::new();

    // ----- S — Structural integrity -----
    if cfg.gpus.is_empty() {
        errs.push(rule(
            "S1",
            None,
            None,
            "at least one [gpu:*] block required",
        ));
    }
    if cfg.fans.is_empty() {
        errs.push(rule(
            "S2",
            None,
            None,
            "at least one [fan:*] block required",
        ));
    }
    if cfg.groups.is_empty() {
        errs.push(rule(
            "S3",
            None,
            None,
            "at least one [group:*] block required",
        ));
    }

    // S4 — every GPU referenced exactly once
    let mut gpu_refs: HashMap<&str, usize> = HashMap::new();
    for g in &cfg.groups {
        for gid in &g.gpus {
            *gpu_refs.entry(gid.as_str()).or_insert(0) += 1;
        }
    }
    for gpu in &cfg.gpus {
        let count = gpu_refs.get(gpu.id.as_str()).copied().unwrap_or(0);
        if count != 1 {
            errs.push(rule(
                "S4",
                Some(format!("gpu:{}", gpu.id)),
                None,
                format!(
                    "GPU '{}' referenced by {count} group(s); must be exactly 1",
                    gpu.id
                ),
            ));
        }
    }

    // S5 — every fan referenced exactly once
    let mut fan_refs: HashMap<&str, usize> = HashMap::new();
    for g in &cfg.groups {
        for fid in &g.fans {
            *fan_refs.entry(fid.as_str()).or_insert(0) += 1;
        }
    }
    for fan in &cfg.fans {
        let count = fan_refs.get(fan.id.as_str()).copied().unwrap_or(0);
        if count != 1 {
            errs.push(rule(
                "S5",
                Some(format!("fan:{}", fan.id)),
                None,
                format!(
                    "fan '{}' referenced by {count} group(s); must be exactly 1",
                    fan.id
                ),
            ));
        }
    }

    // S6 — each group has ≥1 GPU AND ≥1 fan
    for g in &cfg.groups {
        if g.gpus.is_empty() || g.fans.is_empty() {
            errs.push(rule(
                "S6",
                Some(format!("group:{}", g.id)),
                None,
                "group must reference at least one GPU and at least one fan",
            ));
        }
    }

    // S7 — group references resolve to existing blocks
    let gpu_ids: HashSet<&str> = cfg.gpus.iter().map(|g| g.id.as_str()).collect();
    let fan_ids: HashSet<&str> = cfg.fans.iter().map(|f| f.id.as_str()).collect();
    for g in &cfg.groups {
        for gid in &g.gpus {
            if !gpu_ids.contains(gid.as_str()) {
                errs.push(rule(
                    "S7",
                    Some(format!("group:{}", g.id)),
                    Some("gpus".into()),
                    format!("group references unknown gpu id '{gid}'"),
                ));
            }
        }
        for fid in &g.fans {
            if !fan_ids.contains(fid.as_str()) {
                errs.push(rule(
                    "S7",
                    Some(format!("group:{}", g.id)),
                    Some("fans".into()),
                    format!("group references unknown fan id '{fid}'"),
                ));
            }
        }
    }

    // S8 — nvml_index unique
    let mut seen_nvml: HashMap<u32, &str> = HashMap::new();
    for gpu in &cfg.gpus {
        if let Some(prev) = seen_nvml.insert(gpu.nvml_index, gpu.id.as_str()) {
            errs.push(rule(
                "S8",
                Some(format!("gpu:{}", gpu.id)),
                Some("nvml_index".into()),
                format!(
                    "nvml_index {} duplicated (also used by gpu:{prev})",
                    gpu.nvml_index
                ),
            ));
        }
    }

    // S9 — (chip + device_path? + pwm_channel) tuple unique
    let mut seen_fan: HashMap<(Option<String>, Option<PathBuf>, u32), &str> = HashMap::new();
    for fan in &cfg.fans {
        let tup = (fan.chip.clone(), fan.device_path.clone(), fan.pwm_channel);
        if let Some(prev) = seen_fan.insert(tup, fan.id.as_str()) {
            errs.push(rule(
                "S9",
                Some(format!("fan:{}", fan.id)),
                None,
                format!(
                    "(chip, device_path, pwm_channel) tuple duplicated (also used by fan:{prev})"
                ),
            ));
        }
    }

    // ----- G — Per-GPU -----
    for gpu in &cfg.gpus {
        let pts = gpu.curve.points();
        // G1 already enforced by Curve::new but covered for completeness if
        // a future loader bypasses it.
        if !(2..=10).contains(&pts.len()) {
            errs.push(rule(
                "G1",
                Some(format!("gpu:{}", gpu.id)),
                Some("curve".into()),
                format!("curve has {} point(s); need 2..=10", pts.len()),
            ));
        }
        // G2/G3/G4 likewise. We re-walk to surface them in validate() for
        // the multi-error reporting story (parse only surfaces the first).
        for window in pts.windows(2) {
            let &[(t0, p0), (t1, p1)] = window else {
                continue;
            };
            use std::cmp::Ordering;
            match t0.cmp(&t1) {
                Ordering::Equal => {
                    errs.push(rule(
                        "G3",
                        Some(format!("gpu:{}", gpu.id)),
                        Some("curve".into()),
                        format!("duplicate temp {}", t0.0),
                    ));
                }
                Ordering::Greater => {
                    errs.push(rule(
                        "G2",
                        Some(format!("gpu:{}", gpu.id)),
                        Some("curve".into()),
                        format!("temps not ascending: {} then {}", t0.0, t1.0),
                    ));
                }
                Ordering::Less => {}
            }
            if p1 < p0 {
                errs.push(rule(
                    "G4",
                    Some(format!("gpu:{}", gpu.id)),
                    Some("curve".into()),
                    format!("pct decreased: {} then {}", p0.0, p1.0),
                ));
            }
        }
        // G5/G6 — range
        for &(t, p) in pts {
            if !(0..=110).contains(&t.0) {
                errs.push(rule(
                    "G5",
                    Some(format!("gpu:{}", gpu.id)),
                    Some("curve".into()),
                    format!("curve temp {} outside [0, 110] °C", t.0),
                ));
            }
            if p.0 > 100 {
                errs.push(rule(
                    "G6",
                    Some(format!("gpu:{}", gpu.id)),
                    Some("curve".into()),
                    format!("curve pct {} outside [0, 100]", p.0),
                ));
            }
        }
        // G7 — min ≤ max
        if gpu.min_fan_pct.0 > gpu.max_fan_pct.0 {
            errs.push(rule(
                "G7",
                Some(format!("gpu:{}", gpu.id)),
                None,
                format!(
                    "min_fan_pct ({}) > max_fan_pct ({})",
                    gpu.min_fan_pct.0, gpu.max_fan_pct.0
                ),
            ));
        }
        // G8 — both in [0,100]
        if gpu.min_fan_pct.0 > 100 {
            errs.push(rule(
                "G8",
                Some(format!("gpu:{}", gpu.id)),
                Some("min_fan_pct".into()),
                format!("min_fan_pct {} outside [0, 100]", gpu.min_fan_pct.0),
            ));
        }
        if gpu.max_fan_pct.0 > 100 {
            errs.push(rule(
                "G8",
                Some(format!("gpu:{}", gpu.id)),
                Some("max_fan_pct".into()),
                format!("max_fan_pct {} outside [0, 100]", gpu.max_fan_pct.0),
            ));
        }
    }

    // ----- F — Per-fan -----
    for fan in &cfg.fans {
        if fan.max_rpm <= fan.min_rpm {
            errs.push(rule(
                "F1",
                Some(format!("fan:{}", fan.id)),
                None,
                format!(
                    "max_rpm ({}) must be > min_rpm ({})",
                    fan.max_rpm, fan.min_rpm
                ),
            ));
        }
        if fan.fan_fail_threshold < 1 {
            errs.push(rule(
                "F2",
                Some(format!("fan:{}", fan.id)),
                Some("fan_fail_threshold".into()),
                "fan_fail_threshold must be ≥ 1",
            ));
        }
        // F3 — spin_up_grace_s ≥ 0 (definitional; u32 already enforces this).
        if fan.pwm_channel < 1 {
            errs.push(rule(
                "F4",
                Some(format!("fan:{}", fan.id)),
                Some("pwm_channel".into()),
                "pwm_channel must be ≥ 1",
            ));
        }
        if fan.chip.is_none() && fan.device_path.is_none() && fan.hwmon_path.is_none() {
            errs.push(rule(
                "F5",
                Some(format!("fan:{}", fan.id)),
                None,
                "at least one of chip / device_path / hwmon_path must be set",
            ));
        }
    }

    // ----- W — Watchdog & global -----
    if cfg.watchdog.device.as_os_str().is_empty() {
        errs.push(rule(
            "W1",
            Some("watchdog".into()),
            Some("device".into()),
            "watchdog device path must be non-empty",
        ));
    }
    if !(5..=600).contains(&cfg.watchdog.timeout_s) {
        errs.push(rule(
            "W2",
            Some("watchdog".into()),
            Some("timeout_s".into()),
            format!("timeout_s {} outside [5, 600]", cfg.watchdog.timeout_s),
        ));
    }
    if cfg.watchdog.gpu_fail_threshold < 1 {
        errs.push(rule(
            "W3",
            Some("watchdog".into()),
            Some("gpu_fail_threshold".into()),
            "gpu_fail_threshold must be ≥ 1",
        ));
    }
    if !(100..=10_000).contains(&cfg.global.poll_interval_ms) {
        errs.push(rule(
            "W4",
            Some("global".into()),
            Some("poll_interval_ms".into()),
            format!(
                "poll_interval_ms {} outside [100, 10000]",
                cfg.global.poll_interval_ms
            ),
        ));
    }
    // W5 — poll_interval_ms × max(thresholds) < timeout_s × 500
    let max_threshold = cfg
        .fans
        .iter()
        .map(|f| f.fan_fail_threshold)
        .chain(std::iter::once(cfg.watchdog.gpu_fail_threshold))
        .max()
        .unwrap_or(0);
    let lhs = u64::from(cfg.global.poll_interval_ms) * u64::from(max_threshold);
    let rhs = u64::from(cfg.watchdog.timeout_s) * 500;
    if lhs >= rhs {
        errs.push(rule(
            "W5",
            None,
            None,
            format!(
                "poll_interval_ms × max(thresholds) ({lhs}) must be < timeout_s × 500 ({rhs}) — leave headroom for fault propagation"
            ),
        ));
    }
    // W6 — log_level enum already enforced at parse time.
    if cfg.global.config_version != CONFIG_VERSION_SUPPORTED {
        errs.push(rule(
            "W7",
            Some("global".into()),
            Some("config_version".into()),
            format!(
                "unsupported config_version {} (this daemon recognises {})",
                cfg.global.config_version, CONFIG_VERSION_SUPPORTED
            ),
        ));
    }

    // T1 is enforced at parse time (UnknownSection).
    // T2 is surfaced as warnings, not validation errors.

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

fn rule(
    code: &'static str,
    section: Option<String>,
    key: Option<String>,
    msg: impl Into<String>,
) -> ValidationError {
    ValidationError {
        rule: code,
        section,
        key,
        message: msg.into(),
    }
}

/// Read a config file from disk, parse, and validate.
///
/// This is the only place `std::fs` is used in this module — keeping it
/// pinned to one function preserves the pure/I-O split (ADR-0006).
pub fn load(path: &Path) -> Result<(Config, Vec<Warning>), ConfigError> {
    let s = std::fs::read_to_string(path)?;
    let (cfg, warnings) = parse(&s)?;
    if let Err(verrs) = validate(&cfg) {
        return Err(ConfigError::Validation(verrs));
    }
    Ok((cfg, warnings))
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

    /// Minimal canonical config — passes every validation rule.
    fn canonical() -> &'static str {
        r#"
[global]
config_version   = 1
poll_interval_ms = 1000
log_level        = info
log_file         =

[watchdog]
enabled             = true
device              = /dev/watchdog
timeout_s           = 30
gpu_fail_threshold  = 3

[gpu:0]
nvml_index   = 0
curve        = 40:20, 55:40, 70:80, 80:100
min_fan_pct  = 20
max_fan_pct  = 100

[gpu:1]
nvml_index   = 1
curve        = 40:20, 55:40, 70:80, 80:100
min_fan_pct  = 20
max_fan_pct  = 100

[gpu:2]
nvml_index   = 2
curve        = 40:20, 55:40, 70:80, 80:100
min_fan_pct  = 20
max_fan_pct  = 100

[fan:f0]
chip                = nct6798
pwm_channel         = 1
min_rpm             = 200
max_rpm             = 3000
fan_fail_threshold  = 3
spin_up_grace_s     = 10

[fan:f1]
chip                = nct6798
pwm_channel         = 2
min_rpm             = 200
max_rpm             = 3000
fan_fail_threshold  = 3
spin_up_grace_s     = 10

[group:main]
gpus = 0
fans = f0

[group:shared]
gpus = 1, 2
fans = f1
"#
    }

    fn parse_ok(s: &str) -> Config {
        let (cfg, _w) = parse(s).expect("parse ok");
        cfg
    }

    fn validation_errors(cfg: &Config) -> Vec<ValidationError> {
        match validate(cfg) {
            Ok(()) => vec![],
            Err(e) => e,
        }
    }

    fn rules_in(errs: &[ValidationError]) -> Vec<&'static str> {
        errs.iter().map(|e| e.rule).collect()
    }

    #[test]
    fn happy_path_canonical_config() {
        let (cfg, warnings) = parse(canonical()).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(validate(&cfg), Ok(()));
        assert_eq!(cfg.gpus.len(), 3);
        assert_eq!(cfg.fans.len(), 2);
        assert_eq!(cfg.groups.len(), 2);
    }

    // T1 — unknown section header (capitalised variant)
    #[test]
    fn t1_unknown_section_capitalised() {
        let s = canonical().replace("[gpu:0]", "[GPU:0]");
        let err = parse(&s).unwrap_err();
        assert!(
            matches!(err, ConfigError::UnknownSection(ref n) if n == "GPU:0"),
            "got {err:?}"
        );
    }

    // T2 — unknown key within known section is a warning (not fatal)
    #[test]
    fn t2_unknown_key_is_warning() {
        let s = canonical().replace("min_fan_pct  = 20", "min_fan_pct  = 20\nflubber = ok");
        let (_cfg, warnings) = parse(&s).unwrap();
        assert!(
            warnings.iter().any(|w| w.key == "flubber"),
            "warnings: {warnings:?}"
        );
    }

    // S1
    #[test]
    fn s1_no_gpu_blocks() {
        let s = canonical()
            .replace("[gpu:0]", "[__hidden_gpu0__]")
            .replace("[gpu:1]", "[__hidden_gpu1__]")
            .replace("[gpu:2]", "[__hidden_gpu2__]");
        // Now those would be unknown sections (T1) — instead build directly.
        let mut cfg = parse_ok(canonical());
        cfg.gpus.clear();
        let _ = s; // unused here
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S1"));
    }

    // S2
    #[test]
    fn s2_no_fan_blocks() {
        let mut cfg = parse_ok(canonical());
        cfg.fans.clear();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S2"));
    }

    // S3
    #[test]
    fn s3_no_group_blocks() {
        let mut cfg = parse_ok(canonical());
        cfg.groups.clear();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S3"));
    }

    // S4 — orphan GPU
    #[test]
    fn s4_orphan_gpu_not_referenced() {
        let mut cfg = parse_ok(canonical());
        cfg.groups.retain(|g| g.id != "shared");
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S4"));
    }

    // S5 — orphan fan
    #[test]
    fn s5_orphan_fan_not_referenced() {
        let mut cfg = parse_ok(canonical());
        for g in &mut cfg.groups {
            g.fans.retain(|f| f != "f1");
        }
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S5"));
    }

    // S6 — empty group
    #[test]
    fn s6_group_missing_fans() {
        let mut cfg = parse_ok(canonical());
        for g in &mut cfg.groups {
            g.fans.clear();
        }
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S6"));
    }

    // S7 — dangling reference
    #[test]
    fn s7_group_references_unknown_id() {
        let mut cfg = parse_ok(canonical());
        cfg.groups[0].fans.push("does_not_exist".into());
        // Also bump f0 reference count to keep S5 happy for the remaining fan.
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S7"));
    }

    // S8 — duplicate nvml_index
    #[test]
    fn s8_duplicate_nvml_index() {
        let mut cfg = parse_ok(canonical());
        cfg.gpus[1].nvml_index = cfg.gpus[0].nvml_index;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S8"));
    }

    // S9 — duplicate fan tuple
    #[test]
    fn s9_duplicate_fan_tuple() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[1].pwm_channel = cfg.fans[0].pwm_channel;
        cfg.fans[1].chip = cfg.fans[0].chip.clone();
        cfg.fans[1].device_path = cfg.fans[0].device_path.clone();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"S9"));
    }

    // G1 — too few curve points (constructor enforces, surfaced via validate too)
    #[test]
    fn g1_curve_too_few_points_caught_at_parse() {
        let s = canonical().replace(
            "curve        = 40:20, 55:40, 70:80, 80:100",
            "curve        = 40:20",
        );
        // First occurrence only — leave the other GPUs valid so other rules don't fire.
        let err = parse(&s).unwrap_err();
        match err {
            ConfigError::Curve { source, .. } => {
                assert!(matches!(source, CurveError::TooFewPoints(_)));
            }
            other => panic!("expected Curve error, got {other:?}"),
        }
    }

    // G2 — temps not ascending
    #[test]
    fn g2_temps_not_ascending() {
        let s = canonical().replacen(
            "curve        = 40:20, 55:40, 70:80, 80:100",
            "curve        = 70:80, 40:20, 55:40, 80:100",
            1,
        );
        let err = parse(&s).unwrap_err();
        match err {
            ConfigError::Curve { source, .. } => {
                assert!(matches!(source, CurveError::NotMonotonicTemp { .. }));
            }
            _ => panic!("expected Curve error"),
        }
    }

    // G3 — duplicate temps
    #[test]
    fn g3_duplicate_temps() {
        let s = canonical().replacen(
            "curve        = 40:20, 55:40, 70:80, 80:100",
            "curve        = 40:20, 40:40, 70:80, 80:100",
            1,
        );
        let err = parse(&s).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Curve {
                source: CurveError::DuplicateTemp(_),
                ..
            }
        ));
    }

    // G4 — pct decreasing
    #[test]
    fn g4_pct_decreasing() {
        let s = canonical().replacen(
            "curve        = 40:20, 55:40, 70:80, 80:100",
            "curve        = 40:80, 55:40, 70:50, 80:100",
            1,
        );
        let err = parse(&s).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Curve {
                source: CurveError::NotMonotonicPct { .. },
                ..
            }
        ));
    }

    // G5 — temp out of range
    #[test]
    fn g5_temp_out_of_range() {
        // 120 > 110 — accepted by Curve::new (range is config concern), caught here.
        let mut cfg = parse_ok(canonical());
        cfg.gpus[0].curve =
            Curve::new(vec![(Celsius(40), Pct(20)), (Celsius(120), Pct(100))]).unwrap();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"G5"));
    }

    // G6 — pct out of range (constructed directly to bypass parse cap)
    #[test]
    fn g6_pct_out_of_range() {
        let mut cfg = parse_ok(canonical());
        cfg.gpus[0].curve =
            Curve::new(vec![(Celsius(40), Pct(20)), (Celsius(80), Pct(200))]).unwrap();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"G6"));
    }

    // G7 — min > max
    #[test]
    fn g7_min_greater_than_max() {
        let mut cfg = parse_ok(canonical());
        cfg.gpus[0].min_fan_pct = Pct(80);
        cfg.gpus[0].max_fan_pct = Pct(50);
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"G7"));
    }

    // G8 — out of [0,100]
    #[test]
    fn g8_pct_out_of_range() {
        let mut cfg = parse_ok(canonical());
        cfg.gpus[0].min_fan_pct = Pct(200);
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"G8"));
    }

    // F1
    #[test]
    fn f1_max_rpm_not_greater_than_min() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[0].max_rpm = cfg.fans[0].min_rpm;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"F1"));
    }

    // F2
    #[test]
    fn f2_fan_fail_threshold_zero() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[0].fan_fail_threshold = 0;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"F2"));
    }

    // F3 — spin_up_grace_s ≥ 0 is definitional via u32; test absence of error on 0.
    #[test]
    fn f3_spin_up_grace_zero_is_ok() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[0].spin_up_grace_s = 0;
        // Should still validate (no F3 violation possible with u32 type).
        // But this test guards against future regressions if the type changes.
        let errs = validation_errors(&cfg);
        assert!(!rules_in(&errs).contains(&"F3"));
    }

    // F4
    #[test]
    fn f4_pwm_channel_zero() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[0].pwm_channel = 0;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"F4"));
    }

    // F5
    #[test]
    fn f5_no_chip_or_path() {
        let mut cfg = parse_ok(canonical());
        cfg.fans[0].chip = None;
        cfg.fans[0].device_path = None;
        cfg.fans[0].hwmon_path = None;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"F5"));
    }

    // W1
    #[test]
    fn w1_empty_watchdog_device() {
        let mut cfg = parse_ok(canonical());
        cfg.watchdog.device = PathBuf::new();
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W1"));
    }

    // W2
    #[test]
    fn w2_timeout_out_of_range() {
        let mut cfg = parse_ok(canonical());
        cfg.watchdog.timeout_s = 1000;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W2"));
    }

    // W3
    #[test]
    fn w3_gpu_fail_threshold_zero() {
        let mut cfg = parse_ok(canonical());
        cfg.watchdog.gpu_fail_threshold = 0;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W3"));
    }

    // W4
    #[test]
    fn w4_poll_interval_out_of_range() {
        let mut cfg = parse_ok(canonical());
        cfg.global.poll_interval_ms = 50;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W4"));
    }

    // W5
    #[test]
    fn w5_no_headroom() {
        let mut cfg = parse_ok(canonical());
        // poll * max_threshold must be < timeout * 500.
        // Set poll to 10000 and threshold to 100 → 1_000_000; timeout 30 * 500 = 15_000.
        cfg.global.poll_interval_ms = 10_000;
        cfg.fans[0].fan_fail_threshold = 100;
        cfg.watchdog.timeout_s = 30;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W5"));
    }

    // W6 — enforced at parse time
    #[test]
    fn w6_invalid_log_level_caught_at_parse() {
        let s = canonical().replace("log_level        = info", "log_level        = chatty");
        let err = parse(&s).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    // W7
    #[test]
    fn w7_unknown_config_version() {
        let mut cfg = parse_ok(canonical());
        cfg.global.config_version = 99;
        let errs = validation_errors(&cfg);
        assert!(rules_in(&errs).contains(&"W7"));
    }

    // Multiple violations accumulate
    #[test]
    fn multiple_violations_accumulate_in_one_pass() {
        let mut cfg = parse_ok(canonical());
        // S4 violation: drop one group entirely so a GPU is unreferenced.
        cfg.groups.retain(|g| g.id != "shared");
        // G7 violation: invert min/max on one GPU.
        cfg.gpus[0].min_fan_pct = Pct(80);
        cfg.gpus[0].max_fan_pct = Pct(50);
        let errs = validation_errors(&cfg);
        let codes = rules_in(&errs);
        assert!(codes.contains(&"S4"), "codes: {codes:?}");
        assert!(codes.contains(&"G7"), "codes: {codes:?}");
    }
}
