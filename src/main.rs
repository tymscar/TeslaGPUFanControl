//! tesla_fan_control daemon — composition root (Wave 3).
//!
//! Init / shutdown order is load-bearing for safety (ADR-0003): PWM control
//! is taken AFTER the watchdog is armed and released BEFORE the watchdog is
//! disarmed. Every fault path converges on "stop feeding the watchdog"
//! (ADR-0001), which lets the kernel reboot via `/dev/watchdog` if the
//! cooling logic itself becomes unsafe.

mod chip;
mod config;
mod curve;
mod fan;
mod group;
mod logger;
mod nvml;
mod power;
mod units;
mod watchdog;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
use signal_hook::flag;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, FanConfig, GpuConfig, GroupConfig};
use crate::fan::{Fan, SPIN_UP_DELTA_PCT};
use crate::group::{CoolingGroup, FanId, GpuId, GpuView};
use crate::nvml::{NvmlOps, NvmlReader};
use crate::units::{Celsius, Pct};
use crate::watchdog::{FanHealth, FaultTracker, HardwareWatchdog};

const DEFAULT_CONFIG_PATH: &str = "/etc/tesla_fan_control.conf";

#[derive(Parser, Debug)]
#[command(
    name = "tesla_fan_control",
    version,
    about = "Headless Tesla GPU fan control daemon"
)]
struct Args {
    /// Path to the INI config file.
    #[arg(short = 'c', long = "config", default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    /// Log to stderr (in addition to journald / file sinks). Logger level
    /// still comes from the config; `RUST_LOG` overrides it via EnvFilter.
    #[arg(short = 'f', long = "foreground")]
    foreground: bool,

    /// Validate the config and exit. Performs no I/O against /sys, NVML, or
    /// the watchdog. Runnable as a non-root user.
    #[arg(long = "check-config", conflicts_with = "calibrate_fans")]
    check_config: bool,

    /// Sweep PWM duty across every fan and print a per-fan recommendation.
    /// Refuses to run while the systemd unit is active.
    #[arg(long = "calibrate-fans")]
    calibrate_fans: bool,
}

fn main() {
    let args = Args::parse();
    std::process::exit(dispatch(args));
}

fn dispatch(args: Args) -> i32 {
    if args.check_config {
        return check_config_cmd(&args.config);
    }
    if args.calibrate_fans {
        return calibrate_fans_cmd(&args.config);
    }
    match run_daemon(&args.config, args.foreground) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "daemon exited with error");
            eprintln!("fatal: {e:#}");
            1
        }
    }
}

// --------------------------------------------------------------------- //
// --check-config                                                         //
// --------------------------------------------------------------------- //

