# TeslaGPUFanControl — Implementation Plan

## Context

The Tesla P100 is a passively-cooled compute GPU (no internal fans). Fans are mounted via 3D-printed adaptors and driven from motherboard PWM headers. The daemon reads GPU temperatures via NVML (runtime `dlopen` of `libnvidia-ml.so.1`, not via `nvidia-smi`) and controls fan PWM via the kernel hwmon sysfs interface. Cooling topology is modelled as **cooling groups** (see CONTEXT.md): N fans cool M GPUs that share an airflow path. The daemon runs as a systemd service guarded by the kernel hardware watchdog.

See ADRs for load-bearing decisions: 0001 (safety > availability), 0002 (BIOS 100% as failsafe floor), 0003 (init order: watchdog before PWM takeover), 0004 (Rust), 0005 (Apache-2.0 license), 0006 (pure-vs-I/O architecture), 0007 (single-threaded blocking, panic = abort).

---

## Language: Rust (see ADR-0004)

Compiled, single static binary against `x86_64-unknown-linux-musl`. NVML accessed via the `nvml-wrapper` crate, which `dlopen`s `libnvidia-ml.so.1` at runtime. Two language properties map directly to safety invariants: `Result<T, E>` enforces "no fault silently swallowed" (ADR-0001) at the compiler, and newtypes (`Celsius`, `Pct`, `Pwm`) prevent unit-confusion bugs.

---

## Project Layout

```
TeslaGPUFanControl/
├── src/
│   ├── main.rs           # Daemon entry, init order, main loop, signal handlers
│   ├── config.rs         # INI parser → Config struct, validation, SIGHUP reload
│   ├── nvml.rs           # nvml-wrapper façade; per-GPU temp reads
│   ├── chip.rs           # Resolve chip name → /sys/class/hwmon/hwmonN at startup
│   ├── fan.rs            # sysfs PWM lifecycle: pwm_enable, pwmN write, fanN_input read
│   ├── curve.rs          # Per-GPU curve type + linear interpolation
│   ├── group.rs          # Cooling-group target = max(clamp(curve(temp_gpu), gpu.min, gpu.max) for gpu in gpus)
│   ├── watchdog.rs       # Hardware watchdog (ioctl WDIOC_*) + internal failure counters
│   ├── logger.rs         # tracing-subscriber to journald + optional file
│   └── units.rs          # Newtypes: Celsius, Pct, Pwm + checked conversions
├── config/
│   └── tesla_fan_control.conf      # Default config (installed to /etc/)
├── systemd/
│   └── tesla_fan_control.service
├── docs/
│   ├── PLAN.md
│   └── adr/                        # Architectural decision records
├── Cargo.toml
├── Cargo.lock                      # committed (binary crate convention)
├── rust-toolchain.toml             # pin compiler version + musl target
├── .github/
│   └── workflows/
│       ├── ci.yml                  # fmt + clippy + test + static build
│       └── release.yml             # tag push → GitHub release with static binary
├── CHANGELOG.md                    # Keep-a-Changelog format
├── install.sh
├── uninstall.sh
├── README.md
└── LICENSE                         # Apache-2.0 (see ADR-0005)
```

---

## Configuration File Format

Installed to `/etc/tesla_fan_control.conf`. INI-style; parser choice (the `rust-ini` crate vs a hand-written ~150-line parser) is decided at implementation time. Supports hot-reload on SIGHUP, with the limitations described in §SIGHUP reload policy.

```ini
[global]
config_version   = 1           # schema version. Daemon refuses to start if it
                               # does not recognise this value. Increment on
                               # breaking schema changes.
poll_interval_ms = 1000        # How often to read temp and update fans
log_level        = info        # debug | info | warn | error
log_file         = /var/log/tesla_fan_control.log   # empty = journald only

[watchdog]
enabled             = true

# Path to the kernel watchdog device.
# The daemon feeds this device every poll loop. If the daemon crashes,
# hangs, or deadlocks, the kernel triggers a hard reboot after timeout_s.
#
# Two backends exist behind /dev/watchdog. The daemon API is identical for
# both — the choice is which kernel module you load on the target box:
#
#   • Hardware watchdog (PREFERRED) — backed by a real chip (BMC, SuperIO,
#     dedicated WDT). Survives kernel hangs, kernel panics, and deadlocks
#     in interrupt context. Requires hardware support and the matching
#     kernel module (iTCO_wdt, nct6795_wdt, sp5100_tco, etc.).
#
#   • softdog (FALLBACK) — kernel module that simulates a watchdog using a
#     kernel timer. Works on any system. CANNOT recover from a hard kernel
#     hang or a panic that disables interrupts — for those cases you need
#     a real hardware watchdog.
#     Enable with:  echo softdog | sudo tee /etc/modules-load.d/softdog.conf
#
# If both backends are loaded, they appear as /dev/watchdog0 and
# /dev/watchdog1 (order depends on module-load timing). Pin to a specific
# one in that case; default /dev/watchdog points at whichever loaded first.
device              = /dev/watchdog

# Watchdog timeout in seconds. The daemon must feed the device within this
# window or the kernel reboots. Set generously above poll_interval_ms.
# The kernel may round this to its nearest supported value.
timeout_s           = 30

# How many consecutive NVML temperature-read failures before declaring a
# thermal-blind fault: all fans → 100%, watchdog stops being fed, kernel
# reboots after timeout_s. Symmetric with fan_fail_threshold below.
gpu_fail_threshold  = 3

# --- GPUs ---
# A GPU has its own curve and its own temperature sensor. It belongs to
# exactly one cooling group (declared at the bottom).

[gpu:0]
nvml_index   = 0              # NVML device index

# Fan curve: comma-separated list of TEMP:SPEED pairs
#   TEMP  = GPU temperature in degrees Celsius (integer)
#   SPEED = target fan speed in percent of maximum (0–100)
#           (NOT a raw PWM value; the daemon converts percent → 0–255 internally)
# Pairs must be sorted by temperature ascending. At least 2 points required.
# Fan speed is linearly interpolated between adjacent points.
# Below the lowest temp point the speed is clamped to the first SPEED value.
# Above the highest temp point the speed is clamped to the last SPEED value.
#
# Example below:
#   GPU at 40 °C → 20 % fan speed
#   GPU at 55 °C → 40 % fan speed
#   GPU at 70 °C → 70 % fan speed
#   GPU at 85 °C → 100 % fan speed
#   GPU at 62 °C → interpolated ≈ 55 % fan speed
curve = 40:20, 55:40, 70:80, 80:100        # P100 throttles at 85 °C, so 100% lands at 80 °C (preventive, not reactive)

min_fan_pct  = 20             # Floor: never drop below this percent (prevents fan stall)
max_fan_pct  = 100            # Ceiling: never exceed this percent

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

# --- Fans ---
# A fan is identified by (hwmon_path, pwm_channel). Each fan declares its
# own RPM health bounds. It belongs to exactly one cooling group.

[fan:f0]
chip          = nct6798        # primary identifier; matched against /sys/class/hwmon/*/name
                              # daemon refuses to start if no chip matches (ADR-0002 BIOS failsafe)
# device_path = /sys/devices/platform/nct6775.656   # fallback for duplicate chips
# hwmon_path  = /sys/class/hwmon/hwmon2             # last-resort literal pin (NOT recommended — index drifts across reboots)
pwm_channel   = 1             # maps to pwm1 / fan1_input under the resolved hwmon path

# RPM health bounds for this fan.
# If the fan reads 0, below min_rpm, or above max_rpm on fan_fail_threshold
# consecutive polls, it is declared a hardware failure:
#   → all fans (across ALL cooling groups) are set to 100 % immediately
#   → the hardware watchdog stops being fed → kernel reboots after timeout_s
# min_rpm protects against a stalled or disconnected fan.
# max_rpm protects against a broken sensor reporting nonsense values.
min_rpm       = 200           # RPM below this (incl. 0) = stalled / dead fan
max_rpm       = 3000          # RPM above this = sensor fault or fan overspeed
fan_fail_threshold = 3        # consecutive out-of-range reads before acting

# Spin-up grace window — RPM below min_rpm during this window after take_manual_control()
# (or after a curve-driven duty increase crossing the spin-up delta) is treated as
# transient (fan still spooling up) and does NOT count toward fan_fail_threshold.
# Increase for slow / large server fans. Run with --calibrate-fans to find the right value.
spin_up_grace_s = 10

[fan:f1]
chip          = nct6798
pwm_channel   = 2
min_rpm       = 200
max_rpm       = 3000
fan_fail_threshold = 3
spin_up_grace_s = 10

# --- Cooling Groups ---
# A cooling group binds a set of fans to a set of GPUs that share an airflow
# path. For each GPU in the group, the curve is evaluated at the GPU's
# current temperature and the result is clamped to that GPU's
# [min_fan_pct, max_fan_pct] band. The group's target fan % is the max()
# of those bounded per-GPU values, applied uniformly to every fan in the
# group. Any single GPU's min_fan_pct can act as a group-wide floor.
#
# Examples below model: fan f0 cools only GPU 0; fan f1 cools both GPU 1 and 2
# via a Y-duct adaptor.

[group:main]
gpus = 0
fans = f0

[group:shared]
gpus = 1, 2
fans = f1
```

