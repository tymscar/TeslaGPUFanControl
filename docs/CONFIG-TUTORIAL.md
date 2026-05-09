# Configuration tutorial

How to discover the values that go into `tesla_fan_control.conf` on a fresh
box, in the order the config sections need to be filled in. Pairs with
`config/tesla_fan_control.conf` (annotated template) and `docs/PLAN.md`
§Configuration File Format (canonical schema).

---

## 1. Find your GPUs → `nvml_index`

`[gpu:N]` blocks key off the NVML device index, not the PCI bus or the X
display index.

```bash
nvidia-smi --query-gpu=index,name,uuid,pci.bus_id --format=csv
```

The `index` column is the value for `nvml_index =`. Record the UUID alongside
it — the index is enumeration-order and can shift if you add or remove cards.

Sanity check that the daemon will read the same temperature source:

```bash
nvidia-smi --query-gpu=index,temperature.gpu --format=csv
```

---

## 2. Find your hwmon chip → `chip =`

The daemon scans `/sys/class/hwmon/*/name` and matches that string verbatim
(`src/chip.rs:52`). One command lists every candidate:

```bash
for d in /sys/class/hwmon/hwmon*; do
    echo "$d -> $(cat "$d/name") (driver: $(basename "$(readlink -f "$d/device/driver" 2>/dev/null)"))"
done
```

Typical SuperIO board:

```
/sys/class/hwmon/hwmon2 -> nct6798 (driver: nct6775)
/sys/class/hwmon/hwmon3 -> coretemp (driver: coretemp)
/sys/class/hwmon/hwmon4 -> nvme    (driver: nvme)
```

You want the SuperIO chip: `nct6798`, `nct6775`, `it8628`, `it87`,
`f71808a`, etc. Copy the name verbatim into `chip = …`.

If no SuperIO chip appears, load the right kernel module:

```bash
sudo modprobe nct6775   # or it87, f71882fg, nct6683 (some AMD), …
```

If `nct6775` refuses with "no supported chip", you may need
`modprobe nct6775 force_id=0xd428` (id from `dmidecode` / vendor docs). On
some AMD platforms the in-tree driver does not bind — load the vendor's
out-of-tree driver.

### Duplicate chip names (rare)

Dual-SuperIO boards expose the same chip name twice. Pin the literal directory
with `hwmon_path = /sys/class/hwmon/hwmon2` instead of `chip =`. `hwmon_path`
wins over `chip` (`src/chip.rs:94-103`). Note that `hwmonN` numbering can
drift across reboots — prefer `device_path = /sys/devices/platform/…` if your
kernel exposes a stable platform path.

---

## 3. Find your fans → `pwm_channel =`

Each `pwmN` controls one header; `fanN_input` reports its tach RPM. The
mapping `N` → physical header is per-board and not discoverable from
software. You discover it by wiggling fans one at a time.

### 3a. See which channels are wired

```bash
HWMON=/sys/class/hwmon/hwmon2   # whichever path matched your chip in step 2
grep . "$HWMON"/fan*_input
```

Channels reading `0` consistently are either unconnected or the fan isn't
spinning yet (BIOS may stall low-temp headers). Channels reading real RPM are
wired.

### 3b. Map channel → physical fan

For each candidate channel, take manual control and pulse it:

```bash
sudo tee "$HWMON/pwm1_enable" <<<1   # 1 = manual
sudo tee "$HWMON/pwm1"        <<<255 # full duty
sleep 3
cat  "$HWMON/fan1_input"             # RPM should jump
sudo tee "$HWMON/pwm1"        <<<0   # back to silent
sudo tee "$HWMON/pwm1_enable" <<<5   # 5 = automatic (BIOS) — restore safe state
```

Listen / look at which fan changed. Repeat for `pwm2`, `pwm3`, … and write
down the mapping (e.g. *pwm1 = front intake duct on GPU0, pwm2 = Y-duct on
GPU1+GPU2*).

> Always restore `pwm*_enable = 5` between tests so BIOS keeps cooling. A fan
> that looks unconnected might just be sitting under BIOS's silent-mode
> threshold — forcing `pwm=255` is the disambiguator.

