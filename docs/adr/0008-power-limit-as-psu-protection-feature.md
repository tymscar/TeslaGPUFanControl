# Power Limit as PSU-protection feature

The daemon enforces a configurable per-GPU **Power Limit** (NVML `power_management_limit`), set at startup and re-asserted on a global cadence. The motivation is **PSU protection** on systems whose power supply cannot sustain all GPUs at full board power simultaneously: the limit is a hard ceiling that prevents brown-out. This puts the feature in the same risk class as the fan curves — protective, not cosmetic — so external overrides that *raise* the limit count as safety events, not routine drift. Downward drift is reverted at WARN; upward drift is reverted at ERROR + `sd_notify(STATUS=…)`.

ADR-0001 mentions "GPU power-cap" only when *rejecting* it as a softer fault-response policy. That rejection still stands. This ADR introduces Power Limit in the opposite role — a normal-operation protective ceiling, not a fault response — which is why CONTEXT.md uses "Power Limit" and reserves "power-cap" for the rejected idea.

NVML failures on the power path (read or set) count toward the existing per-GPU `gpu_fail_threshold` and declare the same fault as a temperature-read failure, so the fault category is renamed from `Fault::ThermalBlind` to `Fault::GpuNvml` and the log field becomes `fault_kind = "gpu_nvml"`. The `FaultTracker` keeps two independent counters per GPU (one for temp, one for power) but a single shared threshold and a single shared fault — splitting the counter avoids a dilution bug where successful temp ticks would reset a counter that the rarer power tick had just incremented.

## Consequences

- A new optional config block per GPU (`power_limit_enabled`, `power_limit_w`) and three new `[global]` keys (`power_limit_check_interval_s`, `power_limit_restore_on_shutdown`, `power_limit_validate_max_w`).
- The daemon refuses to start if a configured `power_limit_w` is outside the driver's `power_management_limit_constraints`, or if the initial `set_power_management_limit` call fails (new runtime check R6).
- `Fault::ThermalBlind` is renamed to `Fault::GpuNvml`; the log field `fault_kind = "thermal_blind"` becomes `"gpu_nvml"`. Operators with log-monitoring rules must update them. The CHANGELOG calls this out.
- `nvml.rs` exposes a richer `NvmlOps` trait (replacing `TempReader`) with `&mut self` for `set_power_limit_w`. `FakeNvml` gains the same methods.
- The PSU budget itself is *not* enforced in code — the daemon does not know non-GPU draws. Operators are responsible for picking `power_limit_w` values whose sum fits their PSU's sustained budget. The CONFIG-TUTORIAL spells this out.
