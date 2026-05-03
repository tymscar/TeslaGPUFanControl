//! Per-GPU fan curve — pure, no I/O.
//!
//! See PLAN.md §curve.rs and CONTEXT.md ("Fan Curve"). Construction validates
//! structural rules G1–G4; range checks (G5/G6) live in config validation.

use crate::units::{Celsius, Pct};
use thiserror::Error;

const MIN_POINTS: usize = 2;
const MAX_POINTS: usize = 10;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CurveError {
    #[error("curve must have at least 2 points (G1), got {0}")]
    TooFewPoints(usize),
    #[error("curve must have at most 10 points (G1), got {0}")]
    TooManyPoints(usize),
    #[error("curve temperatures must be strictly ascending (G2): {prev:?} then {next:?}")]
    NotMonotonicTemp { prev: Celsius, next: Celsius },
    #[error("curve temperatures must be unique (G3): duplicate {0:?}")]
    DuplicateTemp(Celsius),
    #[error("curve percentages must be monotonic non-decreasing (G4): {prev:?} then {next:?}")]
    NotMonotonicPct { prev: Pct, next: Pct },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curve {
    points: Vec<(Celsius, Pct)>,
}

impl Curve {
    pub fn new(points: Vec<(Celsius, Pct)>) -> Result<Self, CurveError> {
        if points.len() < MIN_POINTS {
            return Err(CurveError::TooFewPoints(points.len()));
        }
        if points.len() > MAX_POINTS {
            return Err(CurveError::TooManyPoints(points.len()));
        }
        for window in points.windows(2) {
            let &[(prev_temp, prev_pct), (next_temp, next_pct)] = window else {
                continue;
            };
            if prev_temp == next_temp {
                return Err(CurveError::DuplicateTemp(prev_temp));
            }
            if next_temp < prev_temp {
                return Err(CurveError::NotMonotonicTemp {
                    prev: prev_temp,
                    next: next_temp,
                });
            }
            if next_pct < prev_pct {
                return Err(CurveError::NotMonotonicPct {
                    prev: prev_pct,
                    next: next_pct,
                });
            }
        }
        Ok(Curve { points })
    }

    pub fn points(&self) -> &[(Celsius, Pct)] {
        &self.points
    }

    /// Linear interpolation between adjacent points, clamped at the edges.
    ///
    /// Below the lowest temp returns the first point's pct; above the highest
    /// returns the last point's pct. Constructor invariants guarantee at
    /// least two points and strictly ascending unique temps.
    pub fn evaluate(&self, temp: Celsius) -> Pct {
        // Constructor guarantees at least MIN_POINTS=2 entries; first()/last()
        // would only be None on an empty Vec, which cannot occur here.
        let Some(&first) = self.points.first() else {
            return Pct(100); // Defensive: max fans on impossible state.
        };
        let Some(&last) = self.points.last() else {
            return Pct(100);
        };

        if temp <= first.0 {
            return first.1;
        }
        if temp >= last.0 {
            return last.1;
        }

        for window in self.points.windows(2) {
            let &[(t0, p0), (t1, p1)] = window else {
                continue;
            };
            if temp >= t0 && temp <= t1 {
                if temp == t0 {
                    return p0;
                }
                if temp == t1 {
                    return p1;
                }
                let span_t = i32::from(t1.0) - i32::from(t0.0);
                let span_p = i32::from(p1.0) - i32::from(p0.0);
                let offset = i32::from(temp.0) - i32::from(t0.0);
                let interp = i32::from(p0.0) + (offset * span_p) / span_t;
                return Pct(interp as u8);
            }
        }

        last.1
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn pt(t: i16, p: u8) -> (Celsius, Pct) {
        (Celsius(t), Pct(p))
    }

    #[test]
    fn rejects_zero_points() {
        assert_eq!(Curve::new(vec![]), Err(CurveError::TooFewPoints(0)));
    }

    #[test]
    fn rejects_one_point() {
        assert_eq!(
            Curve::new(vec![pt(40, 20)]),
            Err(CurveError::TooFewPoints(1))
        );
    }

    #[test]
    fn rejects_eleven_points() {
        let pts: Vec<_> = (0i16..11).map(|i| pt(i * 10, (i as u8) * 10)).collect();
        assert_eq!(Curve::new(pts), Err(CurveError::TooManyPoints(11)));
    }

    #[test]
    fn rejects_out_of_order_temps() {
        let result = Curve::new(vec![pt(40, 20), pt(30, 30)]);
        assert!(matches!(result, Err(CurveError::NotMonotonicTemp { .. })));
    }

    #[test]
    fn rejects_duplicate_temps() {
        let result = Curve::new(vec![pt(40, 20), pt(40, 30)]);
        assert_eq!(result, Err(CurveError::DuplicateTemp(Celsius(40))));
    }

    #[test]
    fn rejects_decreasing_pct() {
        let result = Curve::new(vec![pt(40, 50), pt(60, 40)]);
        assert!(matches!(result, Err(CurveError::NotMonotonicPct { .. })));
    }

    #[test]
    fn accepts_canonical_curve() {
        let c = Curve::new(vec![pt(40, 20), pt(55, 40), pt(70, 80), pt(80, 100)]);
        assert!(c.is_ok());
    }

    #[test]
    fn evaluate_midpoint_interpolates() {
        // PLAN.md example: 40:20, 60:60 → at 50 returns 40
        let c = Curve::new(vec![pt(40, 20), pt(60, 60)]).unwrap();
        assert_eq!(c.evaluate(Celsius(50)), Pct(40));
    }

    #[test]
    fn evaluate_clamps_below() {
        let c = Curve::new(vec![pt(40, 20), pt(60, 60)]).unwrap();
        assert_eq!(c.evaluate(Celsius(0)), Pct(20));
        assert_eq!(c.evaluate(Celsius(40)), Pct(20));
    }

    #[test]
    fn evaluate_clamps_above() {
        let c = Curve::new(vec![pt(40, 20), pt(60, 60)]).unwrap();
        assert_eq!(c.evaluate(Celsius(100)), Pct(60));
    }

    #[test]
    fn evaluate_returns_exact_at_defined_point() {
        let c = Curve::new(vec![pt(40, 20), pt(55, 40), pt(70, 80), pt(80, 100)]).unwrap();
        assert_eq!(c.evaluate(Celsius(40)), Pct(20));
        assert_eq!(c.evaluate(Celsius(55)), Pct(40));
        assert_eq!(c.evaluate(Celsius(70)), Pct(80));
        assert_eq!(c.evaluate(Celsius(80)), Pct(100));
    }
}