### 3c. Note the RPM range

While sweeping, record the RPM at full duty and at the lowest duty where the
fan still spins. These become `min_rpm` / `max_rpm`:

- `min_rpm` ≈ ~50% of the lowest-stable RPM (gives spin-up headroom)
- `max_rpm` ≈ ~110% of full-duty RPM (catches sensor flips, not normal swings)

`--calibrate-fans` (step 6) does this sweep automatically and prints
suggested values, but doing one channel by hand first is the cheapest way to
confirm the channel mapping.

---

## 4. Decide cooling groups

This is physical, not discoverable: which fans push air over which GPUs.

- One fan per card → one group per (fan, gpu) pair.
- Y-duct or shared shroud → one group with `fans = fX, fY` and `gpus = 0, 1`.

The group target % is `max()` of the per-GPU curve outputs, clamped to each
GPU's `[min_fan_pct, max_fan_pct]`. The hottest card in the group sets the
speed; any one GPU's `min_fan_pct` acts as a group-wide floor.

---

## 5. Limiting GPU power (PSU protection)

**Skip this section if your PSU can sustain every GPU at full board power
simultaneously.** It's only relevant when the supply can't, which is the
typical case for multi-GPU compute boxes built on prosumer PSUs.

The daemon can enforce a per-GPU **Power Limit** (NVML
`power_management_limit`) at startup and re-assert it on a fixed interval.
External overrides (`nvidia-smi -pl`, driver reloads, other tools) are
reverted within one cadence cycle. See ADR-0008 for the rationale.

### 5a. Find the constraints

Per GPU:

```bash
nvidia-smi -i 0 --query-gpu=power.default_limit,power.min_limit,power.max_limit \
                --format=csv,noheader
```

