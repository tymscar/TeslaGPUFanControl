//! Watchdog module — Wave 1 contains the pure `FaultTracker`. The real
//! `HardwareWatchdog` (ioctl-driven `/dev/watchdog`) is a Wave 2 stub.
//!
//! See PLAN.md §watchdog.rs and CONTEXT.md ("Internal Failure Logic"). The
//! tracker maintains per-GPU and per-fan consecutive-failure counters and
//! latches a fault flag once any threshold is exceeded.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanHealth {
    Ok,
    SpinUp,
    Stalled,
    Overspeed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanFaultKind {
    Stalled,
    Overspeed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    ThermalBlind {
        gpu_id: String,
    },
    FanHardware {
        fan_id: String,
        last_rpm: u32,
        kind: FanFaultKind,
    },
}

pub struct FaultTracker {
    gpu_counters: HashMap<String, u32>,
    fan_counters: HashMap<String, u32>,
    gpu_threshold: u32,
    fan_threshold: u32,
    any_fault: bool,
}

impl FaultTracker {
    pub fn new(
        gpu_ids: &[String],
        fan_ids: &[String],
        gpu_threshold: u32,
        fan_threshold: u32,
    ) -> Self {
        let gpu_counters = gpu_ids.iter().map(|id| (id.clone(), 0u32)).collect();
        let fan_counters = fan_ids.iter().map(|id| (id.clone(), 0u32)).collect();
        Self {
            gpu_counters,
            fan_counters,
            gpu_threshold,
            fan_threshold,
            any_fault: false,
        }
    }

    /// `read_ok=false` increments the counter; `read_ok=true` resets it.
    /// Returns `Some(ThermalBlind)` on the call that pushes the counter
    /// strictly above `gpu_threshold` (PLAN.md: "counter > gpu_fail_threshold").
    pub fn tick_gpu(&mut self, gpu_id: &str, read_ok: bool) -> Option<Fault> {
        let counter = self.gpu_counters.entry(gpu_id.to_string()).or_insert(0);
        if read_ok {
            *counter = 0;
            return None;
        }
        *counter += 1;
        if *counter > self.gpu_threshold {
            self.any_fault = true;
            return Some(Fault::ThermalBlind {
                gpu_id: gpu_id.to_string(),
            });
        }
        None
    }

    /// `Ok` resets, `SpinUp` is a no-op, `Stalled`/`Overspeed` increment.
    /// Returns `Some(FanHardware)` on the call that pushes the counter
    /// strictly above `fan_threshold`.
    pub fn tick_fan(&mut self, fan_id: &str, health: FanHealth, last_rpm: u32) -> Option<Fault> {
        let counter = self.fan_counters.entry(fan_id.to_string()).or_insert(0);
        let kind = match health {
            FanHealth::Ok => {
                *counter = 0;
                return None;
            }
            FanHealth::SpinUp => {
                return None;
            }
            FanHealth::Stalled => FanFaultKind::Stalled,
            FanHealth::Overspeed => FanFaultKind::Overspeed,
        };
        *counter += 1;
        if *counter > self.fan_threshold {
            self.any_fault = true;
            return Some(Fault::FanHardware {
                fan_id: fan_id.to_string(),
                last_rpm,
                kind,
            });
        }
        None
    }

    /// True once any fault has been declared. Latched: never resets to false.
    pub fn any_fault(&self) -> bool {
        self.any_fault
    }
}