---

## Configuration Validation

The daemon refuses to start on any validation failure (ADR-0002 BIOS failsafe handles the fallback). Validation runs in two phases:

- **Static phase** — all rules in S/G/F/W/T below. No I/O, no hardware. Runnable via `--check-config` as a non-root pre-flight.
- **Runtime phase** — rules R1–R5. Requires root, hardware, and NVML. Runs only at daemon startup, not in `--check-config`.

Both phases **accumulate every violation before exiting**. Validation is a single pass, then the daemon prints every problem grouped by section and exits with code 1. (This matters in practice: a config with 5 typos shouldn't make the operator restart the validator 5 times to find them all.)

### S — Structural integrity

| # | Rule | Rationale |
|---|---|---|
| S1 | ≥ 1 `[gpu:*]` block | Nothing to monitor otherwise |
| S2 | ≥ 1 `[fan:*]` block | Nothing to control otherwise |
| S3 | ≥ 1 `[group:*]` block | No fan↔GPU mapping otherwise |
| S4 | Every `[gpu:*]` is referenced by exactly one group | Orphan GPU = monitored but not cooled (silent overheat risk) |
| S5 | Every `[fan:*]` is referenced by exactly one group | Orphan fan = controlled with no thermal source. The config describes reality; "I'll wire this later" belongs in version control, not in a running config. |
| S6 | Each group has ≥ 1 GPU AND ≥ 1 fan | Half-empty groups do nothing |
| S7 | All ID references in groups point to existing `[gpu:*]` / `[fan:*]` blocks | Typo guard |
| S8 | `nvml_index` is unique across `[gpu:*]` blocks | Two configs for the same GPU = ambiguous curves |
| S9 | `(chip + device_path? + pwm_channel)` tuple is unique across `[fan:*]` blocks | Two configs for the same fan = competing writes |

### G — Per-GPU

| # | Rule | Rationale |
|---|---|---|
| G1 | Curve has 2..=10 points | Lower bound = math; upper bound = parser sanity |
| G2 | Curve points sorted ascending by temperature | Interpolation requires this |
| G3 | Curve temperatures are unique | Avoids divide-by-zero in interpolation |
| G4 | Curve percentages are monotonic non-decreasing | A descending curve would *cause* overheating |
| G5 | Curve temperatures ∈ [0, 110] °C | Outside is almost certainly a typo |
| G6 | Curve percentages ∈ [0, 100] | Definitional |
| G7 | `min_fan_pct ≤ max_fan_pct` | Empty band would be unreachable |
| G8 | `min_fan_pct ∈ [0, 100]`, `max_fan_pct ∈ [0, 100]` | Definitional |

### F — Per-fan

| # | Rule | Rationale |
|---|---|---|
| F1 | `min_rpm ≥ 0` and `max_rpm > min_rpm` | Sane RPM band |
| F2 | `fan_fail_threshold ≥ 1` | Zero would fault on first transient |
| F3 | `spin_up_grace_s ≥ 0` | Definitional |
| F4 | `pwm_channel ≥ 1` | hwmon convention |
| F5 | At least one of `chip`, `device_path`, `hwmon_path` is set | Otherwise the fan can't be located |

### W — Watchdog & global

| # | Rule | Rationale |
|---|---|---|
| W1 | `[watchdog].device` non-empty | Required to open the device |
| W2 | `timeout_s ∈ [5, 600]` | Below 5s flirts with feed-window misses; above 10min is silly |
| W3 | `gpu_fail_threshold ≥ 1` | Same as F2 |
| W4 | `poll_interval_ms ∈ [100, 10000]` | Too fast hammers sysfs; too slow makes thresholds meaningless |
| W5 | `poll_interval_ms × max(thresholds) < timeout_s × 500` | Leave headroom so a fault has time to propagate before the watchdog independently fires |
| W6 | `log_level ∈ {debug, info, warn, error}` | Enum |
| W7 | `[global].config_version` is set and the daemon recognises the value (currently only `1`) | Schema mismatch fails fast rather than producing baffling errors deep in parsing |

### T — Spell/typo detection

| # | Rule | Action |
|---|---|---|
| T1 | Unknown section header (e.g. `[GPU:0]` capitalised) | **Error** — `[gpu:0]` is the canonical spelling |
| T2 | Unknown key within a known section | **Warn** — could be a future field; logged but not fatal |

### R — Runtime checks (daemon startup only)

| # | Check | On failure |
|---|---|---|
| R1 | Each configured `chip` resolves to a `hwmonN` directory | Refuse to start; log which chip wasn't found and which chips *were* available |
| R2 | Each `(hwmon_path, pwm_channel)` exposes `pwmN`, `pwmN_enable`, and `fanN_input` files | Refuse to start; name the missing path |
| R3 | NVML init succeeds; every configured `nvml_index` is present | Refuse to start; log the out-of-range index and the actual device count |
| R4 | `/dev/watchdog` opens without `EBUSY` | Refuse to start; suggest checking `RuntimeWatchdogSec=` and `lsof /dev/watchdog` |
| R5 | `pwm_enable` for each fan is writable | Refuse to start; usually means not running as root |

---

## Module Breakdown

### `units.rs`
- Newtypes: `Celsius(i16)`, `Pct(u8)` (0..=100), `Pwm(u8)` (0..=255).
- Single conversion: `impl From<Pct> for Pwm` → `(pct as u16 * 255 / 100) as u8`. No reverse direction; PWM-to-percent is meaningless in this domain.
- `Pct::clamp(min, max)` for `min_fan_pct` / `max_fan_pct` enforcement.

### `config.rs`
- INI parsing — implementation chooses between the `rust-ini` crate (mature, ~1k LOC dep) and a hand-written ~150-line parser (no extra dep, total control over error messages). Decide at the start of Phase 1 based on how much custom error reporting the validation rule set ends up needing.
- Top-level `Config` struct holding `global`, `watchdog`, `Vec<Gpu>`, `Vec<Fan>`, `Vec<CoolingGroup>`.
- Validation at parse time: curve points sorted, ≥2 points, all groups reference existing GPU and Fan IDs, every GPU and Fan belongs to exactly one group, range checks on every percent / RPM / threshold.
- SIGHUP reload: parse + validate fully, then hot-swap. If new config differs structurally (different number of GPUs, different fan IDs), reload is rejected and the existing config remains in effect — log a critical error. (Restart the service to apply structural changes.)

### `nvml.rs`
- Wraps `nvml_wrapper::Nvml`. Holds the singleton `Nvml` handle and a `Vec<Device<'_>>` keyed by `nvml_index`.
- `read_temp(idx) -> Result<Celsius, NvmlError>` — calls `device.temperature(TemperatureSensor::Gpu)` and **applies a sanity-bounds check**: any value outside `[5, 110] °C` is treated as if NVML had errored (returns `Err(NvmlError::OutOfBounds)`). This guards against driver bugs or sensor misreads that return `0` (which would silently idle the fan while the GPU overheats) or absurdly high values (which would peg the fan but indicate the temp itself is unreliable). The out-of-bounds error counts toward `gpu_fail_threshold` like any other read failure.
- Init failure is fatal at startup per ADR-0003 step 3. The realistic failure modes are: `libnvidia-ml.so.1` not present (driver not installed), driver present but its kernel module not loaded, or the configured `nvml_index` exceeds the device count. `nvidia-persistenced` not running is *not* a failure mode — its absence only adds latency to the next `nvmlInit` after idle, which we avoid by keeping NVML open continuously.

### `chip.rs`
- At startup: scan `/sys/class/hwmon/*/name`, build a map `chip_name -> hwmonN_path`.
- For each `[fan:*]` config block: resolve `chip` to a path, optionally further constrained by `device_path`. If `hwmon_path` is set explicitly, skip resolution (escape hatch).
- Refuse to start if any configured chip is not present (ADR-0002 — BIOS failsafe handles the fallback).

### `fan.rs`
- `Fan` struct: holds resolved `hwmonN` path, `pwm_channel`, RPM bounds, file descriptors for `pwmN` and `fanN_input`.
- `Fan::take_manual_control()` — first reads and stores the current `pwm_enable` value (the *startup snapshot*), then writes `pwm_enable = 1`. Called only after the watchdog is armed (ADR-0003).
- `Fan::set(Pct)` — converts to `Pwm`, writes to `pwmN`.
- `Fan::set_max()` — writes `Pwm(255)` directly, bypassing curve / clamp logic. The fault-handling primitive.
- `Fan::read_rpm() -> Result<u32>` — reads `fanN_input`.
- `Fan::check_health(rpm, min, max, in_spin_up) -> FanHealth { Ok | SpinUp | Stalled | Overspeed }` — pure function; tested in isolation. `SpinUp` is returned in place of `Stalled` while the fan is in its spin-up grace window; the `FaultTracker` ignores `SpinUp` instead of counting it.
- Spin-up state machine: enter spin-up on `take_manual_control()`, or when a `set(Pct)` call increases duty by more than 20 percentage points (`SPIN_UP_DELTA_PCT`, hard-coded constant in v0.1; promote to per-fan config field if a real fan ever needs different). Leave spin-up the moment RPM > `min_rpm` for one poll. If RPM never crosses `min_rpm` within `spin_up_grace_s`, exit spin-up state and the next poll's `Stalled` reading is real.
- `Fan::restore()` — writes the *startup snapshot* of `pwm_enable` back to the register on graceful shutdown. Restores whatever value the daemon found there (BIOS auto, chip thermal cruise, manual-from-prior-tool, etc.) without needing a per-chip mapping table. Called *before* disarming the watchdog (ADR-0003 inverse invariant). See "Fan restore policy" below.

**Fan restore policy.** `pwm_enable=2` is *not* universally "auto" across chip families:

| Chip family | What `pwm_enable=2` actually means |
|---|---|
| `nct6775` / `nct6798` (Nuvoton) | Thermal cruise — chip-internal thermistor drives the fan, **not** BIOS state |
| `it87` (ITE) | Automatic mode (closest to "BIOS state") |
| `f71889ed` (Fintek) | Auto |

To stay chip-agnostic and preserve ADR-0002's BIOS failsafe semantics, the daemon **snapshots** the original `pwm_enable` value at `take_manual_control()` time and restores that exact value on graceful shutdown. This works for every chip family the kernel supports, because we don't interpret the value — we just put it back.

Uninstall recommendation: after `uninstall.sh` removes the service, reboot the box once. This re-runs BIOS PWM initialization and resyncs every channel with current BIOS settings (which may have been changed since the last daemon startup).

**RPM read timing note:** RPM is read *after* each PWM write. After a large speed increase the fan takes time to spin up; the `fan_fail_threshold` consecutive-failure window absorbs transient low readings during ramp-up and prevents false alarms.

### `curve.rs`
- `Curve` holds `Vec<(Celsius, Pct)>`, max 10 points, sorted ascending by temp.
- `Curve::evaluate(temp: Celsius) -> Pct` — linear interpolation, clamped at the edges.
- Construction validates ≥2 points and monotonic non-decreasing percent.

### `group.rs`
- `CoolingGroup { gpus: Vec<GpuId>, fans: Vec<FanId> }`.
- `CoolingGroup::target_pct(temps: &HashMap<GpuId, Celsius>, gpus: &HashMap<GpuId, &Gpu>) -> Pct` — for each GPU in the group, evaluate the curve at its current temperature *and clamp the result to that GPU's `[min_fan_pct, max_fan_pct]`*; then take the `max()` of those bounded values. Clamp-per-GPU happens **before** the max(), not after — there is no group-level clamp.
  - Consequence: any single GPU's `min_fan_pct` becomes a group-wide floor whenever that GPU's bounded contribution is the loudest. `max_fan_pct` caps a GPU's *own* contribution but does not cap what another GPU in the same group can request — `max_fan_pct` is a noise/wear hint, not a thermal hard limit.

### `watchdog.rs`

**Hardware watchdog (`/dev/watchdog`):**
- `HardwareWatchdog::open(device: &Path, timeout_s: u32)` — open device, `ioctl(WDIOC_SETTIMEOUT)`, log the actual timeout the kernel accepted (it may round).
- `feed()` — single byte write; called every successful poll iteration.
- `disarm_and_close()` — writes the magic byte `'V'` then closes (prevents reboot on graceful exit). If the process dies without calling this, the kernel reboots after `timeout_s`.
- If `[watchdog].enabled = false`: the type still exists but every method is a no-op. The daemon still runs; it just has no kernel reboot capability. (Useful for development on non-target machines, but never the production configuration.)

**Internal failure logic (`FaultTracker`):**

GPU temperature failures:
- Per-GPU consecutive NVML failure counter.
- `tick_gpu(gpu_id, read_ok)`:
  - Increment / reset counter.
  - If counter > `gpu_fail_threshold`: declare `Fault::ThermalBlind { gpu_id }`. The caller (main loop) sees the fault, calls `Fan::set_max()` on every fan across every group, and stops feeding the hardware watchdog.

Fan RPM hardware failures:
- Per-fan consecutive out-of-range RPM counter.
- `tick_fan(fan_id, FanHealth)`:
  - `Ok` → reset counter to zero.
  - `SpinUp` → no-op (counter neither incremented nor reset; the fan is mid-spin-up, neither healthy nor faulted).
  - `Stalled | Overspeed` → increment counter; log RPM and fault type.
  - If counter > `fan_fail_threshold`: declare `Fault::FanHardware { fan_id, last_rpm, kind }`. Same response as the GPU path above.

`FaultTracker::any_fault() -> bool` — used by main loop to decide whether to feed the hardware watchdog this iteration.

**systemd notify (optional but recommended):**
- `sd_notify(0, "WATCHDOG=1")` each successful loop iteration. Implemented by writing directly to `$NOTIFY_SOCKET` (no extra crate; ~20 lines). Complements the hardware watchdog: systemd restarts the service if it becomes unresponsive within `WatchdogSec`, which is faster than waiting for the kernel reboot.

### `logger.rs`
- `tracing` + `tracing-subscriber` with the `journald` layer (production) and an optional file layer.
- Log level via config; default `info`.
- Compile-time max level pinned to `info` in release builds (`tracing` features `release_max_level_info`) — `trace`-level events vanish completely in release, no runtime cost.
- Structured fields on every event — see Logging Conventions below.

### Logging conventions

| Level | What goes here |
|---|---|
| `debug` | Per-poll temp / PWM / RPM readings. Opt-in for troubleshooting only — at 1 Hz polling on a 3-GPU box this is ~30 evts/s. |
| `info` | State transitions: daemon ready, config reloaded, SpinUp → Ok, SIGUSR1 dumps, calibration progress. Default level. |
| `warn` | Recoverable anomalies: per-poll fault-counter increments (RPM out of range), spin-up grace expired without RPM crossing `min_rpm`, NVML transient errors. |
| `error` | **Declared faults**: `ThermalBlind`, `FanHardware`. Startup validation failures. The "stopping watchdog feed" pre-reboot message. |

Standard structured fields (set as `tracing` fields, not interpolated into the message text — searchable via `journalctl _SYSTEMD_UNIT=tesla_fan_control.service FAN_ID=f0`):

| Field | Where | Type |
|---|---|---|
| `gpu_id` | any GPU-related event | `&str` (the `[gpu:0]` ID) |
| `fan_id` | any fan-related event | `&str` |
| `group_id` | any group-target event | `&str` |
| `temp` | NVML reads | `i16` (Celsius) |
| `pct` | curve / target / set events | `u8` |
| `rpm` | RPM reads | `u32` |
| `pwm` | sysfs writes | `u8` |
| `fault_kind` | declared faults | `"thermal_blind"` \| `"fan_hardware"` |

Style:

```rust
// good — searchable, every value is a field
warn!(fan_id = %f.id, rpm = current_rpm, threshold = f.min_rpm, "RPM below threshold");

// bad — values baked into the message text
warn!("fan {} RPM {} below threshold {}", f.id, current_rpm, f.min_rpm);
```

### Fault-storm rate limiting

journald rate-limits log records by default (`RateLimitIntervalSec=30s`, `RateLimitBurst=10000`). Under a fault storm — e.g. several fans fail at once, or startup validation produces dozens of messages — important records can be silently dropped. For this daemon the pre-reboot fault message is the **single most important log line we ever emit**; it must never be rate-limited.

Systemd unit overrides per-service rate limiting:

```ini
[Service]
LogRateLimitIntervalSec=0
LogRateLimitBurst=0
```

The volume risk is bounded — even under maximum pessimism (every poll declares a fault) we are at ~10 evts/s, well below journald's actual capacity. The rate limit exists to protect against runaway services emitting millions/s; we are not such a service.

### `main.rs`
- Single-threaded, blocking. No async, no threads. See ADR-0007.
- CLI via `clap`: `-c <config>`, `-f` (foreground), `--version`, `--check-config` (validate and exit), `--calibrate-fans` (interactive sweep — see Calibration below).
- Init order (ADR-0003): config → chip resolution → NVML → hardware watchdog → take manual PWM control → `sd_notify(READY=1)`.
- Signal handlers via `signal-hook`: SIGTERM/SIGINT → graceful shutdown; SIGHUP → config reload; SIGUSR1 → dump state.
- Graceful shutdown order: stop main loop → `Fan::restore()` for every fan (writes back the `pwm_enable` snapshot taken at startup) → `HardwareWatchdog::disarm_and_close()` → exit 0.
- **Main loop:**
  1. For each GPU: `nvml::read_temp(gpu)` → `fault_tracker.tick_gpu()`; cache temp.
  2. For each cooling group:
     a. `target_pct = group.target_pct(&temps, &gpus)`.
     b. For each fan in the group: `fan.set(target_pct)`, then `fan.read_rpm()` → `fan.check_health()` → `fault_tracker.tick_fan()`.
  3. If `!fault_tracker.any_fault()`: `hw_watchdog.feed()` + `sd_notify("WATCHDOG=1")`.
     If any fault: do not feed; the kernel countdown is now active.
  4. Sleep `poll_interval_ms` via `std::thread::sleep`.

---

## Fan PWM Interface (Linux sysfs)

```
/sys/class/hwmon/hwmonX/pwmN          write 0–255 to set speed
/sys/class/hwmon/hwmonX/pwmN_enable   write 1=manual, 2=auto
/sys/class/hwmon/hwmonX/fanN_input    read RPM (integer)
```

Conversion: `pwm_value = (pct * 255) / 100`

---

## Watchdog Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                      Every poll loop                          │
│                                                               │
│  ┌─ GPU ─────────────────────────────────────────────────┐   │
│  │  NVML read OK?  ──no──→ increment GPU failure counter  │   │
│  │       │                 counter > gpu_fail_threshold?  │   │
│  │      yes                └──yes──→ ALL fans → 100%      │   │
│  │                                   declare THERMAL_BLIND │   │
│  └───────┼────────────────────────────────────────────────┘   │
│          │                                                     │
│  ┌─ Fan (per fan) ────────────────────────────────────────┐   │
│  │  Read RPM from sysfs                                    │   │
│  │  RPM == 0 or < min_rpm  →  FAN_STALLED                 │   │
│  │  RPM > max_rpm          →  FAN_OVERSPEED               │   │
│  │  otherwise              →  FAN_OK                      │   │
│  │                                                         │   │
│  │  FAN_OK  → reset fan failure counter                    │   │
│  │  fault   → increment fan failure counter                │   │
│  │            counter > fan_fail_threshold?                │   │
│  │            └──yes──→ ALL fans → 100%                   │   │
│  │                      declare HARDWARE_FAULT             │   │
│  └─────────────────────────────────────────────────────────┘  │
│                                                               │
│  any_fault()? ──no──→ feed /dev/watchdog + sd_notify        │
│       │                                                       │
│      yes → watchdog NOT fed → kernel countdown active         │
│       ↓                                                       │
│  timeout_s expires → kernel hard reboot                       │
│  (no userspace code needed — works even if process is hung)   │
└──────────────────────────────────────────────────────────────┘

Clean shutdown path:
  SIGTERM → stop main loop
         → restore each fan's pwm_enable snapshot (BIOS regains PWM control)
         → write 'V' to /dev/watchdog → close fd → no reboot

systemd layer (separate, complementary):
  WatchdogSec=30s + sd_notify("WATCHDOG=1") → service restart
  (restarts the process; kernel watchdog handles full reboot)

Kernel module required if no hardware watchdog chip:
  modprobe softdog   # creates /dev/watchdog via software timer
```

---

## systemd Service File

```ini
[Unit]
Description=Tesla GPU Fan Control Daemon
After=nvidia-persistenced.service

[Service]
Type=notify
ExecStart=/usr/local/sbin/tesla_fan_control
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=5s
WatchdogSec=30s
PIDFile=/run/tesla_fan_control.pid

# Disable per-service journal rate limiting — pre-reboot fault messages
# are the single most important records this daemon emits and must
# never be dropped. Volume is bounded (~10 evts/s worst case).
LogRateLimitIntervalSec=0
LogRateLimitBurst=0

# Sandboxing — the daemon needs root for sysfs and /dev/watchdog access,
# but everything else can be locked down. Free hardening, ~zero runtime cost.
ProtectSystem=strict                                # / and /usr read-only
ProtectHome=true                                    # /home, /root invisible
PrivateTmp=true                                     # private /tmp
NoNewPrivileges=true                                # cannot gain caps via setuid
ProtectKernelLogs=true                              # cannot read kernel ring buffer
ProtectControlGroups=true                           # cannot manipulate cgroups
RestrictAddressFamilies=AF_UNIX AF_NETLINK          # no network sockets
                                                    # (AF_UNIX kept for sd_notify)
ReadWritePaths=/sys/class/hwmon /dev /run           # the only paths we write

[Install]
WantedBy=multi-user.target
```

---

## Build

```toml
# Cargo.toml (sketch)
[package]
name    = "tesla_fan_control"
version = "0.1.0"
edition = "2021"

[dependencies]
nvml-wrapper      = "0.12"
nix               = { version = "0.29", features = ["ioctl", "fs"] }
clap              = { version = "4", features = ["derive"] }
tracing           = { version = "0.1", features = ["release_max_level_info"] }
                                   # compile-time pin: trace events vanish in release
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-journald  = "0.3"
signal-hook       = "0.3"
rust-ini          = "0.21"         # OR hand-written; decide at start of Phase 1
thiserror         = "1"            # module-level error enums
anyhow            = "1"            # main.rs only — fatal errors about to be logged + exited

[dev-dependencies]
tempfile          = "3"            # fake sysfs trees for fan.rs / chip.rs tests

[profile.release]
lto           = "fat"
codegen-units = 1
strip         = "symbols"
panic         = "abort"          # ADR-0007: any panic terminates → systemd restart → kernel watchdog if needed

[lints.clippy]
unwrap_used      = "deny"        # ADR-0007: no .unwrap() outside tests
expect_used      = "warn"        # only in main.rs init with invariant-naming messages
indexing_slicing = "warn"        # prefer .get()
panic            = "warn"        # explicit panic!() requires a justifying comment
```

Release build:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# → target/x86_64-unknown-linux-musl/release/tesla_fan_control  (single static binary)
```

Install targets are provided by `install.sh` (binary, default config, systemd unit, daemon-reload). No Makefile shim — Cargo is the build system.

---

## Signal Handling

| Signal  | Action                                                                  |
|---------|-------------------------------------------------------------------------|
| SIGTERM | Graceful shutdown (see lifecycle below): restore fans, disarm watchdog, exit 0 |
| SIGINT  | Same as SIGTERM                                                         |
| SIGHUP  | Re-read config; hot-swap if structurally identical, else reject (see below) |
| SIGUSR1 | Dump current temps, fan speeds, RPMs, fault counters to log             |

### SIGHUP reload policy

The daemon deliberately limits what SIGHUP can change. This falls out of ADR-0003 (the watchdog must be open continuously while PWM is under our control), which means we cannot atomically re-resolve hwmon paths or re-open `/dev/watchdog` mid-flight.

**Hot-reloadable** (data-only swap, applied next poll iteration):
- Per-GPU curve points, `min_fan_pct`, `max_fan_pct`.
- `gpu_fail_threshold`, `fan_fail_threshold`.
- Per-fan `min_rpm`, `max_rpm`.
- `log_level`, `poll_interval_ms`.

**Not hot-reloadable** (require `systemctl restart`):
- `chip`, `device_path`, `hwmon_path`, `pwm_channel` (would need to close and re-open sysfs PWM fds).
- `nvml_index` (NVML re-init is heavy and racy).
- Adding / removing / renaming GPUs, Fans, or Cooling Groups (changes `FaultTracker` shape).
- Watchdog `device` or `timeout_s` (cannot be re-opened atomically).

On SIGHUP the daemon parses the new config, validates it, and computes a structural diff against the running config. If the diff is empty it hot-swaps the data fields and logs `config reloaded`. If the diff is non-empty it logs a critical error naming the fields that require restart, leaves the running config in place, and continues.

### Watchdog and fan lifecycle

Symmetric init / shutdown order (see ADR-0003 for the underlying invariant):

**Startup:**
1. Parse + validate config.
2. Resolve chips → hwmon paths.
3. Initialize NVML, verify configured GPUs are present.
4. **Open `/dev/watchdog`, arm with `WDIOC_SETTIMEOUT`.**
5. **For each fan: `pwm_enable = 1` (take manual control).**
6. `sd_notify(READY=1)`, enter main loop.

**Shutdown (SIGTERM/SIGINT):**
1. Stop main loop.
2. **For each fan: write the snapshotted `pwm_enable` value back (see Fan Restore Policy).**
3. **Write `'V'` to `/dev/watchdog` and close the fd (disarm).**
4. `nvmlShutdown()`, exit 0.

The two starred steps in startup are reversed in shutdown — the watchdog stays open through every transition that involves manual PWM control, and PWM control is released *before* the watchdog is disarmed.

**SIGKILL / crash:** the daemon never gets to run shutdown. The kernel closes the `/dev/watchdog` fd as part of process teardown; with `nowayout=0` (documented prerequisite) this auto-disarms the timer. PWM registers retain their last-written value until either (a) a new daemon instance takes over via `Restart=always`, or (b) the watchdog `timeout_s` expires and the kernel reboots — at which point BIOS 100% (ADR-0002) takes effect.

---

## Build Phases (Implementation Order)

1. **Phase 1 — Scaffold**: `cargo init`, `units.rs` newtypes, `logger.rs`, `config.rs` skeleton + INI parsing, module stubs.
2. **Phase 2 — Core I/O**: `nvml.rs` (read temp), `chip.rs` (resolve hwmon), `fan.rs` (PWM lifecycle + RPM). Each module gets unit tests using fake sysfs trees in `tempfile`.
3. **Phase 3 — Logic**: `curve.rs` (interpolation, table-driven tests), `group.rs` (max-of-curves), `watchdog.rs` internal `FaultTracker`. Pure logic — no I/O — fully unit-tested.
4. **Phase 4 — Daemon**: `main.rs` init order (ADR-0003), hardware watchdog ioctl, signal handlers, `sd_notify` socket writes, graceful shutdown ordering.
5. **Phase 5 — Polish**: `install.sh`, `uninstall.sh`, systemd unit, README, LICENSE, integration test against `softdog`.

---

## Verification

```bash
# Build (single static binary)
cargo build --release --target x86_64-unknown-linux-musl

# Validate the config without running the daemon
sudo ./target/x86_64-unknown-linux-musl/release/tesla_fan_control \
    --check-config -c config/tesla_fan_control.conf

# Test run in foreground with verbose logging (RUST_LOG honoured by tracing-subscriber)
sudo RUST_LOG=debug ./target/x86_64-unknown-linux-musl/release/tesla_fan_control \
    -f -c config/tesla_fan_control.conf

# Watch fan PWM and RPM live (substitute the resolved chip's hwmon path)
HWMON=$(grep -l '^nct6798$' /sys/class/hwmon/*/name | xargs dirname)
watch -n1 "cat $HWMON/pwm1; cat $HWMON/fan1_input"

# Simulate NVML failure: unbind/rebind the nvidia kernel module while the
# daemon is running (`echo 0000:XX:00.0 | sudo tee /sys/bus/pci/drivers/nvidia/unbind`)
# — confirm gpu_fail_threshold ticks, fans go to 100%, and the kernel
# reboots after timeout_s.

# Install and run as service
sudo ./install.sh
sudo systemctl enable --now tesla_fan_control
journalctl -u tesla_fan_control -f
```

## Calibration (`--calibrate-fans`)

A standalone CLI mode that helps the operator pick safe values for `min_fan_pct` and `spin_up_grace_s` *before* running the daemon as a service. Run once after install and after any fan hardware change.

Behavior:

1. Refuses to run if the daemon service is active (`systemctl is-active tesla_fan_control` → exit). Concurrent PWM writes from two processes would produce nonsense readings and risk leaving the chip in a weird state.
2. Loads config, resolves chips, takes manual control of every configured fan. **Does not open `/dev/watchdog`** — calibration is a foreground operator task, not a safety-critical one. (The fans are at 100% from BIOS prior to start, and graceful Ctrl-C restores `pwm_enable` snapshots; no watchdog needed.)
3. For each fan: sweep PWM from 10% to 60% in 5%-step increments, holding each step for 4 seconds. Log measured RPM after each settle period. Identify the lowest PWM at which RPM > 0 and stable.
4. After the sweep: print a per-fan recommendation:
   ```
   fan f0 (nct6798/pwm1): lowest stable PWM = 25%, suggest min_fan_pct = 30 (5% headroom)
   fan f0 (nct6798/pwm1): observed spin-up time at 30% from cold = 6.2s, suggest spin_up_grace_s = 10
   ```
5. Restores `pwm_enable` snapshots and exits 0. Writes nothing to disk — recommendations are for the operator to copy into the config manually.

The mode is read-only with respect to disk state and idempotent. Safe to run repeatedly.

---

## Testing

The architecture splits cleanly into pure and I/O modules (ADR-0006), and the test strategy follows that split.

### Workflow: where TDD applies, where it doesn't

TDD (red-green-refactor) is mandated where the test surface is high-leverage and naturally precedes implementation; for I/O glue and the composition root it's left as test-after, where writing tests first tends to produce contrived setups before the real I/O shape is known.

| Module | Workflow | Why |
|---|---|---|
| `units.rs`, `curve.rs`, `group.rs` | **Strict TDD** — red-green-refactor each function | Pure, deterministic; the test *is* the spec. Fastest feedback loop in the project. |
| `watchdog::FaultTracker` | **Strict TDD** | Most safety-critical pure code; branch coverage is the goal and TDD makes that natural. |
| `config::validation` | **Strict TDD, one test per rule** | The S/G/F/W/T validation table maps one-to-one onto `#[test]` functions. Each rule's failing test comes first, then the validator code grows to make it pass. |
| `fan.rs`, `chip.rs` | **Spike then test** — write the API and one happy-path tempfile test, then iterate | TDD against sysfs tends to produce contrived test trees before the real I/O shape is known. |
| `nvml.rs`, `watchdog::HardwareWatchdog` | **Test after** | Thin shims; integration tests against `FakeNvml` and `softdog` are what matter, and those don't fit a red-green loop well. |
| `main.rs` | **No TDD** — smoke tests written last | Composition root; TDD doesn't apply meaningfully. |

The strict-TDD modules cover every interesting logical decision in the system. By the end of Phase 3 (Logic), the validation rule set, fan curve, group target computation, and fault tracker should each have ≥ 1 test per code path before any of them have implementations.

### Pure modules — in-module unit tests

`units.rs`, `curve.rs`, `group.rs`, `watchdog::FaultTracker`, `config::validation`. Tested via `#[cfg(test)] mod tests`. No fixtures, no mocks, just call functions with values and assert.

```rust
// curve.rs
#[test]
fn curve_interpolates_linearly_between_points() {
    let c = Curve::new(vec![(40, 20), (60, 60)]).unwrap();
    assert_eq!(c.evaluate(50), Pct(40));   // halfway
    assert_eq!(c.evaluate(30), Pct(20));   // clamp low
    assert_eq!(c.evaluate(70), Pct(60));   // clamp high
}
```

The `FaultTracker` is the most safety-critical pure module — every fault path must have a test:

- NVML failure for `gpu_fail_threshold` consecutive polls → declares `ThermalBlind`.
- RPM stalled past `fan_fail_threshold` → declares `FanHardware`.
- `SpinUp` health does *not* increment the counter.
- A single `Ok` reading mid-fault resets the counter.
- `any_fault()` returns true exactly when a fault is declared.

Aim for branch coverage on `FaultTracker`. The rest of the pure modules can target line coverage.

### I/O modules — `tempfile` for sysfs, trait for NVML, real device for watchdog

**`fan.rs`, `chip.rs` — fake sysfs via `tempfile`.** Linux sysfs is just files; we do not need to mock anything.

```rust
let dir = tempdir()?;
fs::write(dir.path().join("name"), "nct6798")?;
fs::write(dir.path().join("pwm1"), "0")?;
fs::write(dir.path().join("pwm1_enable"), "5")?;
fs::write(dir.path().join("fan1_input"), "1500")?;

let chip = Chip::scan_at(dir.path())?;
let mut fan = Fan::open(&chip, 1)?;
fan.take_manual_control()?;
assert_eq!(fs::read_to_string(dir.path().join("pwm1_enable"))?.trim(), "1");
fan.restore()?;
assert_eq!(fs::read_to_string(dir.path().join("pwm1_enable"))?.trim(), "5");
```

The `Chip::scan_at(path)` form is what makes this testable — production code calls `Chip::scan()` (defaults to `/sys/class/hwmon/`), tests call `Chip::scan_at(tempdir)`.

**`nvml.rs` — `TempReader` trait + `FakeNvml`.**

```rust
trait TempReader {
    fn read_temp(&self, idx: u32) -> Result<Celsius, NvmlError>;
}

// production
struct NvmlReader { /* nvml-wrapper handle */ }
impl TempReader for NvmlReader { /* ... */ }

// tests
struct FakeNvml { temps: HashMap<u32, Celsius>, fail_after: Option<u32> }
impl TempReader for FakeNvml { /* ... */ }
```

This is the only trait we introduce purely for testability — and only because tempfs cannot fake an NVIDIA library. Per ADR-0006 we don't pre-introduce traits without a real second implementation; here `FakeNvml` *is* the second implementation.

**`watchdog::HardwareWatchdog` — integration test against `softdog`.**

```rust
#[test]
#[ignore]   // requires modprobe softdog; CI may not have it. Run locally before release.
fn watchdog_open_feed_disarm_lifecycle() {
    let wd = HardwareWatchdog::open(Path::new("/dev/watchdog"), 5).unwrap();
    for _ in 0..10 {
        wd.feed().unwrap();
        thread::sleep(Duration::from_secs(1));
    }
    wd.disarm_and_close().unwrap();
    // If we got here without rebooting, the lifecycle is correct.
}
```

The `#[ignore]` keeps it out of the default `cargo test` run. The README documents `cargo test -- --ignored` as a pre-release check, run locally on a box with `softdog` loaded.

### Composition root — smoke test, not unit test

`main.rs` is not unit-tested. Instead:

- `cargo test --test smoke` builds the binary and runs `--check-config` against the example config (`config/tesla_fan_control.conf`); asserts exit 0.
- The smoke test also runs `--check-config` against a fixture per validation rule (S4, G2, F1, etc.), asserts exit 1, and asserts the error output mentions the specific rule code.

This catches CLI wiring regressions and proves the validator covers what it claims to cover.

### What we don't test

- `tracing-subscriber` formatting — third-party.
- `clap` argument parsing — third-party.
- The actual NVIDIA driver — out of scope.
- Real PWM-on-real-hardware effects — operator responsibility, covered by `--calibrate-fans`.

---

## CI and release

### CI (`.github/workflows/ci.yml`) — runs on every PR

```yaml
- cargo fmt --check                   # rustfmt enforced
- cargo clippy -- -D warnings          # zero clippy warnings allowed
- cargo test                           # unit + tempfs-fake I/O + smoke
- cargo build --release --target x86_64-unknown-linux-musl
                                       # verify static binary builds clean
```

The `softdog`-backed integration test (`#[ignore]`) does NOT run in CI — GitHub runners can't reliably load kernel modules. It runs locally before tagging a release; the release checklist in `CHANGELOG.md` includes `cargo test -- --ignored` as a manual step.

`cargo audit` and `cargo deny` are intentionally not added at v0.1. Direct deps are ~10; the noise-to-signal ratio of an unmonitored audit pass is bad. Add them when the dep tree exceeds ~30 or after the first reported CVE.

### Release (`.github/workflows/release.yml`) — runs on tag push (`v*`)

```yaml
- build static binary against x86_64-unknown-linux-musl
- compute sha256 checksum
- create GitHub Release for the tag
- attach binary + checksum file
```

No `cargo publish` to crates.io — this is a binary, not a library.

### Versioning

- **Semver, starting at `0.1.0`.** Stays pre-1.0 until the config schema and CLI surface have been used long enough to be confident in their stability. `1.0.0` is a commitment to backwards compatibility, not "first useful release."
- **Annotated git tags** `v0.1.0`, `v0.2.0`, etc. Signed if a signing key is set up; otherwise unsigned is fine for a personal OSS project.
- **`CHANGELOG.md`** in [Keep a Changelog](https://keepachangelog.com/) format — manually maintained, sections Added / Changed / Fixed per release. Writing it forces explicit thought about what users care about.

### Dependency strategy

- `Cargo.toml` uses caret semver (`nvml-wrapper = "0.12"`) — Cargo's default. Means "compatible updates only," which is what we want for transparent security patches.
- `Cargo.lock` is **committed** (binary-crate convention). Gives reproducible builds.
- No exact `=` pinning in `Cargo.toml` — over-constrains and prevents `cargo update` from picking up patch fixes.
- `cargo update` run quarterly; verify tests pass before committing the lockfile change. No automated bot (Renovate / Dependabot) at v0.1 — the dep tree is small enough that manual review is fine.

### Reproducibility

`rust-toolchain.toml` at the repo root pins the exact compiler version and target:

```toml
[toolchain]
channel    = "1.85"
targets    = ["x86_64-unknown-linux-musl"]
components = ["rustfmt", "clippy"]
```

Anyone (including you, six months later) who clones and builds gets the exact compiler that produced the released binaries. CI uses this same file. Eliminates one class of "works on my machine" — at zero cost.

### Config schema versioning

The `[global].config_version` field is mandatory. The daemon's parser refuses to start if the value is unknown to it. Bumping the daemon's major version may require bumping `config_version`; migrations are documented in `CHANGELOG.md` and `README.md`. v0.1.0 ships with `config_version = 1`.

---

## Known considerations (deferred)

Items recognised during the design phase but deliberately not addressed in v0.1. Documented here so future contributors don't reinvent the consideration without context.

### Curve hysteresis

Linear interpolation between curve points reduces fan oscillation near inflection points but does not eliminate it. A GPU bouncing between e.g. 70 °C and 71 °C with a `70:80` curve point will produce small but visible PWM oscillation around 80%. Possible mitigations include:

- A minimum-PWM-delta threshold: do not write a new PWM value unless it differs from the last write by more than N units.
- Explicit hysteresis windows on each curve segment: enter the segment at temp T, exit at T − Δ.
- An EMA filter on the temperature reading.

YAGNI for v0.1 — see if it's a real problem on real hardware first. If a user reports audible fan oscillation in field use, the minimum-delta-threshold mitigation is the smallest change with the most impact.

---

## Deployment prerequisites

- BIOS configured to drive every PWM header at 100% by default (ADR-0002).
- Proprietary NVIDIA driver installed (`libnvidia-ml.so.1` present at `/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1` or equivalent).
- `nvidia-persistenced.service` enabled. Not strictly required (the daemon holds NVML open continuously and acts as a de facto persistence agent), but belt-and-suspenders against driver state being torn down during a daemon restart.
- `/dev/watchdog` available — either a hardware watchdog chip or `softdog` loaded at boot (`echo softdog | sudo tee /etc/modules-load.d/softdog.conf`).
- **`/dev/watchdog` is single-open.** The daemon must be the sole owner. `install.sh` checks for known conflicts and refuses to install if any are detected:
  - `RuntimeWatchdogSec=` set in `/etc/systemd/system.conf` or `/etc/systemd/system.conf.d/*.conf` (systemd would claim the device).
  - `lsof /dev/watchdog*` reports any holder at install time (rare — kdump, pacemaker, drbd heartbeat).
- **`nowayout` kernel parameter.** Some watchdog modules accept `nowayout=1`, which means writing the magic byte `'V'` on shutdown does *not* prevent a reboot. `install.sh` checks `/sys/module/<wdt_module>/parameters/nowayout` and warns if set; for our use case we want `nowayout=0` so graceful shutdown can disarm cleanly.
- **Startup self-check.** If `open(/dev/watchdog)` returns `EBUSY`, the daemon refuses to start with a clear error message naming the suspected conflict (systemd `RuntimeWatchdogSec=` is the most likely cause) instead of silently running without a reboot path. Per ADR-0002, BIOS failsafe is the fallback in that case.
