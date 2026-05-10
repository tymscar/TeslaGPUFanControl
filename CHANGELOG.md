# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Fans stuck at 100 % after resume from system suspend. Kernel hwmon
  drivers re-initialise the PWM controller on resume, resetting
  `pwm{N}_enable` from `1` (manual, daemon-owned) back to the BIOS-auto
  value. The daemon kept writing duty values to `pwm{N}` but the chip
  silently ignored them while BIOS-auto ran the fan at 100 %. `Fan::set`
  / `Fan::set_max` now check `pwm{N}_enable` on every poll and re-arm it
  to `1` if it has drifted, logging a WARN with the observed value and
  re-entering spin-up grace. The startup snapshot used by
  `Fan::restore` on graceful shutdown is preserved unchanged.

### Added

- Per-GPU **Power Limit** enforcement via NVML. Opt-in per GPU
  (`power_limit_enabled`, `power_limit_w`); set at startup and re-asserted
  on a global cadence (`[global] power_limit_check_interval_s`, default
  120 s) so external changes (`nvidia-smi -pl`, driver reloads, other
  tools) are reverted. Designed for PSU-constrained boxes where the
  power supply cannot sustain all GPUs at full board power. See
  ADR-0008 and `docs/CONFIG-TUTORIAL.md` §"Limiting GPU power".
- Asymmetric drift handling: downward drift logs WARN and re-asserts;
  upward drift (PSU exposure) logs ERROR, emits `sd_notify(STATUS=…)`,
  and re-asserts.
- New `[global] power_limit_restore_on_shutdown` (default `false`) —
  controls whether graceful shutdown restores each GPU's limit to the
  NVML driver default. Default preserves PSU protection across
  daemon-down windows. See ADR-0009.
- New `[global] power_limit_validate_max_w` (default `1000`) — operator-
  configurable typo-defence upper bound for `power_limit_w`. Hardware
  whose peak exceeds 1 kW can bump this without a code change.
- New static validation rules **P1–P5** (per-GPU power), runnable via
  `--check-config`. New runtime check **R6** validates the configured
  value against the driver's `power_management_limit_constraints` and
  the initial `set_power_management_limit` call at startup; the daemon
  refuses to start on failure.

### Changed

- **Fault category renamed: `Fault::ThermalBlind` → `Fault::GpuNvml`.**
  The fault now covers any per-GPU NVML operation failure (temperature
  read, power-limit read, power-limit set), not just temperature reads.
  Log field `fault_kind = "thermal_blind"` becomes `"gpu_nvml"`.
  **Operator action:** update any log-monitoring rules / dashboards
  that filter on the old string. The threshold knob name
  (`gpu_fail_threshold`) is unchanged.
- `FaultTracker` now keeps two independent counters per GPU (temperature
  and power) sharing the same `gpu_fail_threshold` and declaring the
  same `Fault::GpuNvml`. Existing temperature-only behaviour is
  byte-identical; the split exists to prevent a dilution bug where
  successful temperature ticks would reset a counter the rarer power
  tick had just incremented.
- `nvml.rs` trait `TempReader` renamed to `NvmlOps` and extended with
  `read_power_limit_w`, `power_limit_constraints_w`, `set_power_limit_w`
  (`&mut self`). `FakeNvml` extended in lockstep.

## [0.1.0] - 2026-05-03

### Added

- Initial release of `tesla_fan_control`, a Linux daemon that drives
  motherboard PWM fans from GPU temperature for passively-cooled compute
  cards, guarded by the kernel hardware watchdog.
- Static musl binary build targeting `x86_64-unknown-linux-musl`.
- INI configuration format with single-pass validator (S/G/F/W/T static
  rules + R runtime rules) accessible via `--check-config`.
- Interactive `--calibrate-fans` sweep to discover safe `min_fan_pct` and
  `spin_up_grace_s` per fan.
- NVML temperature reads, sysfs PWM control with snapshot/restore of the
  `pwm_enable` register, and per-fan RPM health checks with consecutive
  failure thresholds.
- Hardware watchdog integration via `/dev/watchdog` ioctl, armed before
  PWM takeover (ADR-0003).
- systemd service unit, `install.sh`, and `uninstall.sh` for deployment.
- SIGTERM/SIGINT graceful shutdown (restore PWM, disarm watchdog), SIGHUP
  hot-reload of structurally identical configs, SIGUSR1 state dump.
- GitHub Actions CI (fmt, clippy, test, musl release build) and release
  workflow that publishes the static binary plus a SHA-256 checksum on
  `v*` tag pushes.

[Unreleased]: https://github.com/unimatrix099/TeslaGPUFanControl/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/unimatrix099/TeslaGPUFanControl/releases/tag/v0.1.0
