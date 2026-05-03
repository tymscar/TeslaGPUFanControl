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
}

pub trait TempReader {
    fn read_temp(&self, idx: u32) -> Result<Celsius, NvmlError>;
}

pub struct NvmlReader {
    nvml: Nvml,
    indices: HashSet<u32>,
}

impl NvmlReader {
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

impl TempReader for NvmlReader {
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
    pub fail_after: Option<u32>,
    reads: std::cell::Cell<u32>,
}

#[cfg(test)]
impl FakeNvml {
    pub fn new(temps: std::collections::HashMap<u32, Celsius>) -> Self {
        Self {
            temps,
            fail_after: None,
            reads: std::cell::Cell::new(0),
        }
    }

    pub fn with_fail_after(
        temps: std::collections::HashMap<u32, Celsius>,
        fail_after: u32,
    ) -> Self {
        Self {
            temps,
            fail_after: Some(fail_after),
            reads: std::cell::Cell::new(0),
        }
    }
}

#[cfg(test)]
impl TempReader for FakeNvml {
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
}
