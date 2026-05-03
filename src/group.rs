//! Cooling-group target computation — pure, no I/O.
//!
//! See PLAN.md §group.rs and CONTEXT.md ("Cooling Group"). The clamp-per-GPU
//! happens BEFORE the max() — this is load-bearing. Each GPU's
//! `[min_fan_pct, max_fan_pct]` band caps that GPU's contribution; the group
//! target is then the maximum of those bounded contributions.

use crate::curve::Curve;
use crate::units::{Celsius, Pct};
use std::collections::HashMap;

pub type GpuId = String;
pub type FanId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoolingGroup {
    pub id: String,
    pub gpus: Vec<GpuId>,
    pub fans: Vec<FanId>,
}

/// Per-GPU view that `target_pct` needs.
///
/// Defined here rather than imported from `config.rs` to keep this module
/// pure-with-no-config-deps and to make tests trivial. `config::GpuConfig`
/// can produce one of these via a `&` reference field projection if needed.
#[derive(Debug, Clone)]
pub struct GpuView<'a> {
    pub curve: &'a Curve,
    pub min_fan_pct: Pct,
    pub max_fan_pct: Pct,
}

/// For each GPU in the group: evaluate the curve at the GPU's temperature,
/// clamp to `[min_fan_pct, max_fan_pct]`, then take the max() across GPUs.
///
/// Caller invariant: every `id` in `group.gpus` exists as a key in both
/// `temps` and `gpus`. Config validation rule S7 enforces this at startup.
/// On a missing key the GPU contributes `Pct(0)` (i.e. is ignored). This
/// is the safer-than-panic choice under panic = abort: an absent ID at
/// runtime is recoverable; a panic in the main loop is not.
pub fn target_pct(
    group: &CoolingGroup,
    temps: &HashMap<GpuId, Celsius>,
    gpus: &HashMap<GpuId, GpuView<'_>>,
) -> Pct {
    let mut target = Pct(0);
    for gpu_id in &group.gpus {
        let Some(temp) = temps.get(gpu_id) else {
            continue;
        };
        let Some(view) = gpus.get(gpu_id) else {
            continue;
        };
        let raw = view.curve.evaluate(*temp);
        let bounded = raw.clamp(view.min_fan_pct, view.max_fan_pct);
        if bounded > target {
            target = bounded;
        }
    }
    target
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn curve_simple() -> Curve {
        Curve::new(vec![
            (Celsius(40), Pct(20)),
            (Celsius(55), Pct(40)),
            (Celsius(70), Pct(80)),
            (Celsius(80), Pct(100)),
        ])
        .unwrap()
    }

    #[test]
    fn single_gpu_target_is_clamped_curve_value() {
        let curve = curve_simple();
        let group = CoolingGroup {
            id: "main".into(),
            gpus: vec!["0".into()],
            fans: vec!["f0".into()],
        };
        let mut temps = HashMap::new();
        temps.insert("0".into(), Celsius(70));
        let mut gpus = HashMap::new();
        gpus.insert(
            "0".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(100),
            },
        );

        // curve(70) = 80; clamp(80, 20, 100) = 80
        assert_eq!(target_pct(&group, &temps, &gpus), Pct(80));
    }

    #[test]
    fn two_gpus_hot_one_wins() {
        // CONTEXT.md example: GPU 0 at 80°C, GPU 1 at 35°C → curve(80)=100 wins
        let curve = curve_simple();
        let group = CoolingGroup {
            id: "shared".into(),
            gpus: vec!["0".into(), "1".into()],
            fans: vec!["f0".into()],
        };
        let mut temps = HashMap::new();
        temps.insert("0".into(), Celsius(80));
        temps.insert("1".into(), Celsius(35));
        let mut gpus = HashMap::new();
        gpus.insert(
            "0".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(100),
            },
        );
        gpus.insert(
            "1".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(100),
            },
        );
        assert_eq!(target_pct(&group, &temps, &gpus), Pct(100));
    }

    #[test]
    fn quiet_gpus_min_floor_raises_group_target() {
        // Quiet idle GPU with min_fan_pct=30 alongside hot GPU below 30.
        // The hot GPU's bounded contribution is curve(35)=20 capped to its own
        // band [20,100] => 20, but the quiet GPU has min=30 so curve(30)=20
        // clamps UP to 30 → group target is 30.
        let curve = Curve::new(vec![(Celsius(30), Pct(20)), (Celsius(80), Pct(100))]).unwrap();
        let group = CoolingGroup {
            id: "shared".into(),
            gpus: vec!["quiet".into(), "low".into()],
            fans: vec!["f0".into()],
        };
        let mut temps = HashMap::new();
        temps.insert("quiet".into(), Celsius(30));
        temps.insert("low".into(), Celsius(30));
        let mut gpus = HashMap::new();
        gpus.insert(
            "quiet".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(30),
                max_fan_pct: Pct(100),
            },
        );
        gpus.insert(
            "low".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(100),
            },
        );
        assert_eq!(target_pct(&group, &temps, &gpus), Pct(30));
    }

    #[test]
    fn max_fan_pct_caps_own_contribution_only() {
        // GPU A capped at 50% even though hot. GPU B uncapped, also hot.
        // Group target = max(clamp(curve(80), 20, 50), clamp(curve(80), 20, 100))
        //              = max(50, 100) = 100. The cap on A does not limit B.
        let curve = curve_simple();
        let group = CoolingGroup {
            id: "shared".into(),
            gpus: vec!["a".into(), "b".into()],
            fans: vec!["f0".into()],
        };
        let mut temps = HashMap::new();
        temps.insert("a".into(), Celsius(80));
        temps.insert("b".into(), Celsius(80));
        let mut gpus = HashMap::new();
        gpus.insert(
            "a".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(50),
            },
        );
        gpus.insert(
            "b".to_string(),
            GpuView {
                curve: &curve,
                min_fan_pct: Pct(20),
                max_fan_pct: Pct(100),
            },
        );
        assert_eq!(target_pct(&group, &temps, &gpus), Pct(100));
    }
}