fn check_config_cmd(path: &Path) -> i32 {
    match config::load(path) {
        Ok((_cfg, warnings)) => {
            for w in &warnings {
                eprintln!("warning: [{}] {}: {}", w.section, w.key, w.message);
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

// --------------------------------------------------------------------- //
// daemon                                                                 //
// --------------------------------------------------------------------- //

fn run_daemon(cfg_path: &Path, foreground: bool) -> Result<()> {
    let _ = foreground; // logger sinks come from config + RUST_LOG.

    // ADR-0003 step 1: parse + validate config (pure, before any I/O).
    let (mut cfg, warnings) =
        config::load(cfg_path).with_context(|| format!("loading config {}", cfg_path.display()))?;

    // ADR-0003 step 1b: install logger before further work so init failures
    // surface through the configured sink.
    logger::init(cfg.global.log_level, cfg.global.log_file.as_deref())
        .map_err(|e| anyhow!("logger init failed: {e}"))?;
    for w in &warnings {
        tracing::warn!(section = %w.section, key = %w.key, "{}", w.message);
    }

    // ADR-0003 step 2: resolve hwmon chips and verify R2 sysfs surface.
    let chip_map = chip::scan().context("scanning /sys/class/hwmon")?;
    let mut hwmon_paths: HashMap<FanId, PathBuf> = HashMap::new();
    for fan_cfg in &cfg.fans {
        let path = chip::resolve(&chip_map, fan_cfg)
            .with_context(|| format!("resolving fan {}", fan_cfg.id))?;
        verify_fan_sysfs(&path, fan_cfg)?;
        hwmon_paths.insert(fan_cfg.id.clone(), path);
    }

    // ADR-0003 step 3: NVML init (validates every requested index).
    let nvml_indices: Vec<u32> = cfg.gpus.iter().map(|g| g.nvml_index).collect();
    let mut nvml = NvmlReader::init(&nvml_indices).context("NVML initialisation")?;

    // ADR-0008 R6: apply Power Limits to every GPU that opted in. Runs before
    // the watchdog is armed so any failure leaves BIOS in control of the
    // fans (ADR-0002 BIOS failsafe still holds — refuse-to-start is safe).
    apply_power_limits_at_startup(&mut nvml, &cfg.gpus)
        .context("R6: applying configured Power Limits")?;

    // ADR-0003 step 4: arm the hardware watchdog BEFORE taking PWM control.
    // If watchdog opens fail here, BIOS still owns the fans (safe fallback).
    let mut hw_watchdog = if cfg.watchdog.enabled {
        let wd = HardwareWatchdog::open(&cfg.watchdog.device, cfg.watchdog.timeout_s)
            .with_context(|| format!("opening watchdog {}", cfg.watchdog.device.display()))?;
        if let Some(t) = wd.accepted_timeout_s() {
            tracing::info!(
                requested_timeout_s = cfg.watchdog.timeout_s,
                accepted_timeout_s = t,
                "hardware watchdog armed"
            );
        }
        wd
    } else {
        tracing::warn!("watchdog disabled in config; running without hardware fail-safe");
        HardwareWatchdog::disabled()
    };

    // ADR-0003 step 5: take manual control of every fan AFTER the watchdog
    // is armed. From here on, a panic / unhandled error must converge on
    // "stop feeding the watchdog".
    let mut fans: HashMap<FanId, Fan> = HashMap::new();
    let mut spin_up_entries: HashMap<FanId, Instant> = HashMap::new();
    for fan_cfg in &cfg.fans {
        let path = hwmon_paths
            .get(&fan_cfg.id)
            .cloned()
            .ok_or_else(|| anyhow!("internal: hwmon_path missing for fan {}", fan_cfg.id))?;
        let mut fan = Fan::open(
            path,
            fan_cfg.pwm_channel,
            fan_cfg.min_rpm,
            fan_cfg.max_rpm,
            fan_cfg.spin_up_grace_s,
        );
        fan.take_manual_control()
            .with_context(|| format!("take_manual_control for fan {}", fan_cfg.id))?;
        spin_up_entries.insert(fan_cfg.id.clone(), Instant::now());
        fans.insert(fan_cfg.id.clone(), fan);
    }

    // ADR-0003 step 6: sd_notify READY=1 (best-effort; the hardware watchdog
    // is the actual safety mechanism).
    sd_notify(&format!("READY=1\nMAINPID={}", std::process::id()));

    // Build runtime cooling groups + fault tracker.
    let cooling_groups = build_cooling_groups(&cfg.groups);
    let gpu_ids: Vec<String> = cfg.gpus.iter().map(|g| g.id.clone()).collect();
    let fan_ids: Vec<String> = cfg.fans.iter().map(|f| f.id.clone()).collect();
    // Per-fan thresholds in the config differ; the tracker takes one
    // fan_threshold, so use the most-permissive (largest) value.
    let fan_threshold = cfg
        .fans
        .iter()
        .map(|f| f.fan_fail_threshold)
        .max()
        .unwrap_or(1);
    tracing::info!(
        fan_threshold,
        gpu_threshold = cfg.watchdog.gpu_fail_threshold,
        "fault tracker thresholds"
    );
    let mut fault_tracker = FaultTracker::new(
        &gpu_ids,
        &fan_ids,
        cfg.watchdog.gpu_fail_threshold,
        fan_threshold,
    );

    // ADR-0003 step 7: signal handlers (atomic-flag pattern; no work in the
    // handler — main loop reacts on the next iteration).
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let reload_flag = Arc::new(AtomicBool::new(false));
    let dump_flag = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&shutdown_flag)).context("register SIGTERM")?;
    flag::register(SIGINT, Arc::clone(&shutdown_flag)).context("register SIGINT")?;
    flag::register(SIGHUP, Arc::clone(&reload_flag)).context("register SIGHUP")?;
    flag::register(SIGUSR1, Arc::clone(&dump_flag)).context("register SIGUSR1")?;

    let mut last_temps: HashMap<GpuId, Celsius> = HashMap::new();
    let mut last_set_pct: HashMap<FanId, Pct> = HashMap::new();
    let mut last_rpm: HashMap<FanId, u32> = HashMap::new();
    let mut last_power_check: HashMap<GpuId, Instant> = HashMap::new();

    // ------------------------- main loop ------------------------------ //
    while !shutdown_flag.load(Ordering::Relaxed) {
        if reload_flag.swap(false, Ordering::Relaxed) {
            handle_sighup(&mut cfg, cfg_path);
        }
        if dump_flag.swap(false, Ordering::Relaxed) {
            handle_sigusr1(
                &last_temps,
                &last_set_pct,
                &last_rpm,
                fault_tracker.any_fault(),
            );
        }

        let mut should_slam = false;

        // Read every GPU's temperature.
        let mut temps: HashMap<GpuId, Celsius> = HashMap::new();
        for gpu in &cfg.gpus {
            let result = nvml.read_temp(gpu.nvml_index);
            let read_ok = result.is_ok();
            match &result {
                Ok(t) => {
                    tracing::debug!(gpu_id = %gpu.id, temp = t.0, "nvml read");
                    temps.insert(gpu.id.clone(), *t);
                    last_temps.insert(gpu.id.clone(), *t);
                }
                Err(e) => {
                    tracing::warn!(gpu_id = %gpu.id, error = %e, "nvml read failed");
                }
            }
            if let Some(fault) = fault_tracker.tick_gpu(&gpu.id, read_ok) {
                tracing::error!(
                    ?fault,
                    fault_kind = "gpu_nvml",
                    "declared fault — stopping watchdog feed"
                );
                should_slam = true;
            }

            // ADR-0008 main-loop step 1b — Power Limit drift check.
            // Runs only when due (every Nth poll). Failures here use the
            // independent power counter in FaultTracker (split-counter
            // design — see ADR-0008 §FaultTracker).
            if gpu.power_limit_enabled {
                let target_w = match gpu.power_limit_w {
                    Some(w) => w,
                    None => continue, // P1 should have caught; defensive.
                };
                let interval =
                    Duration::from_secs(u64::from(cfg.global.power_limit_check_interval_s));
                if power::due(
                    last_power_check.get(&gpu.id).copied(),
                    Instant::now(),
                    interval,
                ) {
                    let outcome = match nvml.read_power_limit_w(gpu.nvml_index) {
                        Ok(measured) if measured == target_w => Ok(()),
                        Ok(measured) if measured < target_w => {
                            tracing::warn!(
                                gpu_id = %gpu.id,
                                measured_w = measured,
                                target_w,
                                direction = "down",
                                "power-limit drift below target; re-asserting (no PSU risk)"
                            );
                            nvml.set_power_limit_w(gpu.nvml_index, target_w)
                        }
                        Ok(measured) => {
                            // Upward drift = PSU exposure (ADR-0008).
                            tracing::error!(
                                gpu_id = %gpu.id,
                                measured_w = measured,
                                target_w,
                                direction = "up",
                                "power-limit drift ABOVE target; PSU exposure — re-asserting"
                            );
                            sd_notify(&format!(
                                "STATUS=power-limit drift up on {} ({}W > {}W target) — re-asserted",
                                gpu.id, measured, target_w
                            ));
                            nvml.set_power_limit_w(gpu.nvml_index, target_w)
                        }
                        Err(e) => Err(e),
                    };
                    let op_ok = outcome.is_ok();
                    if let Err(e) = outcome {
                        tracing::warn!(
                            gpu_id = %gpu.id,
                            error = %e,
                            "power-limit operation failed"
                        );
                    }
                    if let Some(fault) = fault_tracker.tick_gpu_power(&gpu.id, op_ok) {
                        tracing::error!(
                            ?fault,
                            fault_kind = "gpu_nvml",
                            "declared fault — stopping watchdog feed"
                        );
                        should_slam = true;
                    }
                    last_power_check.insert(gpu.id.clone(), Instant::now());
                }
            }
        }

        // Per-group target → fan set → RPM read → tracker tick.
        // Build views once per iteration so SIGHUP reloads of curves /
        // min/max take effect on the very next loop.
        let gpu_views: HashMap<GpuId, GpuView<'_>> = cfg
            .gpus
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    GpuView {
                        curve: &g.curve,
                        min_fan_pct: g.min_fan_pct,
                        max_fan_pct: g.max_fan_pct,
                    },
                )
            })
            .collect();
        for group in &cooling_groups {
            let target = group::target_pct(group, &temps, &gpu_views);
            for fan_id in &group.fans {
                let Some(fan) = fans.get_mut(fan_id) else {
                    continue;
                };
                let entered_spin_up = match last_set_pct.get(fan_id) {
                    Some(prev) => target.0.saturating_sub(prev.0) > SPIN_UP_DELTA_PCT,
                    None => false,
                };
                if let Err(e) = fan.set(target) {
                    tracing::error!(fan_id = %fan_id, error = %e, "fan set failed");
                }
                last_set_pct.insert(fan_id.clone(), target);
                if entered_spin_up {
                    spin_up_entries.insert(fan_id.clone(), Instant::now());
                }
                let elapsed = spin_up_entries
                    .get(fan_id)
                    .map(|i| u32::try_from(i.elapsed().as_secs()).unwrap_or(u32::MAX))
                    .unwrap_or(0);
                match fan.read_rpm() {
                    Ok(rpm) => {
                        last_rpm.insert(fan_id.clone(), rpm);
                        fan.tick_spin_up(rpm, elapsed);
                        let health = fan.check_health(rpm);
                        if let Some(fault) = fault_tracker.tick_fan(fan_id, health, rpm) {
                            tracing::error!(
                                ?fault,
                                fault_kind = "fan_hardware",
                                "declared fault — stopping watchdog feed"
                            );
                            should_slam = true;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(fan_id = %fan_id, error = %e, "rpm read failed");
                        if let Some(fault) = fault_tracker.tick_fan(fan_id, FanHealth::Stalled, 0) {
                            tracing::error!(
                                ?fault,
                                fault_kind = "fan_hardware",
                                "declared fault — stopping watchdog feed"
                            );
                            should_slam = true;
                        }
                    }
                }
            }
        }

        if should_slam {
            slam_all_fans_to_max(&mut fans, &mut spin_up_entries);
        }

        // Watchdog feed gating (ADR-0001): no fault → feed; fault → silence.
        if !fault_tracker.any_fault() {
            if let Err(e) = hw_watchdog.feed() {
                tracing::error!(error = %e, "watchdog feed failed");
            }
            sd_notify("WATCHDOG=1");
        }

        std::thread::sleep(Duration::from_millis(u64::from(
            cfg.global.poll_interval_ms,
        )));
    }

    // ----------------------- graceful shutdown ------------------------ //
    tracing::info!("shutdown signal received; restoring fans and disarming watchdog");
    sd_notify("STOPPING=1");

    // ADR-0009: Power Limit restore is opt-in. Default keeps the limit at
    // its last-set value so PSU protection persists across daemon-down
    // windows. Runs FIRST (before fan restore) so it happens while NVML is
    // healthy and we still own manual fan control. Failures here log WARN
    // and don't block the rest of shutdown.
    if cfg.global.power_limit_restore_on_shutdown {
        restore_power_limits_to_default(&mut nvml, &cfg.gpus);
    }

    // ADR-0003 inverse order: release PWM control BEFORE disarming the
    // watchdog. Continue restoring even if one fan fails.
    for (fan_id, fan) in fans.iter_mut() {
        if let Err(e) = fan.restore() {
            tracing::error!(fan_id = %fan_id, error = %e, "fan restore failed (continuing)");
        }
    }
    if let Err(e) = hw_watchdog.disarm_and_close() {
        tracing::error!(error = %e, "watchdog disarm failed");
    }
    drop(nvml); // explicit; Nvml::Drop runs nvmlShutdown.
    Ok(())
}

