//! Fan I/O lifecycle and pure health classification.
//!
//! See PLAN.md §fan.rs and §"Fan restore policy" (L301–L323). The `Fan`
//! struct owns the sysfs lifecycle for a single PWM channel:
//! `take_manual_control` snapshots the kernel's current `pwm_enable` value
//! before forcing manual mode, and `restore` rewrites that exact snapshot —
//! never a hard-coded `"2"` (`pwm_enable=2` is chip-specific; on Nuvoton it
//! means "thermal cruise", not "BIOS state"). `check_health` is pure and is
//! exposed as both a free function (for direct testing) and an `&self`
//! method (which derives `in_spin_up` from internal state).
//!
//! Per ADR-0006 no traits are introduced — sysfs I/O goes through `std::fs`
//! and tests point `Fan::open` at a tempfile-backed hwmon directory rather
//! than mocking. Per ADR-0007 there is no `unsafe` here.

use crate::units::{Pct, Pwm};
use crate::watchdog::FanHealth;
use std::path::PathBuf;
use thiserror::Error;

/// Threshold above which a single `set()` jump enters spin-up grace.
///
/// PLAN.md L308 hard-codes this rather than making it per-fan config —
/// promote to a config field if a real fan ever needs a different value.
pub const SPIN_UP_DELTA_PCT: u8 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpinUpState {
    Idle,
    Active { elapsed_s: u32 },
}

pub struct Fan {
    hwmon_path: PathBuf,
    pwm_channel: u32,
    min_rpm: u32,
    max_rpm: u32,
    spin_up_grace_s: u32,
    pwm_enable_snapshot: Option<String>,
    last_set_pct: Option<Pct>,
    spin_up_state: SpinUpState,
}

#[derive(Debug, Error)]
pub enum FanError {
    #[error("failed to read {path:?}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write {path:?}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse RPM from {path:?}: {raw:?}")]
    ParseRpm { path: PathBuf, raw: String },
    #[error("restore called without prior take_manual_control")]
    NoSnapshot,
}

impl Fan {
    /// Pure constructor. sysfs path validation is the main loop's job
    /// (Wave 3 R2), not ours.
    pub fn open(
        hwmon_path: PathBuf,
        pwm_channel: u32,
        min_rpm: u32,
        max_rpm: u32,
        spin_up_grace_s: u32,
    ) -> Self {
        Self {
            hwmon_path,
            pwm_channel,
            min_rpm,
            max_rpm,
            spin_up_grace_s,
            pwm_enable_snapshot: None,
            last_set_pct: None,
            spin_up_state: SpinUpState::Idle,
        }
    }

    fn pwm_path(&self) -> PathBuf {
        self.hwmon_path.join(format!("pwm{}", self.pwm_channel))
    }

    fn pwm_enable_path(&self) -> PathBuf {
        self.hwmon_path
            .join(format!("pwm{}_enable", self.pwm_channel))
    }

    fn fan_input_path(&self) -> PathBuf {
        self.hwmon_path
            .join(format!("fan{}_input", self.pwm_channel))
    }

    fn write_sysfs(path: PathBuf, contents: &str) -> Result<(), FanError> {
        std::fs::write(&path, contents).map_err(|source| FanError::Write { path, source })
    }

    /// Snapshot the current `pwm_enable` value verbatim, then force manual
    /// mode (`"1"`) and enter spin-up grace. The snapshot is whatever bytes
    /// the kernel returned (after trim) — round-tripped exactly by
    /// `restore()`. Hard-coding the restore value would silently break any
    /// chip whose "BIOS state" is not `2`.
    pub fn take_manual_control(&mut self) -> Result<(), FanError> {
        let path = self.pwm_enable_path();
        let raw = std::fs::read_to_string(&path).map_err(|source| FanError::Read {
            path: path.clone(),
            source,
        })?;
        let snapshot = raw.trim().to_string();
        Self::write_sysfs(self.pwm_enable_path(), "1")?;
        self.pwm_enable_snapshot = Some(snapshot);
        self.spin_up_state = SpinUpState::Active { elapsed_s: 0 };
        Ok(())
    }

    pub fn set(&mut self, pct: Pct) -> Result<(), FanError> {
        let pwm = Pwm::from(pct);
        Self::write_sysfs(self.pwm_path(), &pwm.0.to_string())?;
        if let Some(last) = self.last_set_pct {
            let delta = pct.0.saturating_sub(last.0);
            if delta > SPIN_UP_DELTA_PCT {
                self.spin_up_state = SpinUpState::Active { elapsed_s: 0 };
            }
        }
        self.last_set_pct = Some(pct);
        Ok(())
    }

    /// Fault-handling primitive — bypasses curve/clamp and slams to 100%.
    /// Sets `last_set_pct = Pct(100)` so a follow-up `set(small)` does not
    /// re-trigger spin-up via the delta path; spin-up is entered here
    /// because a max-duty jump is itself a large duty change.
    pub fn set_max(&mut self) -> Result<(), FanError> {
        Self::write_sysfs(self.pwm_path(), "255")?;
        self.last_set_pct = Some(Pct(100));
        self.spin_up_state = SpinUpState::Active { elapsed_s: 0 };
        Ok(())
    }

