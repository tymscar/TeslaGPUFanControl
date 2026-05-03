# BIOS 100%-fans policy is the system-wide failsafe floor

The motherboard BIOS is configured to drive every PWM header at 100% by default. The daemon takes over by setting `pwm_enable=1` (manual) on the channels it owns and applying a curve; on graceful shutdown it restores `pwm_enable=2`, returning control to the BIOS policy.

This makes "no daemon" the safe state of the system. Every failure mode the daemon does *not* recover from in-process — chip-name lookup miss, NVML init failure, missing config, segfault during init before the **Hardware Watchdog** is open — falls back to BIOS-controlled 100%-fan operation. Loud and power-hungry, but thermally safe and visibly abnormal under remote KVM.

## Consequences

- The daemon is allowed to refuse to start on any validation failure (wrong chip, bad config). It does not need to attempt a "best effort" startup.
- On any clean shutdown (SIGTERM/SIGINT) the daemon **must** write `pwm_enable=2` to every channel it touched. `fan_restore()` is not optional.
- The init order is load-bearing: the daemon must open and arm the **Hardware Watchdog** *before* it sets any `pwm_enable=1`. If a crash happens after PWM is taken over but before the watchdog is armed, the system is stuck at whatever PWM value was last written with no recovery path. See ADR-0003.
- BIOS configuration is a deployment prerequisite — documented in install steps, not enforced by the daemon.