fn slam_all_fans_to_max(
    fans: &mut HashMap<FanId, Fan>,
    spin_up_entries: &mut HashMap<FanId, Instant>,
) {
    for (fan_id, fan) in fans.iter_mut() {
        if let Err(e) = fan.set_max() {
            tracing::error!(fan_id = %fan_id, error = %e, "set_max failed");
        }
        spin_up_entries.insert(fan_id.clone(), Instant::now());
    }
}

fn build_cooling_groups(groups: &[GroupConfig]) -> Vec<CoolingGroup> {
    groups
        .iter()
        .map(|g| CoolingGroup {
            id: g.id.clone(),
            gpus: g.gpus.clone(),
            fans: g.fans.clone(),
        })
        .collect()
}

/// ADR-0009 — opt-in shutdown helper. Restores each enabled GPU's
/// management limit to the driver default. Failures log WARN and do not
/// abort the rest of the graceful-shutdown sequence; we are already in the
/// pre-disarm phase and the watchdog still has to be released.
fn restore_power_limits_to_default(nvml: &mut NvmlReader, gpus: &[GpuConfig]) {
    for gpu in gpus {
        if !gpu.power_limit_enabled {
            continue;
        }
        let default_w = match nvml.read_power_limit_default_w(gpu.nvml_index) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    gpu_id = %gpu.id,
                    error = %e,
                    "shutdown: could not read default Power Limit; leaving current value in place"
                );
                continue;
            }
        };
        match nvml.set_power_limit_w(gpu.nvml_index, default_w) {
            Ok(()) => tracing::info!(
                gpu_id = %gpu.id,
                default_w,
                "shutdown: restored Power Limit to driver default"
            ),
            Err(e) => tracing::warn!(
                gpu_id = %gpu.id,
                error = %e,
                "shutdown: failed to restore Power Limit"
            ),
        }
    }
}

