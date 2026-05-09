# tesla_fan_control

A Linux daemon that drives motherboard PWM fans from GPU temperature for
passively-cooled compute cards (e.g. NVIDIA Tesla P100), guarded by the kernel
hardware watchdog. Optionally enforces a per-GPU **Power Limit** for systems
whose PSU cannot sustain all GPUs at full board power (ADR-0008).

The safety model — fail-toward-cooling, watchdog-armed before PWM takeover,
BIOS 100% as the failsafe floor — is captured in
`docs/adr/0001-prioritize-hardware-safety-over-availability.md`,
`docs/adr/0002-bios-100-percent-as-failsafe-floor.md`, and
`docs/adr/0003-init-order-watchdog-before-pwm-takeover.md`. The Power Limit
feature is documented in
`docs/adr/0008-power-limit-as-psu-protection-feature.md` and
`docs/adr/0009-power-limit-not-restored-on-shutdown-by-default.md`.

## Prerequisites

- BIOS configured to drive every PWM header at 100% by default (ADR-0002).
  This is the failsafe floor: if the daemon dies and the watchdog reboots the
  box, the cards are cooled by BIOS until the daemon restarts.
- NVIDIA proprietary driver installed (`libnvidia-ml.so.1` reachable via the
  default loader path).
- `nvidia-persistenced.service` enabled (recommended — keeps driver state
  resident across daemon restarts).
- `/dev/watchdog` available — either a hardware watchdog chip or the
  `softdog` kernel module loaded at boot
  (`echo softdog | sudo tee /etc/modules-load.d/softdog.conf`).
- `RuntimeWatchdogSec=` **must not** be set in `/etc/systemd/system.conf`.
  systemd's runtime watchdog opens `/dev/watchdog` itself; the device has
  single-open semantics and the daemon will fail with `EBUSY` at startup.
- Recommended: `nowayout=0` on the watchdog module. With `nowayout=1` a
  graceful shutdown cannot disarm the timer, and crashing the daemon while
  developing reboots the box.

## Build

The toolchain is pinned by `rust-toolchain.toml` (channel 1.85, target
`x86_64-unknown-linux-musl`).

```
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

`.cargo/config.toml` disables `-crt-static` for the musl target because
NVML (`libnvidia-ml.so.1`) is `dlopen`-loaded at runtime by the
`nvml-wrapper` crate, which a fully-static binary cannot do (musl's
static libc has no `dlopen`). The release artifact is therefore
dynamically linked. The CI release pipeline runs in a container with a
full musl cross-toolchain, producing an artifact linked against musl
libc; a local `cargo build` on a Debian/Ubuntu box without that
toolchain falls back to the system linker and ships a binary linked
against glibc instead. Both work on the target box as long as the
matching libc is present (every modern Linux ships glibc).

## Calibrate (run first)

Before installing, run the interactive calibration sweep to discover the
lowest PWM percent at which each fan reliably spins up and the time it needs
to reach a stable RPM:

```
sudo ./target/x86_64-unknown-linux-musl/release/tesla_fan_control --calibrate-fans
```

It prints suggested `min_fan_pct` and `spin_up_grace_s` values per fan. Copy
those into `config/tesla_fan_control.conf` before installing.

## Validate the config

`--check-config` runs the full static validator (sections S/G/F/W/P/T in
`docs/PLAN.md`) without touching hardware and exits non-zero on any
violation. Safe to run as a non-root user as a pre-flight check.

```
tesla_fan_control --check-config -c config/tesla_fan_control.conf
```

## Power Limit (PSU protection)

Opt-in per GPU. When enabled the daemon enforces a configured power-management
limit at startup and re-asserts it every `power_limit_check_interval_s`
(default 120 s) so external overrides — `nvidia-smi -pl`, driver reloads,
other tools — are reverted. The intended use case is a multi-GPU box whose
PSU cannot sustain every card at full board power simultaneously.

```ini
[gpu:0]
power_limit_enabled = true
power_limit_w       = 260
```

Driver-side feasibility (`power.min_limit` ≤ `power_limit_w` ≤ `power.max_limit`)
is checked at daemon startup (R6); the daemon refuses to start on violation.
Power-limit NVML failures count toward the same `gpu_fail_threshold` as
temperature-read failures and declare the same `Fault::GpuNvml` category, so a
permanently-broken power path eventually triggers the same fault response as
a thermal-blind GPU.

A worked PSU-budget example, drift handling, and the SIGHUP / shutdown
escape hatches are in `docs/CONFIG-TUTORIAL.md` §5 and
`config/examples/03-with-power-limit.conf`.

## Install

`install.sh` copies the binary, default config (only if not already present),
and systemd unit into place, then runs `systemctl daemon-reload`.

```
sudo ./install.sh
sudo systemctl enable --now tesla_fan_control
journalctl -u tesla_fan_control -f
```

## Uninstall

```
sudo ./uninstall.sh
```

The config at `/etc/tesla_fan_control.conf` is preserved. A reboot is
recommended after uninstall — it re-runs BIOS PWM initialization and resyncs
every channel with current BIOS settings, which may have drifted from what
the daemon last wrote.

## Config reference

The canonical configuration schema, validation rules (S/G/F/W/P/T static,
R runtime), and SIGHUP reload policy live in `docs/PLAN.md` under
§Configuration File Format and §Configuration Validation.
`config/tesla_fan_control.conf` is a working template; three worked
examples live under `config/examples/`. The end-to-end calibration walkthrough
is in `docs/CONFIG-TUTORIAL.md`.

## Rebuild and reinstall

```
cargo build --release --target x86_64-unknown-linux-musl
sudo ./install.sh
sudo systemctl restart tesla_fan_control
```

## License

Apache-2.0 — see `LICENSE`. Rationale in
`docs/adr/0005-license-apache-2.md`.