    pub fn read_rpm(&self) -> Result<u32, FanError> {
        let path = self.fan_input_path();
        let raw = std::fs::read_to_string(&path).map_err(|source| FanError::Read {
            path: path.clone(),
            source,
        })?;
        let parsed = raw.trim().parse::<u32>();
        parsed.map_err(|_| FanError::ParseRpm { path, raw })
    }

    /// Round-trip the `pwm_enable` snapshot taken at `take_manual_control`
    /// time. Errors with `NoSnapshot` if `take_manual_control` was never
    /// called — `restore` must never invent a value.
    pub fn restore(&mut self) -> Result<(), FanError> {
        let snapshot = self
            .pwm_enable_snapshot
            .as_ref()
            .ok_or(FanError::NoSnapshot)?
            .clone();
        Self::write_sysfs(self.pwm_enable_path(), &snapshot)?;
        Ok(())
    }

    /// Advance the spin-up state machine after a poll iteration's RPM read.
    /// Exits the moment RPM rises above `min_rpm` (fan caught air) or the
    /// grace window expires (next poll's `Stalled` is real, per PLAN.md
    /// L308). No-op when not in spin-up.
    pub fn tick_spin_up(&mut self, rpm: u32, elapsed_s: u32) {
        if matches!(self.spin_up_state, SpinUpState::Active { .. }) {
            if rpm > self.min_rpm || elapsed_s >= self.spin_up_grace_s {
                self.spin_up_state = SpinUpState::Idle;
            } else {
                self.spin_up_state = SpinUpState::Active { elapsed_s };
            }
        }
    }

    pub fn check_health(&self, rpm: u32) -> FanHealth {
        let in_spin_up = matches!(self.spin_up_state, SpinUpState::Active { .. });
        check_health(rpm, self.min_rpm, self.max_rpm, in_spin_up)
    }
}

