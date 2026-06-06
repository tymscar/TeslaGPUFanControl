# Idle power tuning for Tesla P100 (and similar Pascal GPUs)

Operator-facing notes on reducing per-card idle power on Pascal-era Tesla
GPUs. Pairs with the helper scripts in `config/helper_scripts/`. Not a
feature of the daemon itself — these are knobs you flip on the host.

## Why this exists

Pascal Tesla cards (P100, P40, P4, P6) idle at ~22–28 W per card by
default. That's a hard floor set by the architecture; Pascal predates the
deep-idle features that ship with Volta and later silicon. You won't get
a P100 to single-digit idle without taking it offline. But there are five
levers that, applied judiciously, can shave 5–15 W per card and translate
to real power-bill savings on a multi-card box.

This document orders the levers from safest-most-effective to
risky-or-niche, and points to a script per lever that you can run, roll
back, and measure with.

## Realistic expectations

| Configuration | Per-card idle (P100) |
|---|---|
| Stock (ECC on, no persistenced, fans at BIOS 100 %) | ~26–30 W |
| ECC off + persistenced configured + daemon-managed fans | ~22–25 W |
| Above + PCIe ASPM powersave (when supported) | ~20–23 W |
| Above + PCI runtime PM (incompatible with daemon as configured) | ~10–15 W |
| Driver unbound entirely | ~3–5 W (PCIe slot baseline) |

Numbers are typical, not guaranteed. Your hardware revision, BIOS, kernel,
and driver version all move the needle. **Always measure before/after.**

## How to measure

Before you touch anything, snapshot the idle baseline with the GPU
genuinely idle (no jobs, no monitoring agents in a tight loop). Then
re-snapshot after each change.

```bash
./config/helper_scripts/measure-idle-power.sh --label before
# ... apply a lever ...
./config/helper_scripts/measure-idle-power.sh 30 --label after
```

The optional duration argument averages `power.draw` across all GPUs over
N seconds — better than a single-shot read for noise-prone cards.

The script also dumps:
- ECC mode (per card)
- `persistence_mode` (per card)
- `nvidia-persistenced.service` state
- `pcie_aspm` policy
- Per-card PCI `power/control` and `runtime_status`

so you can see at a glance which levers are already in effect.

---

## Lever 1 — ECC off

**Saves: 2–5 W per card. Safe. Reversible. Requires reboot.**

ECC keeps memory partially active for parity. Disabling it lets DRAM enter
deeper power states. Trade-off is data integrity — fine for re-runnable
training/inference workloads, less fine for long scientific compute where
you can't easily detect a silent bit flip.

```bash
sudo ./config/helper_scripts/disable-ecc.sh    # disable on every GPU
# (script prints "now reboot" — ECC mode change requires a GPU reset)
sudo reboot
# Verify after reboot:
nvidia-smi --query-gpu=index,ecc.mode.current --format=csv
```

Re-enable:

```bash
sudo ./config/helper_scripts/disable-ecc.sh --revert
sudo reboot
```

---

## Lever 2 — `nvidia-persistenced` (and the `-pm 1` nuance on Pascal)

**Saves: 1–3 W per card. Safe. No reboot needed.**

`nvidia-persistenced` is a userspace daemon that holds an NVML handle
open, keeping the driver resident. With it running, the firmware can
enter deeper P-states between jobs without the driver tearing down and
re-initialising the card every time something opens NVML. It is the
*supported* path for keeping the driver loaded — different from
`nvidia-smi -pm 1` (which sets a per-device persistence-mode bit).

On Pascal, you may need **both**:
- The systemd unit running (`systemctl enable --now nvidia-persistenced`)
- The per-device persistence-mode bit set (`nvidia-smi -pm 1`)

That second piece often gets dropped because Volta+ persistenced does not
require it. If `check-persistenced.sh` reports the daemon active but
`persistence_mode` reads `Disabled`, run:

```bash
sudo nvidia-smi -pm 1
```

…and add it to a boot-time script (e.g., a `systemd` `ExecStartPre` on
your compute service, or a one-shot unit) so it survives reboot.

Inspect the current state:

```bash
./config/helper_scripts/check-persistenced.sh
```

The script flags both common misconfigurations:
- Daemon not active but persistence_mode Enabled (someone ran `-pm 1` by
  hand once and it stuck)
- Daemon active but the per-device bit is Disabled (Pascal-specific case
  above)

---

## Lever 3 — PCIe ASPM L1

**Saves: 1–3 W per link. Risky on server boards. Reversible.**

When both endpoints support it, the PCIe link drops to a low-power state
between transactions. Most server BIOSes disable ASPM out of caution
because some chipsets exhibit link-training storms or AER (Advanced Error
Reporting) events under powersave. Worth trying, but watch dmesg.

```bash
sudo ./config/helper_scripts/enable-pcie-aspm.sh           # set powersave
sudo dmesg -w | grep -iE 'pcieport|aer'                    # in another shell
# wait several minutes; if quiet, the lever works on your hardware.
```

Roll back the moment you see `PCIe Bus Error` or sustained AER events:

```bash
sudo ./config/helper_scripts/enable-pcie-aspm.sh --revert
```

To make persistent across reboots, add to your kernel cmdline (NOT done
by the script — incorrect cmdline edits can prevent boot):

