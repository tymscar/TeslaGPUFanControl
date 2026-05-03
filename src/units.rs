//! Domain newtypes — pure, no I/O.
//!
//! Maps directly to the language in CONTEXT.md and the contract in PLAN.md
//! §units.rs. Newtypes prevent unit-confusion bugs at the type system level
//! (a `Pwm` cannot accidentally be passed where a `Pct` is required, etc).

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Celsius(pub i16);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pct(pub u8);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pwm(pub u8);

impl From<Pct> for Pwm {
    fn from(pct: Pct) -> Self {
        Pwm((u16::from(pct.0) * 255 / 100) as u8)
    }
}

impl Pct {
    /// Clamp the inner value to `[min, max]`.
    ///
    /// Caller invariant: `min <= max`. Config validation rule G7 enforces this
    /// at startup, so we document the invariant rather than guarding at
    /// runtime. If `min > max` slips through, std's `Ord::clamp` will panic —
    /// which is the desired behaviour under `panic = "abort"` (ADR-0007).
    pub fn clamp(self, min: Pct, max: Pct) -> Pct {
        Pct(self.0.clamp(min.0, max.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_to_pwm_zero() {
        assert_eq!(Pwm::from(Pct(0)), Pwm(0));
    }

    #[test]
    fn pct_to_pwm_fifty() {
        // 50 * 255 / 100 = 127 (integer truncation, not 127.5)
        assert_eq!(Pwm::from(Pct(50)), Pwm(127));
    }

    #[test]
    fn pct_to_pwm_full() {
        assert_eq!(Pwm::from(Pct(100)), Pwm(255));
    }

    #[test]
    fn clamp_low() {
        assert_eq!(Pct(10).clamp(Pct(20), Pct(80)), Pct(20));
    }

    #[test]
    fn clamp_high() {
        assert_eq!(Pct(95).clamp(Pct(20), Pct(80)), Pct(80));
    }

    #[test]
    fn clamp_passthrough() {
        assert_eq!(Pct(50).clamp(Pct(20), Pct(80)), Pct(50));
    }
}