/// R6 — refuse to start if any configured `power_limit_w` is outside the
/// driver's `power_management_limit_constraints` or if the initial
/// `set_power_management_limit` call fails. ADR-0001 fail-loud policy:
/// if the operator asked for a Power Limit and we can't honour it, do not
/// pretend.
fn apply_power_limits_at_startup(nvml: &mut NvmlReader, gpus: &[GpuConfig]) -> Result<()> {
    for gpu in gpus {
        if !gpu.power_limit_enabled {
            continue;
        }
        let target_w = gpu.power_limit_w.ok_or_else(|| {
            anyhow!(
                "internal: P1 should have caught missing power_limit_w for {}",
                gpu.id
            )
        })?;
        let (min_w, max_w) = nvml
            .power_limit_constraints_w(gpu.nvml_index)
            .with_context(|| format!("R6: reading power-limit constraints for gpu:{}", gpu.id))?;
        if target_w < min_w || target_w > max_w {
            return Err(anyhow!(
                "R6: gpu:{} power_limit_w {} W outside driver constraints [{}, {}] W",
                gpu.id,
                target_w,
                min_w,
                max_w
            ));
        }
        nvml.set_power_limit_w(gpu.nvml_index, target_w)
            .with_context(|| format!("R6: applying initial Power Limit to gpu:{}", gpu.id))?;
        tracing::info!(
            gpu_id = %gpu.id,
            power_limit_w = target_w,
            min_w,
            max_w,
            "applied configured Power Limit"
        );
    }
    Ok(())
}