```
pcie_aspm=force pcie_aspm.policy=powersave
```

The script also has a `--status` mode that shows the current policy plus
the count of AER events in dmesg since boot, useful for periodic checks.

---

## Lever 4 — PCI runtime power management

**Saves: 8–15 W per card. Incompatible with the daemon as configured.**

Linux can suspend a PCIe device to D3hot when nothing accesses it. The
proprietary nvidia driver disables this by default on Tesla because most
compute workloads need fast resume. You can force it back on:

```bash
sudo ./config/helper_scripts/enable-pci-runtime-pm.sh
```

**The catch:** `tesla_fan_control` polls NVML once per second. Every poll
is a device access that wakes the GPU from D3hot. With the daemon running
at default `poll_interval_ms = 1000`, runtime PM saves nothing — the card
spends every second briefly waking up. The script refuses to run while
the daemon service is active.

Two ways forward:
- **Stop the daemon** during testing, or
- **Increase `poll_interval_ms` to 10000+** (ten seconds or more) and
  reload the daemon (SIGHUP). The cost: thermal-blind fault detection
  takes 10× longer to escalate. With `gpu_fail_threshold = 3` that's
  ~40 s instead of ~4 s before fans go to 100 % on a stuck NVML read. On
  a passively-cooled P100 that is not nothing — it's a real safety
  trade-off, not just a tuning knob. Decide deliberately.

Additional caveats:
- The driver's PCI runtime PM support on Pascal is uneven. Some
  kernel/driver combos hang on resume. Watch dmesg for `nvidia` errors.
- Anything else opening NVML — Prometheus exporters, monitoring agents,
  `nvidia-smi -l 1` left running, even `gpu_burn` finishing without
  cleanly closing — also defeats the savings.

Roll back:

```bash
sudo ./config/helper_scripts/enable-pci-runtime-pm.sh --revert
```

I'd skip this lever for most setups. The fan-daemon trade-off is real
and the savings on idle Pascal hardware are not big enough to justify
weakening the safety story.

---

## Lever 5 — Unbind unused cards

**Saves: ~15–20 W per unbound card. Aggressive but clean.**

If you have N cards but only need M < N at any given time, unbind the
spares. They drop to PCIe-slot baseline (a few watts) and disappear from
NVML. Rebind to bring them back online.

```bash
./config/helper_scripts/unbind-gpu.sh --list           # show bus IDs
sudo ./config/helper_scripts/unbind-gpu.sh 0000:0X:00.0
# ... use the active cards normally ...
sudo ./config/helper_scripts/unbind-gpu.sh --rebind 0000:0X:00.0
```

**Daemon coordination is mandatory.** An unbound GPU disappears from
NVML. The daemon will see consecutive read failures, hit
`gpu_fail_threshold`, declare `Fault::GpuNvml`, slam every fan to 100 %,
and stop feeding the watchdog → kernel reboot. The script refuses to
unbind while the daemon is active. To use this lever cleanly:

1. `sudo systemctl stop tesla_fan_control`
2. Edit `/etc/tesla_fan_control.conf` — remove the `[gpu:N]` block AND
   its group reference for each card you intend to unbind. Make sure
   structural-identity rules still hold (every group has ≥1 GPU + ≥1 fan,
   every fan referenced exactly once). If your topology has each fan
   bound to its own card, unbinding a card means dropping its fan from
   the config too.
3. `tesla_fan_control --check-config -c /etc/tesla_fan_control.conf` to
   validate.
4. `sudo systemctl start tesla_fan_control`
5. `sudo ./config/helper_scripts/unbind-gpu.sh <bus>`

Reversal is symmetric: rebind, restore the config block, restart the
daemon.

This is worth the effort *only* if you have a stable on/off pattern (e.g.
"I run 2 of 4 cards on weekdays, all 4 only for big jobs"). For
intermittent use, the config-edit overhead outweighs the savings.

---

## What I'd actually do

For most P100 boxes:

1. **Lever 1 (ECC off)** — biggest no-risk win, one reboot.
2. **Lever 2 (verify persistenced + the `-pm 1` Pascal nuance)** — already
   recommended by every NVIDIA datacenter guide; just confirm it's set up
   right.
3. **Lever 3 (ASPM)** — try it, watch dmesg for 24 h, keep or revert.
4. **Stop here.** The remaining levers either weaken the daemon's safety
   story (Lever 4) or require operational coordination that's only worth
   it for a specific usage pattern (Lever 5).

Measure before and after each. The whole exercise is a couple of hours of
work; you should expect to drop ~5–8 W per card without compromising
anything operational.

---

## The PSU efficiency angle

Worth raising because it changes the conclusion on small boxes.

GPU idle power is one term in the total system idle. The other big term
is **PSU efficiency at low load**. A 1500 W Platinum PSU running at 80 W
total draw is operating at ~5 % load, well below its efficiency sweet
spot (typically 40–60 %). At that point the PSU itself wastes more power
than the savings from any of the levers above.

If your goal is "lower the electricity bill of an idle compute box", the
biggest move is often **right-sizing the PSU** — a 750 W or 850 W unit
in place of a 1500 W unit can shave 10–20 W from idle just by running
closer to its efficiency peak. Check 80PLUS efficiency curves for your
specific PSU model at low load before deciding which optimisation to
chase first.

You can't apply this from a script.