Output looks like `250.00 W, 125.00 W, 300.00 W`. Those are your
`power_management_limit_default`, `_min`, and `_max` from NVML. Your
`power_limit_w` must fall in `[min, max]` (the daemon refuses to start at
runtime check R6 if it doesn't).

### 5b. Pick a value that fits your PSU budget

The daemon does **not** know what your PSU can sustain — that depends on
non-GPU draws (CPU, drives, fans, board) and your PSU's sustained-vs-peak
spec. Operator math:

```
sum(power_limit_w for each GPU)  +  ~50 W per CPU socket  +  drives/fans
                                 ≤  PSU sustained budget × 0.85
```

Worked example: 2× P100 (TDP 250 W) on a 750 W Gold PSU with one socket and
modest drives. Sustained budget ≈ 750 × 0.85 = 638 W; non-GPU ≈ 100 W;
GPU budget ≈ 538 W. Cap each GPU at 250 W stock — already fits. If
upgrading to 2× H100 PCIe (TDP 350 W), GPU budget 700 W exceeds 538 W —
cap each at 260 W (`power_limit_w = 260`).

### 5c. Add the keys to the config

```ini
[global]
# … existing keys …
power_limit_check_interval_s   = 120     # re-assert every 2 minutes
power_limit_restore_on_shutdown = false  # PSU protection persists across stops
power_limit_validate_max_w     = 1000    # typo guard upper bound

[gpu:0]
# … existing keys …
power_limit_enabled = true
power_limit_w       = 260                # in watts; converted to mW for NVML
```

`--check-config` runs P1–P5 (and the existing rules). Hardware-side
constraints are checked at daemon startup (R6). On startup the daemon
applies each enabled GPU's limit before arming the watchdog; if NVML
rejects the value (out of `power_min_limit..power_max_limit`) or the set
itself fails (driver bug, missing root, GPU doesn't support power
management), the daemon refuses to start.

### 5d. Drift handling

| Direction | Daemon's response |
|---|---|
| Downward (e.g., admin ran `nvidia-smi -pl 200` while target is 260) | WARN log, re-assert. No PSU risk. |
| Upward (e.g., something raised it to 350) | **ERROR** log + `sd_notify(STATUS=…)` + re-assert. PSU exposure for the cadence window — keep `power_limit_check_interval_s` short on PSU-constrained boxes. |

If you need to manually adjust the limit while the daemon is running,
**don't fight it with `nvidia-smi -pl`** — set `power_limit_enabled = false`
in the config and `systemctl reload tesla_fan_control` (SIGHUP). The
daemon will stop re-asserting and you can set whatever value you like.

### 5e. Uninstalling / disabling enforcement

By default, `systemctl stop tesla_fan_control` leaves the last-applied
power limit in place — that's deliberate, so PSU protection persists
across daemon-down windows. To get the GPU back at firmware default,
either:

```bash
# Option 1: opt into restore on next stop
sed -i 's/power_limit_restore_on_shutdown = false/power_limit_restore_on_shutdown = true/' /etc/tesla_fan_control.conf
sudo systemctl reload tesla_fan_control
sudo systemctl stop tesla_fan_control

# Option 2: stop, then restore manually
sudo systemctl stop tesla_fan_control
sudo nvidia-smi -i 0 -pl 250    # use power.default_limit from §5a
```

Crashes (panic, SIGKILL, kernel watchdog reboot) always preserve the
limit regardless of `power_limit_restore_on_shutdown` — that's correct
under PSU framing. After a reboot, firmware defaults are restored by
hardware and the daemon re-applies the configured limit on its next
start.

---

## 6. Wire it into a config

Start from `config/tesla_fan_control.conf`. Minimal example — one GPU, one
fan:

```ini
[global]
config_version   = 1
poll_interval_ms = 1000
log_level        = info
log_file         =                          # empty = journald only

[watchdog]
enabled            = true
device             = /dev/watchdog
timeout_s          = 30
gpu_fail_threshold = 3

[gpu:0]
nvml_index   = 0
curve        = 40:20, 55:40, 70:80, 80:100  # P100 throttles at 85 °C
min_fan_pct  = 20
max_fan_pct  = 100

[fan:f0]
chip               = nct6798                # from step 2
pwm_channel        = 1                      # from step 3
min_rpm            = 200
max_rpm            = 3000
fan_fail_threshold = 3
spin_up_grace_s    = 10

[group:main]
gpus = 0
fans = f0
```

Save anywhere — e.g. `./config/dev.conf`.

---

## 7. Validate, calibrate, dry-run

### 7a. Static validation (no hardware, no root)

```bash
./target/release/tesla_fan_control --check-config -c ./config/dev.conf
```

Runs every S/G/F/W/T/P rule from `docs/PLAN.md` §Configuration Validation.
Fix any violation before going near the hardware.

### 7b. Calibration sweep (root; daemon must be stopped)

```bash
sudo systemctl stop tesla_fan_control 2>/dev/null
sudo ./target/release/tesla_fan_control --calibrate-fans -c ./config/dev.conf
```

Prints suggested `min_fan_pct` and `spin_up_grace_s` per fan based on real
spin-up behavior. Copy them back into the config. (Refuses to run while the
systemd unit is active — `src/main.rs:558-563`.)

### 7c. Live foreground run

```bash
sudo ./target/release/tesla_fan_control -c ./config/dev.conf -f
```

Watch the log; load the GPU with a real CUDA workload (`gpu_burn`,
`nvidia-smi dmon`, or a training step). Confirm fans track the curve.
`Ctrl+C` cleanly disarms the watchdog and returns the headers to BIOS auto.

---

## Prerequisites worth re-checking before step 7

- **BIOS**: every PWM header set to **100 % at boot**. This is the failsafe
  floor — if the daemon dies, the watchdog reboots and BIOS cools the cards
  until the daemon restarts (ADR-0002).
- **Watchdog device**: `/dev/watchdog` available. No hardware WDT? Load
  softdog: `echo softdog | sudo tee /etc/modules-load.d/softdog.conf && sudo modprobe softdog`.
- **systemd does not own the watchdog**: `RuntimeWatchdogSec=` must **not** be
  set in `/etc/systemd/system.conf`. systemd grabs `/dev/watchdog`
  exclusively if it is, and the daemon will fail with `EBUSY` at startup.
- **Development safety**: set `nowayout=0` on the watchdog module
  (`options softdog nowayout=0` in `/etc/modprobe.d/softdog.conf`) so a
  `Ctrl+C` does not strand the timer and reboot the box.
