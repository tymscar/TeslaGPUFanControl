# Power Limit not restored on shutdown by default

On graceful shutdown the daemon restores manual fan control to the BIOS-managed `pwm_enable` snapshot (ADR-0002 BIOS failsafe), then disarms the watchdog. The natural symmetry would have it also restore each GPU's `power_management_limit` to the driver default. We deliberately do not — by default. The feature exists to protect a PSU that cannot sustain all GPUs at full board power (see ADR-0008), so the failsafe state for power is *staying low*, not *going to default*. Restoring to default during the daemon-down window between a graceful stop and the next start would create the very PSU-exposure window the feature exists to prevent.

The behaviour is gated by a single global key: `power_limit_restore_on_shutdown` (default `false`). When `true` (clean uninstall scenarios), graceful shutdown writes the NVML driver default back via `set_power_management_limit(default_mw)` for each GPU that had `power_limit_enabled = true`, before fan restore and watchdog disarm. Crash paths (panic=abort, SIGKILL) skip graceful shutdown entirely under ADR-0007, so the limit *always* persists across crashes regardless of this flag — which is correct under the PSU-protection framing.

## Consequences

- The shutdown sequence in `run_daemon` is no longer "fan restore → watchdog disarm." It becomes "(optional) power-limit restore → fan restore → watchdog disarm." Power-limit restore goes first so it happens while NVML is healthy and the daemon still owns manual fan control.
- An operator who stops the daemon and runs `nvidia-smi --query-gpu=power.limit` will see the daemon's last-applied limit, not the firmware default. The CONFIG-TUTORIAL documents both the rationale and the manual escape hatch (`nvidia-smi -pl <default_w>` after stop, or set the flag to `true`).
- Future engineers reading the shutdown loop will see fans being restored and may try to "fix" the missing power-limit restore. The comment in `run_daemon` and this ADR exist specifically to tell them not to.