fn verify_fan_sysfs(path: &Path, fan_cfg: &FanConfig) -> Result<()> {
    let n = fan_cfg.pwm_channel;
    for required in [
        format!("pwm{n}"),
        format!("pwm{n}_enable"),
        format!("fan{n}_input"),
    ] {
        let p = path.join(&required);
        if !p.exists() {
            return Err(anyhow!(
                "fan {}: missing sysfs file {} under {} (R2)",
                fan_cfg.id,
                required,
                path.display()
            ));
        }
    }
    Ok(())
}

// --------------------------------------------------------------------- //
// SIGHUP — data-only swap; structural drift is rejected                 //
// --------------------------------------------------------------------- //

fn handle_sighup(cfg: &mut Config, cfg_path: &Path) {
    let new = match config::load(cfg_path) {
        Ok((c, warnings)) => {
            for w in &warnings {
                tracing::warn!(section = %w.section, key = %w.key, "{}", w.message);
            }
            c
        }
        Err(e) => {
            tracing::error!(error = %e, "SIGHUP: failed to reload config");
            return;
        }
    };

    if let Err(diff) = structurally_identical(cfg, &new) {
        tracing::error!(
            diff = %diff,
            "SIGHUP: structural drift; keeping previous config"
        );
        return;
    }
    swap_data_fields(cfg, new);
    tracing::info!("config reloaded");
}