/// Pure classification of an RPM reading. Overspeed is reported even during
/// spin-up (a sensor over the max is a real anomaly); only `Stalled` is
/// masked to `SpinUp` while the grace window is active.
pub fn check_health(rpm: u32, min: u32, max: u32, in_spin_up: bool) -> FanHealth {
    if rpm > max {
        FanHealth::Overspeed
    } else if rpm == 0 || rpm < min {
        if in_spin_up {
            FanHealth::SpinUp
        } else {
            FanHealth::Stalled
        }
    } else {
        FanHealth::Ok
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
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn make_hwmon_with_channel(dir: &Path, n: u32, pwm_enable: &str) -> PathBuf {
        fs::write(dir.join(format!("pwm{n}")), "0\n").unwrap();
        fs::write(dir.join(format!("pwm{n}_enable")), pwm_enable).unwrap();
        fs::write(dir.join(format!("fan{n}_input")), "1500\n").unwrap();
        dir.to_path_buf()
    }

    // ---- check_health (pure) ----

    #[test]
    fn check_health_ok_for_in_band_rpm() {
        assert_eq!(check_health(1500, 200, 3000, false), FanHealth::Ok);
    }

    #[test]
    fn check_health_zero_is_stalled() {
        assert_eq!(check_health(0, 200, 3000, false), FanHealth::Stalled);
    }

    #[test]
    fn check_health_below_min_is_stalled() {
        assert_eq!(check_health(100, 200, 3000, false), FanHealth::Stalled);
    }

    #[test]
    fn check_health_above_max_is_overspeed() {
        assert_eq!(check_health(4000, 200, 3000, false), FanHealth::Overspeed);
    }

    #[test]
    fn check_health_below_min_during_spin_up_is_spin_up() {
        assert_eq!(check_health(100, 200, 3000, true), FanHealth::SpinUp);
    }

    #[test]
    fn check_health_zero_during_spin_up_is_spin_up() {
        assert_eq!(check_health(0, 200, 3000, true), FanHealth::SpinUp);
    }

    #[test]
    fn check_health_overspeed_not_masked_by_spin_up() {
        // PLAN.md L307: overspeed during spin-up is still real.
        assert_eq!(check_health(4000, 200, 3000, true), FanHealth::Overspeed);
    }

    #[test]
    fn check_health_min_boundary_is_ok() {
        // Spec: "< min_rpm" is stalled; equal is fine.
        assert_eq!(check_health(200, 200, 3000, false), FanHealth::Ok);
    }

    #[test]
    fn check_health_max_boundary_is_ok() {
        // Spec: "> max_rpm" is overspeed; equal is fine.
        assert_eq!(check_health(3000, 200, 3000, false), FanHealth::Ok);
    }

    // ---- sysfs lifecycle ----

    #[test]
    fn take_manual_control_snapshots_and_writes_one() {
        // PLAN.md L747: snapshot value "5" specifically — the canary that
        // catches a hard-coded "2" regression in restore().
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();

        let pwm_enable = fs::read_to_string(dir.path().join("pwm1_enable")).unwrap();
        assert_eq!(pwm_enable, "1");
        assert_eq!(fan.pwm_enable_snapshot.as_deref(), Some("5"));
    }

    #[test]
    fn restore_round_trips_snapshot() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();
        fan.restore().unwrap();

        let pwm_enable = fs::read_to_string(dir.path().join("pwm1_enable")).unwrap();
        // If you "fix" this by changing the expectation to "2", STOP and
        // re-read PLAN.md §"Fan restore policy" — `pwm_enable=2` is not
        // universally "auto"; Nuvoton boards (the project's reference
        // nct6798) treat 2 as thermal cruise.
        assert_eq!(pwm_enable, "5");
    }

    #[test]
    fn restore_without_take_errors() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        let err = fan.restore().unwrap_err();
        assert!(matches!(err, FanError::NoSnapshot));
    }

    #[test]
    fn set_writes_correct_pwm() {
        // 50% → 127 (PLAN.md canonical: 50 * 255 / 100, integer truncation).
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set(Pct(50)).unwrap();

        let pwm = fs::read_to_string(dir.path().join("pwm1")).unwrap();
        assert_eq!(pwm, "127");
    }

    #[test]
    fn set_zero_writes_zero() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set(Pct(0)).unwrap();

        let pwm = fs::read_to_string(dir.path().join("pwm1")).unwrap();
        assert_eq!(pwm, "0");
    }

    #[test]
    fn set_max_writes_255() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set_max().unwrap();

        let pwm = fs::read_to_string(dir.path().join("pwm1")).unwrap();
        assert_eq!(pwm, "255");
    }

    #[test]
    fn read_rpm_parses_fan_input() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        assert_eq!(fan.read_rpm().unwrap(), 1500);
    }

    #[test]
    fn read_rpm_garbage_errors() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        fs::write(dir.path().join("fan1_input"), "oops").unwrap();
        let fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        let err = fan.read_rpm().unwrap_err();
        assert!(matches!(err, FanError::ParseRpm { .. }));
    }

    // ---- spin-up state machine ----

    #[test]
    fn take_manual_control_enters_spin_up() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();

        // While in spin-up, an RPM below min must surface as SpinUp, not Stalled.
        assert_eq!(fan.check_health(50), FanHealth::SpinUp);
    }

    #[test]
    fn large_set_delta_enters_spin_up() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set(Pct(10)).unwrap(); // first set: no delta history
        fan.set(Pct(40)).unwrap(); // Δ = 30 > 20 → spin-up

        assert_eq!(fan.check_health(50), FanHealth::SpinUp);
    }

    #[test]
    fn small_set_delta_does_not_enter_spin_up() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set(Pct(50)).unwrap();
        fan.set(Pct(60)).unwrap(); // Δ = 10 ≤ 20 → no spin-up

        // Without spin-up masking, rpm < min surfaces as Stalled.
        assert_eq!(fan.check_health(50), FanHealth::Stalled);
    }

    #[test]
    fn tick_spin_up_exits_when_rpm_above_min() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();
        fan.tick_spin_up(300, 0); // 300 > min_rpm=200

        assert_eq!(fan.check_health(50), FanHealth::Stalled);
    }

    #[test]
    fn tick_spin_up_exits_when_grace_expires() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();
        fan.tick_spin_up(0, 10); // elapsed_s == grace_s=10

        assert_eq!(fan.check_health(50), FanHealth::Stalled);
    }

    #[test]
    fn tick_spin_up_stays_active_in_window() {
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "5\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.take_manual_control().unwrap();
        fan.tick_spin_up(0, 5); // 5 < grace_s=10, rpm 0 < min 200

        assert_eq!(fan.check_health(50), FanHealth::SpinUp);
    }

    #[test]
    fn set_max_enters_spin_up_and_records_full_pct() {
        // After set_max(), a subsequent set(small) must not re-trigger
        // spin-up via the delta path (set_max records Pct(100) so
        // set(50).0.saturating_sub(100) == 0).
        let dir = tempdir().unwrap();
        make_hwmon_with_channel(dir.path(), 1, "1\n");
        let mut fan = Fan::open(dir.path().to_path_buf(), 1, 200, 3000, 10);

        fan.set_max().unwrap();
        // set_max enters spin-up directly.
        assert_eq!(fan.check_health(50), FanHealth::SpinUp);
        // Exit spin-up to isolate the next assertion.
        fan.tick_spin_up(300, 0);
        assert_eq!(fan.check_health(50), FanHealth::Stalled);
        // Down-shift after set_max: delta saturates to 0, no spin-up.
        fan.set(Pct(50)).unwrap();
        assert_eq!(fan.check_health(50), FanHealth::Stalled);
    }
}