/// Wave 2 — real ioctl impl pending.
pub struct HardwareWatchdog;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ids(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn thermal_blind_declared_after_threshold_plus_one_failures() {
        // Threshold = 3, so the 4th consecutive failure declares the fault.
        // PLAN.md: "counter > gpu_fail_threshold". Off-by-one explicit check.
        let mut t = FaultTracker::new(&ids(&["g0"]), &[], 3, 3);
        assert_eq!(t.tick_gpu("g0", false), None); // counter = 1
        assert_eq!(t.tick_gpu("g0", false), None); // counter = 2
        assert_eq!(t.tick_gpu("g0", false), None); // counter = 3 (== threshold, not >)
        let fault = t.tick_gpu("g0", false); // counter = 4 (> threshold)
        assert_eq!(
            fault,
            Some(Fault::ThermalBlind {
                gpu_id: "g0".to_string()
            })
        );
        assert!(t.any_fault());
    }

    #[test]
    fn ok_resets_gpu_counter_mid_failure() {
        let mut t = FaultTracker::new(&ids(&["g0"]), &[], 3, 3);
        t.tick_gpu("g0", false);
        t.tick_gpu("g0", false);
        t.tick_gpu("g0", true); // reset
                                // Now we should need 4 more failures to declare; verify by counting up.
        assert_eq!(t.tick_gpu("g0", false), None);
        assert_eq!(t.tick_gpu("g0", false), None);
        assert_eq!(t.tick_gpu("g0", false), None);
        assert!(t.tick_gpu("g0", false).is_some());
    }

    #[test]
    fn fan_stalled_past_threshold_declares_fan_hardware() {
        let mut t = FaultTracker::new(&[], &ids(&["f0"]), 3, 3);
        for _ in 0..3 {
            assert_eq!(t.tick_fan("f0", FanHealth::Stalled, 0), None);
        }
        let fault = t.tick_fan("f0", FanHealth::Stalled, 0);
        assert_eq!(
            fault,
            Some(Fault::FanHardware {
                fan_id: "f0".to_string(),
                last_rpm: 0,
                kind: FanFaultKind::Stalled,
            })
        );
    }

    #[test]
    fn fan_overspeed_declares_overspeed_variant() {
        let mut t = FaultTracker::new(&[], &ids(&["f0"]), 3, 1);
        assert_eq!(t.tick_fan("f0", FanHealth::Overspeed, 9999), None);
        let fault = t.tick_fan("f0", FanHealth::Overspeed, 9999);
        assert_eq!(
            fault,
            Some(Fault::FanHardware {
                fan_id: "f0".to_string(),
                last_rpm: 9999,
                kind: FanFaultKind::Overspeed,
            })
        );
    }

    #[test]
    fn spinup_does_not_increment_fan_counter() {
        let mut t = FaultTracker::new(&[], &ids(&["f0"]), 3, 3);
        // Interleave Stalled with SpinUp; only 3 Stalleds — should not declare yet.
        t.tick_fan("f0", FanHealth::Stalled, 0);
        t.tick_fan("f0", FanHealth::SpinUp, 50);
        t.tick_fan("f0", FanHealth::Stalled, 0);
        t.tick_fan("f0", FanHealth::SpinUp, 50);
        assert_eq!(t.tick_fan("f0", FanHealth::Stalled, 0), None);
        // One more Stalled (counter = 4) declares fault.
        assert!(t.tick_fan("f0", FanHealth::Stalled, 0).is_some());
    }

    #[test]
    fn ok_resets_fan_counter_after_stalled() {
        let mut t = FaultTracker::new(&[], &ids(&["f0"]), 3, 3);
        t.tick_fan("f0", FanHealth::Stalled, 0);
        t.tick_fan("f0", FanHealth::Stalled, 0);
        t.tick_fan("f0", FanHealth::Ok, 1000); // reset
                                               // Need 4 more Stalleds now.
        assert_eq!(t.tick_fan("f0", FanHealth::Stalled, 0), None);
        assert_eq!(t.tick_fan("f0", FanHealth::Stalled, 0), None);
        assert_eq!(t.tick_fan("f0", FanHealth::Stalled, 0), None);
        assert!(t.tick_fan("f0", FanHealth::Stalled, 0).is_some());
    }

    #[test]
    fn any_fault_latches_true_forever() {
        let mut t = FaultTracker::new(&ids(&["g0"]), &ids(&["f0"]), 1, 1);
        assert!(!t.any_fault());
        // Trip a fault.
        t.tick_gpu("g0", false);
        t.tick_gpu("g0", false);
        assert!(t.any_fault());
        // Subsequent successful reads must NOT clear it.
        t.tick_gpu("g0", true);
        t.tick_fan("f0", FanHealth::Ok, 1500);
        assert!(t.any_fault());
    }
}
