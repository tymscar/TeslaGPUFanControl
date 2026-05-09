//! NVML façade — temperature reads with sanity bounds.
//!
//! Per ADR-0006 the only trait permitted in this module is `TempReader`,
//! whose second concrete implementation is the test-only `FakeNvml`. Both
//! impls route through `check_bounds` so the contract for "valid
//! temperature" is one expression — including the deliberate 5 °C lower
//! bound that turns a driver-bug `0` into `Err(OutOfBounds)` rather than a
//! silent fan-idle (PLAN.md L293).
//!
//! Lifetime choice: the struct holds `Nvml` plus the configured indices
//! and calls `nvml.device_by_index(idx)` per read rather than caching
//! `Device<'_>` handles. `Device` borrows from `Nvml`, so caching would
//! force a self-referential struct; the per-call lookup is what
//! nvml-wrapper docs recommend in straightforward cases (PLAN.md L294).

use crate::units::Celsius;
use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};
use std::collections::HashSet;
use thiserror::Error;

const SANITY_MIN_C: i32 = 5;
const SANITY_MAX_C: i32 = 110;

#[derive(Debug, Error)]
pub enum NvmlError {
    #[error("NVML init failed: {0}")]
    Init(String),
    #[error("GPU index {0} not present (device count = {1})")]
    IndexOutOfRange(u32, u32),
    #[error("NVML temperature read failed for index {idx}: {cause}")]
    ReadFailed { idx: u32, cause: String },
    #[error("GPU index {idx}: temperature {temp}°C is outside sanity bounds [5, 110]")]
    OutOfBounds { idx: u32, temp: i32 },
    #[error("NVML set_power_management_limit failed for index {idx}: {cause}")]
    SetFailed { idx: u32, cause: String },
}

/// Operations the daemon needs from NVML. Renamed from `TempReader` in
/// ADR-0008 once the Power Limit feature broadened the surface. Single
/// trait, single fake (`FakeNvml`) — see ADR-0006 on why we don't split.
pub trait NvmlOps {
    fn read_temp(&self, idx: u32) -> Result<Celsius, NvmlError>;
    fn read_power_limit_w(&self, idx: u32) -> Result<u32, NvmlError>;
    fn power_limit_constraints_w(&self, idx: u32) -> Result<(u32, u32), NvmlError>;
    fn set_power_limit_w(&mut self, idx: u32, watts: u32) -> Result<(), NvmlError>;
}

pub struct NvmlReader {
    nvml: Nvml,
    indices: HashSet<u32>,
}

impl NvmlReader {
    /// Read the driver's *default* `power_management_limit` in watts
    /// (ADR-0009 — used only when `power_limit_restore_on_shutdown = true`).
    /// Not part of the `NvmlOps` trait because there's no main-loop or
    /// FakeNvml use case for it; production-only shutdown helper.
    pub fn read_power_limit_default_w(&self, idx: u32) -> Result<u32, NvmlError> {
        if !self.indices.contains(&idx) {
            return Err(NvmlError::ReadFailed {
                idx,
                cause: format!("index {idx} was not registered at init"),
            });
        }
        let device = self
            .nvml
            .device_by_index(idx)
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        let mw = device
            .power_management_limit_default()
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        Ok(mw / 1000)
    }

    /// Initialise the NVML library (runtime dlopen of `libnvidia-ml.so.1`)
    /// and verify every requested index is present. Failure here is fatal
    /// at startup (ADR-0003 step 3).
    pub fn init(indices: &[u32]) -> Result<Self, NvmlError> {
        let nvml = Nvml::init().map_err(|e| NvmlError::Init(e.to_string()))?;
        let count = nvml
            .device_count()
            .map_err(|e| NvmlError::Init(format!("device_count: {e}")))?;
        for &idx in indices {
            if idx >= count {
                return Err(NvmlError::IndexOutOfRange(idx, count));
            }
        }
        Ok(Self {
            nvml,
            indices: indices.iter().copied().collect(),
        })
    }
}

