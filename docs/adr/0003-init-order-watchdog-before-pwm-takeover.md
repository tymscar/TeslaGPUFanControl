# Init order: arm Hardware Watchdog before taking over PWM

The daemon's startup sequence is load-bearing for safety:

1. Parse and validate config (no I/O side effects yet).
2. Resolve the **Chip** name to a concrete `hwmonN` path. Refuse to start on miss.
3. Open NVML and verify each configured GPU is present and readable.
4. Open `/dev/watchdog` and arm it (`WDIOC_SETTIMEOUT`).
5. Only now: open every fan device and write `pwm_enable=1` to take manual control.
6. Notify systemd `READY=1` and enter the main loop.

The order matters because each stage is a "point of no return" with respect to recoverability:

- Steps 1–3 are pure observation. Failing here leaves the system untouched; the BIOS failsafe (ADR-0002) is in effect.
- Step 4 arms the kernel reboot mechanism. Once armed, any subsequent process death (crash, SIGKILL, OOM-kill) results in a hard reboot, which returns the system to the BIOS failsafe.
- Step 5 takes manual control of PWM. If the daemon crashed between step 5 and step 4 being armed, the fans would be stuck at whatever value the last write set — likely zero or low — with no kernel recovery. **This window must not exist.**

A symmetric invariant applies on shutdown: write `pwm_enable=2` (restore BIOS control) *before* writing `'V'` to `/dev/watchdog` to disarm it. If the order were reversed and the process died between disarming the watchdog and restoring PWM, the system would again be stuck at manual-mode PWM with no recovery.

## Consequences

- `main.c` startup must follow this exact order; it is not a stylistic choice.
- Tests / dry-runs that skip the watchdog must also skip the PWM takeover, or document explicitly that they are running in an unsafe-by-design mode.