fn structurally_identical(a: &Config, b: &Config) -> Result<(), String> {
    if a.watchdog.device != b.watchdog.device {
        return Err("watchdog.device".into());
    }
    if a.watchdog.timeout_s != b.watchdog.timeout_s {
        return Err("watchdog.timeout_s".into());
    }
    let a_gpus: HashMap<&str, u32> = a
        .gpus
        .iter()
        .map(|g| (g.id.as_str(), g.nvml_index))
        .collect();
    let b_gpus: HashMap<&str, u32> = b
        .gpus
        .iter()
        .map(|g| (g.id.as_str(), g.nvml_index))
        .collect();
    if a_gpus != b_gpus {
        return Err("gpu set or nvml_index".into());
    }
    type FanIdentity = (Option<String>, Option<PathBuf>, Option<PathBuf>, u32);
    let identity = |f: &FanConfig| -> FanIdentity {
        (
            f.chip.clone(),
            f.device_path.clone(),
            f.hwmon_path.clone(),
            f.pwm_channel,
        )
    };
    let a_fans: HashMap<&str, FanIdentity> = a
        .fans
        .iter()
        .map(|f| (f.id.as_str(), identity(f)))
        .collect();
    let b_fans: HashMap<&str, FanIdentity> = b
        .fans
        .iter()
        .map(|f| (f.id.as_str(), identity(f)))
        .collect();
    if a_fans != b_fans {
        return Err("fan set or fan identity tuple".into());
    }
    let a_groups: HashMap<&str, (Vec<String>, Vec<String>)> = a
        .groups
        .iter()
        .map(|g| (g.id.as_str(), (g.gpus.clone(), g.fans.clone())))
        .collect();
    let b_groups: HashMap<&str, (Vec<String>, Vec<String>)> = b
        .groups
        .iter()
        .map(|g| (g.id.as_str(), (g.gpus.clone(), g.fans.clone())))
        .collect();
    if a_groups != b_groups {
        return Err("group set or group membership".into());
    }
    Ok(())
}

fn swap_data_fields(running: &mut Config, new: Config) {
    running.global.log_level = new.global.log_level;
    running.global.poll_interval_ms = new.global.poll_interval_ms;
    running.global.power_limit_check_interval_s = new.global.power_limit_check_interval_s;
    running.global.power_limit_restore_on_shutdown = new.global.power_limit_restore_on_shutdown;
    running.global.power_limit_validate_max_w = new.global.power_limit_validate_max_w;
    running.watchdog.gpu_fail_threshold = new.watchdog.gpu_fail_threshold;

    let new_gpus: HashMap<String, GpuConfig> =
        new.gpus.into_iter().map(|g| (g.id.clone(), g)).collect();
    for gpu in &mut running.gpus {
        if let Some(updated) = new_gpus.get(&gpu.id) {
            gpu.curve = updated.curve.clone();
            gpu.min_fan_pct = updated.min_fan_pct;
            gpu.max_fan_pct = updated.max_fan_pct;
            gpu.power_limit_enabled = updated.power_limit_enabled;
            gpu.power_limit_w = updated.power_limit_w;
        }
    }
    let new_fans: HashMap<String, FanConfig> =
        new.fans.into_iter().map(|f| (f.id.clone(), f)).collect();
    for fan in &mut running.fans {
        if let Some(updated) = new_fans.get(&fan.id) {
            fan.min_rpm = updated.min_rpm;
            fan.max_rpm = updated.max_rpm;
            fan.fan_fail_threshold = updated.fan_fail_threshold;
            fan.spin_up_grace_s = updated.spin_up_grace_s;
        }
    }
}

fn handle_sigusr1(
    last_temps: &HashMap<GpuId, Celsius>,
    last_set_pct: &HashMap<FanId, Pct>,
    last_rpm: &HashMap<FanId, u32>,
    any_fault: bool,
) {
    for (gpu_id, temp) in last_temps {
        tracing::info!(gpu_id = %gpu_id, temp_c = temp.0, dump = "gpu", "state dump");
    }
    for (fan_id, pct) in last_set_pct {
        let rpm = last_rpm.get(fan_id).copied().unwrap_or(0);
        tracing::info!(
            fan_id = %fan_id,
            set_pct = pct.0,
            last_rpm = rpm,
            dump = "fan",
            "state dump"
        );
    }
    tracing::info!(any_fault, dump = "tracker", "state dump");
}

// --------------------------------------------------------------------- //
// --calibrate-fans                                                       //
// --------------------------------------------------------------------- //

fn calibrate_fans_cmd(cfg_path: &Path) -> i32 {
    match run_calibrate(cfg_path) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("calibrate failed: {e:#}");
            1
        }
    }
}