impl NvmlOps for NvmlReader {
    fn read_temp(&self, idx: u32) -> Result<Celsius, NvmlError> {
        if !self.indices.contains(&idx) {
            return Err(NvmlError::ReadFailed {
                idx,
                cause: format!("index {idx} was not registered at init"),
            });
        }
        let device = self
            .nvml
            .device_by_index(idx)
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        let raw =
            device
                .temperature(TemperatureSensor::Gpu)
                .map_err(|e| NvmlError::ReadFailed {
                    idx,
                    cause: e.to_string(),
                })?;
        check_bounds(idx, raw as i32)
    }

    /// Reads `power_management_limit` (mW), divides by 1000 to return W.
    /// **Not** `enforced_power_limit` — see plan §2 / ADR-0008: enforced
    /// can drop below the management limit when hardware thermal protection
    /// kicks in, and reading it would make the daemon fight self-protection.
    fn read_power_limit_w(&self, idx: u32) -> Result<u32, NvmlError> {
        if !self.indices.contains(&idx) {
            return Err(NvmlError::ReadFailed {
                idx,
                cause: format!("index {idx} was not registered at init"),
            });
        }
        let device = self
            .nvml
            .device_by_index(idx)
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        let mw = device
            .power_management_limit()
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        Ok(mw / 1000)
    }

    fn power_limit_constraints_w(&self, idx: u32) -> Result<(u32, u32), NvmlError> {
        if !self.indices.contains(&idx) {
            return Err(NvmlError::ReadFailed {
                idx,
                cause: format!("index {idx} was not registered at init"),
            });
        }
        let device = self
            .nvml
            .device_by_index(idx)
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        let c = device
            .power_management_limit_constraints()
            .map_err(|e| NvmlError::ReadFailed {
                idx,
                cause: e.to_string(),
            })?;
        Ok((c.min_limit / 1000, c.max_limit / 1000))
    }

    fn set_power_limit_w(&mut self, idx: u32, watts: u32) -> Result<(), NvmlError> {
        if !self.indices.contains(&idx) {
            return Err(NvmlError::SetFailed {
                idx,
                cause: format!("index {idx} was not registered at init"),
            });
        }
        let mut device = self
            .nvml
            .device_by_index(idx)
            .map_err(|e| NvmlError::SetFailed {
                idx,
                cause: e.to_string(),
            })?;
        device
            .set_power_management_limit(watts.saturating_mul(1000))
            .map_err(|e| NvmlError::SetFailed {
                idx,
                cause: e.to_string(),
            })
    }
}

/// Shared sanity-bounds gate — see module docstring.
fn check_bounds(idx: u32, temp: i32) -> Result<Celsius, NvmlError> {
    if !(SANITY_MIN_C..=SANITY_MAX_C).contains(&temp) {
        return Err(NvmlError::OutOfBounds { idx, temp });
    }
    Ok(Celsius(temp as i16))
}

#[cfg(test)]
pub struct FakeNvml {
    pub temps: std::collections::HashMap<u32, Celsius>,
    pub power_limits_w: std::cell::RefCell<std::collections::HashMap<u32, u32>>,
    pub constraints_w: std::collections::HashMap<u32, (u32, u32)>,
    pub fail_after: Option<u32>,
    pub power_read_fails: std::cell::Cell<bool>,
    pub power_set_fails: std::cell::Cell<bool>,
    reads: std::cell::Cell<u32>,
}

#[cfg(test)]
impl FakeNvml {
    pub fn new(temps: std::collections::HashMap<u32, Celsius>) -> Self {
        Self {
            temps,
            power_limits_w: std::cell::RefCell::new(std::collections::HashMap::new()),
            constraints_w: std::collections::HashMap::new(),
            fail_after: None,
            power_read_fails: std::cell::Cell::new(false),
            power_set_fails: std::cell::Cell::new(false),
            reads: std::cell::Cell::new(0),
        }
    }

