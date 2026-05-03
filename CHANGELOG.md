# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