fn run_calibrate(cfg_path: &Path) -> Result<()> {
    let (cfg, _w) =
        config::load(cfg_path).with_context(|| format!("loading config {}", cfg_path.display()))?;
    if daemon_is_active() {
        return Err(anyhow!(
            "tesla_fan_control daemon appears active (systemctl is-active); refusing to calibrate. Stop the unit first."
        ));
    }
    let chip_map = chip::scan().context("scanning /sys/class/hwmon")?;
    let mut fans: Vec<(String, Fan)> = Vec::new();
    for fan_cfg in &cfg.fans {
        let path = chip::resolve(&chip_map, fan_cfg)
            .with_context(|| format!("resolving fan {}", fan_cfg.id))?;
        verify_fan_sysfs(&path, fan_cfg)?;
        let mut fan = Fan::open(
            path,
            fan_cfg.pwm_channel,
            fan_cfg.min_rpm,
            fan_cfg.max_rpm,
            fan_cfg.spin_up_grace_s,
        );
        fan.take_manual_control()
            .with_context(|| format!("take_manual_control for fan {}", fan_cfg.id))?;
        fans.push((fan_cfg.id.clone(), fan));
    }

    let abort = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&abort)).context("register SIGINT")?;
    flag::register(SIGTERM, Arc::clone(&abort)).context("register SIGTERM")?;

    let mut had_abort = false;
    'outer: for (id, fan) in fans.iter_mut() {
        let chip_label = fan_chip_label(&cfg.fans, id);
        println!("--- calibrating fan {id} ({chip_label}) ---");
        let mut lowest_stable: Option<u8> = None;
        let mut time_to_stable_s: u32 = 0;
        let mut pct = 10u8;
        while pct <= 60 {
            if abort.load(Ordering::Relaxed) {
                had_abort = true;
                break 'outer;
            }
            if let Err(e) = fan.set(Pct(pct)) {
                eprintln!("fan {id}: set({pct}%) failed: {e}");
            }
            std::thread::sleep(Duration::from_secs(4));
            let rpm = fan.read_rpm().unwrap_or(0);
            println!("fan {id} ({chip_label}): pwm={pct}%, rpm={rpm}");
            if rpm > 0 && lowest_stable.is_none() {
                lowest_stable = Some(pct);
                time_to_stable_s = 4;
            }
            pct = pct.saturating_add(5);
        }
        if let Some(low) = lowest_stable {
            let suggested_min = low.saturating_add(5).min(100);
            println!(
                "fan {id}: lowest_stable_pct={low}, suggested min_fan_pct={suggested_min}, suggested spin_up_grace_s={time_to_stable_s}"
            );
        } else {
            println!(
                "fan {id}: never produced RPM > 0 in the swept range — manual investigation needed"
            );
        }
    }

    for (id, fan) in fans.iter_mut() {
        if let Err(e) = fan.restore() {
            eprintln!("fan {id}: restore failed: {e}");
        }
    }
    if had_abort {
        return Err(anyhow!("calibration aborted by signal"));
    }
    Ok(())
}

fn fan_chip_label(fans: &[FanConfig], id: &str) -> String {
    if let Some(f) = fans.iter().find(|f| f.id == id) {
        let chip = f
            .chip
            .as_deref()
            .or_else(|| f.hwmon_path.as_ref().and_then(|p| p.to_str()))
            .unwrap_or("?");
        format!("{}/pwm{}", chip, f.pwm_channel)
    } else {
        id.to_string()
    }
}

fn daemon_is_active() -> bool {
    use std::process::Command;
    let Ok(output) = Command::new("systemctl")
        .args(["is-active", "tesla_fan_control"])
        .output()
    else {
        return false;
    };
    output.status.success()
}

// --------------------------------------------------------------------- //
// sd_notify (~20 LOC; no extra crate)                                   //
// --------------------------------------------------------------------- //

fn sd_notify(state: &str) {
    let Ok(socket) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if socket.is_empty() || socket.starts_with('@') {
        // Abstract sockets need a raw byte API; modern systemd uses
        // path-based sockets, so we silently skip the abstract case.
        return;
    }
    use std::os::unix::net::UnixDatagram;
    if let Ok(sock) = UnixDatagram::unbound() {
        let _ = sock.send_to(state.as_bytes(), Path::new(&socket));
    }
}