    pub fn with_fail_after(
        temps: std::collections::HashMap<u32, Celsius>,
        fail_after: u32,
    ) -> Self {
        Self {
            temps,
            power_limits_w: std::cell::RefCell::new(std::collections::HashMap::new()),
            constraints_w: std::collections::HashMap::new(),
            fail_after: Some(fail_after),
            power_read_fails: std::cell::Cell::new(false),
            power_set_fails: std::cell::Cell::new(false),
            reads: std::cell::Cell::new(0),
        }
    }

    /// Pre-load a power-management state for a GPU: current limit + constraints.
    pub fn with_power(mut self, idx: u32, current_w: u32, min_w: u32, max_w: u32) -> Self {
        self.power_limits_w.borrow_mut().insert(idx, current_w);
        self.constraints_w.insert(idx, (min_w, max_w));
        self
    }

    /// Simulate an external rewrite of the limit (e.g. `nvidia-smi -pl`).
    /// Useful for drift-detection tests.
    pub fn external_drift(&self, idx: u32, new_w: u32) {
        self.power_limits_w.borrow_mut().insert(idx, new_w);
    }
}

#[cfg(test)]
impl NvmlOps for FakeNvml {
    fn read_temp(&self, idx: u32) -> Result<Celsius, NvmlError> {
        if let Some(limit) = self.fail_after {
            let n = self.reads.get();
            if n >= limit {
                return Err(NvmlError::ReadFailed {
                    idx,
                    cause: format!("simulated failure after {limit} successful reads"),
                });
            }
            self.reads.set(n + 1);
        }
        let temp = self
            .temps
            .get(&idx)
            .copied()
            .ok_or_else(|| NvmlError::ReadFailed {
                idx,
                cause: format!("FakeNvml has no entry for index {idx}"),
            })?;
        check_bounds(idx, i32::from(temp.0))
    }

    fn read_power_limit_w(&self, idx: u32) -> Result<u32, NvmlError> {
        if self.power_read_fails.get() {
            return Err(NvmlError::ReadFailed {
                idx,
                cause: "simulated power-limit read failure".into(),
            });
        }
        self.power_limits_w
            .borrow()
            .get(&idx)
            .copied()
            .ok_or_else(|| NvmlError::ReadFailed {
                idx,
                cause: format!("FakeNvml has no power-limit entry for index {idx}"),
            })
    }

    fn power_limit_constraints_w(&self, idx: u32) -> Result<(u32, u32), NvmlError> {
        self.constraints_w
            .get(&idx)
            .copied()
            .ok_or_else(|| NvmlError::ReadFailed {
                idx,
                cause: format!("FakeNvml has no constraints entry for index {idx}"),
            })
    }

