# TeslaGPUFanControl

A Linux daemon that drives motherboard PWM fan headers from NVIDIA Tesla (passively-cooled) GPU temperatures, with hard-failure escalation to the kernel hardware watchdog. Designed for unattended remote compute boxes.

## Language

**GPU**:
A NVIDIA Tesla card with no internal cooling, identified by its NVML index. Each GPU has its own fan curve and its own temperature sensor reading.
_Avoid_: device, card, accelerator (these are too generic — say "GPU" or `nvml_index`).

**Fan**:
A physical fan connected to a motherboard PWM header, identified by `(hwmon_path, pwm_channel)`. Each fan has its own RPM tachometer and its own health bounds (`min_rpm`, `max_rpm`).
_Avoid_: blower, cooler.

**Cooling Group**:
A set of **Fans** that share an airflow path with a set of **GPUs**. It is the unit of fan-speed decision-making: for each GPU the **Fan Curve** is evaluated and the result is clamped to that GPU's `[min_fan_pct, max_fan_pct]` band; the group's target percentage is the `max()` of those bounded per-GPU values, applied uniformly to every fan in the group. Clamp-per-GPU happens **before** the `max()`. Cooling groups model physical reality (single-duct, Y-duct, etc.) rather than logical ownership.
_Avoid_: zone (collides with ACPI thermal zone), channel (collides with PWM channel), pool.

**Fan Curve**:
A monotonic non-decreasing piecewise-linear function from GPU temperature (°C) to fan percentage (0–100), defined by 2–10 `temp:pct` points. Below the lowest point and above the highest, the value is clamped. A property of a **GPU**, not of a **Fan**.

**Hardware Watchdog**:
The kernel device at `/dev/watchdog` (real chip or `softdog` module) that hard-reboots the box if not "fed" within its timeout. The single non-recoverable safety mechanism in the system.
_Avoid_: just "watchdog" — always qualify as "hardware watchdog", "internal failure logic", or "systemd watchdog" (these are three distinct things).

**Internal Failure Logic**:
Per-GPU and per-fan consecutive-failure counters maintained inside the daemon. When a counter exceeds its threshold, the daemon declares a fault, maxes all fans, and stops feeding the **Hardware Watchdog**. Distinct from the **Hardware Watchdog** itself, which is a kernel mechanism.

**GPU NVML Fault** (formerly **Thermal-Blind Fault**):
The fault state declared when any per-**GPU** NVML operation — temperature read, **Power Limit** read, or **Power Limit** set — fails for `gpu_fail_threshold` consecutive ticks. Treated as severe as a fan hardware fault: max all fans, stop feeding the **Hardware Watchdog**, kernel reboots. The shared counter and shared response reflect ADR-0001: we don't act differently per failing API, so they share one fault category.
_Avoid_: "thermal-blind" alone (the term is retained as a historical alias because temperature-read failure was the original trigger, but the category is broader now).

**Power Limit**:
A configured maximum power draw (watts) for a **GPU**, applied via NVML's power-management-limit interface at startup and re-asserted on a global cadence so external changes (e.g. `nvidia-smi -pl …`, driver reloads, other tools) are reverted. A property of a **GPU**, opt-in per **GPU**. **Protective, not convenient**: the typical use case is a system whose PSU cannot sustain all GPUs at full board power simultaneously, so the **Power Limit** is a hard ceiling that prevents brown-out. An external override that *raises* the limit is therefore a safety event, not a routine drift.
_Avoid_: TDP (different — board design power), power-cap (overloaded; ADR-0001 uses the phrase when rejecting a softer fault-response policy, not this feature), throttle (a thermal-driven mechanism inside the GPU).

## Relationships

- A **GPU** belongs to exactly one **Cooling Group**.
- A **Fan** belongs to exactly one **Cooling Group**.
- A **Cooling Group** has 1..N **Fans** and 1..M **GPUs**.
- A **Cooling Group**'s target fan percentage = `max(clamp(curve(temp), gpu.min_fan_pct, gpu.max_fan_pct) for gpu in group.gpus)`.
- All **Fans** in a **Cooling Group** run at the same percentage at any given moment.
- Any fault, in any **Cooling Group**, escalates to "max all fans across all groups + stop feeding the **Hardware Watchdog**" — fault containment is system-wide by design (see ADR-0001).

## Example dialogue

> **Dev:** "If GPU 0 hits 80 °C and GPU 1 is idle at 35 °C, and they're in the same **Cooling Group**, what does the **Fan** do?"
> **Domain expert:** "It runs at the curve output for 80 °C. The group's target is `max(curve(80), curve(35))` so the hot GPU wins."
>
> **Dev:** "And if GPU 0's NVML read fails?"
> **Domain expert:** "After `gpu_fail_threshold` polls we declare a **GPU NVML Fault**, max every **Fan** in every **Cooling Group**, and stop feeding the **Hardware Watchdog**. The kernel reboots after `timeout_s`. Same counter and same response if instead the **Power Limit** read or set is failing — we don't distinguish, since the fault response is identical."

## Flagged ambiguities

- "Watchdog" was used to mean three different things (kernel `/dev/watchdog`, in-process failure counters, `sd_notify` to systemd). Resolved: always qualify — **Hardware Watchdog**, **Internal Failure Logic**, **systemd watchdog**.
- "Fan ownership" originally implied a tree (`gpu0.fan0`). Resolved: fans and GPUs are siblings under a **Cooling Group**, not parent/child.