    fn set_power_limit_w(&mut self, idx: u32, watts: u32) -> Result<(), NvmlError> {
        if self.power_set_fails.get() {
            return Err(NvmlError::SetFailed {
                idx,
                cause: "simulated power-limit set failure".into(),
            });
        }
        self.power_limits_w.borrow_mut().insert(idx, watts);
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temps(pairs: &[(u32, i16)]) -> HashMap<u32, Celsius> {
        pairs.iter().map(|&(i, t)| (i, Celsius(t))).collect()
    }

    #[test]
    fn fake_happy_path_returns_configured_temp() {
        let f = FakeNvml::new(temps(&[(0, 55), (1, 70)]));
        assert_eq!(f.read_temp(0).unwrap(), Celsius(55));
        assert_eq!(f.read_temp(1).unwrap(), Celsius(70));
    }

    #[test]
    fn fake_out_of_bounds_low_zero_is_error() {
        // PLAN.md L293: 0°C is OUT-OF-BOUNDS, not "GPU is cold".
        let f = FakeNvml::new(temps(&[(0, 0)]));
        match f.read_temp(0).unwrap_err() {
            NvmlError::OutOfBounds { idx, temp } => {
                assert_eq!(idx, 0);
                assert_eq!(temp, 0);
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn fake_out_of_bounds_low_just_below_floor_is_error() {
        // 4 < SANITY_MIN_C (5) → still rejected.
        let f = FakeNvml::new(temps(&[(0, 4)]));
        assert!(matches!(
            f.read_temp(0).unwrap_err(),
            NvmlError::OutOfBounds { temp: 4, .. }
        ));
    }

    #[test]
    fn fake_floor_value_is_accepted() {
        let f = FakeNvml::new(temps(&[(0, 5)]));
        assert_eq!(f.read_temp(0).unwrap(), Celsius(5));
    }

    #[test]
    fn fake_ceiling_value_is_accepted() {
        let f = FakeNvml::new(temps(&[(0, 110)]));
        assert_eq!(f.read_temp(0).unwrap(), Celsius(110));
    }

    #[test]
    fn fake_out_of_bounds_high_is_error() {
        let f = FakeNvml::new(temps(&[(0, 120)]));
        match f.read_temp(0).unwrap_err() {
            NvmlError::OutOfBounds { idx, temp } => {
                assert_eq!(idx, 0);
                assert_eq!(temp, 120);
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn fake_fail_after_returns_read_failed_after_n_reads() {
        let f = FakeNvml::with_fail_after(temps(&[(0, 55)]), 2);
        assert_eq!(f.read_temp(0).unwrap(), Celsius(55));
        assert_eq!(f.read_temp(0).unwrap(), Celsius(55));
        match f.read_temp(0).unwrap_err() {
            NvmlError::ReadFailed { idx, .. } => assert_eq!(idx, 0),
            other => panic!("expected ReadFailed, got {other:?}"),
        }
    }

    #[test]
    fn fake_unknown_index_returns_read_failed() {
        let f = FakeNvml::new(temps(&[(0, 55)]));
        match f.read_temp(7).unwrap_err() {
            NvmlError::ReadFailed { idx, .. } => assert_eq!(idx, 7),
            other => panic!("expected ReadFailed, got {other:?}"),
        }
    }

    // Power Limit (ADR-0008) — FakeNvml round-trip + drift simulation.

    #[test]
    fn fake_set_then_read_round_trip() {
        let mut f = FakeNvml::new(temps(&[(0, 55)])).with_power(0, 250, 125, 300);
        f.set_power_limit_w(0, 200).unwrap();
        assert_eq!(f.read_power_limit_w(0).unwrap(), 200);
    }

    #[test]
    fn fake_constraints_returned_as_watts() {
        let f = FakeNvml::new(temps(&[(0, 55)])).with_power(0, 250, 125, 300);
        assert_eq!(f.power_limit_constraints_w(0).unwrap(), (125, 300));
    }

    #[test]
    fn fake_external_drift_visible_to_next_read() {
        // Models `nvidia-smi -pl 350` happening between two daemon checks.
        let f = FakeNvml::new(temps(&[(0, 55)])).with_power(0, 250, 125, 400);
        assert_eq!(f.read_power_limit_w(0).unwrap(), 250);
        f.external_drift(0, 350);
        assert_eq!(f.read_power_limit_w(0).unwrap(), 350);
    }

    #[test]
    fn fake_set_failure_propagates_set_failed_variant() {
        let mut f = FakeNvml::new(temps(&[(0, 55)])).with_power(0, 250, 125, 300);
        f.power_set_fails.set(true);
        match f.set_power_limit_w(0, 200).unwrap_err() {
            NvmlError::SetFailed { idx, .. } => assert_eq!(idx, 0),
            other => panic!("expected SetFailed, got {other:?}"),
        }
    }

    #[test]
    fn fake_read_failure_propagates_read_failed_variant() {
        let f = FakeNvml::new(temps(&[(0, 55)])).with_power(0, 250, 125, 300);
        f.power_read_fails.set(true);
        match f.read_power_limit_w(0).unwrap_err() {
            NvmlError::ReadFailed { idx, .. } => assert_eq!(idx, 0),
            other => panic!("expected ReadFailed, got {other:?}"),
        }
    }
}
