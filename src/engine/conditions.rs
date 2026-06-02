// User-configurable structured trigger rules. A complement to the Rhai
// scripting layer (engine/scripting.rs): same augment-only safety boundary,
// but expressed as serde data the UI can build with dropdowns instead of
// code. A rule has a scope (all zones or a named subset), a boolean
// condition tree over weather + per-zone-soil metrics, and an action
// (skip / extend / adjust the watering multiplier).
//
// SAFETY BOUNDARY (identical to scripting): these run in
// `decide_per_zone` AFTER the deterministic safety + weather gates, and
// ONLY when those gates left the zone running. So a rule can ADD a skip,
// shrink the run, or extend it — it can never clear a freeze / wind /
// restriction / rain gate or force a run. `AdjustMultiplier` is hard-
// clamped to [0.5, 1.5] in code here, never trusted from config. A metric
// with no data (e.g. ZoneSoilPct when the probe is offline) compares as
// `false`, so missing data never *causes* a skip via a custom rule.

use serde::{Deserialize, Serialize};

use crate::engine::skip_rules::{Inputs, ZoneSoil};
use crate::ha::snapshot::RuleEval;

fn default_true() -> bool {
    true
}

/// A value a condition can read. Most are global (one value per refresh);
/// `ZoneSoilPct` resolves against the zone being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    RainProbTomorrow,
    RainNext4hIn,
    RainTodayIn,
    Rain3dayWeightedIn,
    WindNowMph,
    WindMaxTodayMph,
    TempNowF,
    TempMin24hF,
    TempMax3dayF,
    HumidityNowPct,
    DaysSinceRain,
    /// Per-zone soil moisture %. `None` (probe offline / unassigned) → the
    /// comparison is `false`.
    ZoneSoilPct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

impl CmpOp {
    fn apply(self, a: f64, b: f64) -> bool {
        match self {
            CmpOp::Gt => a > b,
            CmpOp::Gte => a >= b,
            CmpOp::Lt => a < b,
            CmpOp::Lte => a <= b,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Gt => ">",
            CmpOp::Gte => "≥",
            CmpOp::Lt => "<",
            CmpOp::Lte => "≤",
        }
    }
}

/// A boolean condition tree. Externally tagged so each node is a clean,
/// unambiguous JSON object (no internal-tag sequence pitfalls). Empty
/// `All` is vacuously true; empty `Any` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConditionExpr {
    Compare {
        metric: Metric,
        op: CmpOp,
        value: f64,
    },
    All(Vec<ConditionExpr>),
    Any(Vec<ConditionExpr>),
    Not(Box<ConditionExpr>),
}

/// What a fired rule does. Augment-only: no run-forcing variant exists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Skip,
    Extend,
    /// Scale the zone's run. Clamped to [0.5, 1.5] at eval time.
    AdjustMultiplier {
        factor: f64,
    },
}

/// Which zones a rule applies to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    #[default]
    AllZones,
    Zones(Vec<String>),
}

impl RuleScope {
    fn includes(&self, slug: &str) -> bool {
        match self {
            RuleScope::AllZones => true,
            RuleScope::Zones(v) => v.iter().any(|s| s == slug),
        }
    }
}

/// One user rule. Lives in `config.conditions.rules`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConditionRule {
    /// snake_case id; shows in the Rule Lab trace.
    pub id: String,
    /// Display label (defaults to `id` if blank).
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub scope: RuleScope,
    pub condition: ConditionExpr,
    pub action: RuleAction,
}

impl ConditionRule {
    fn label(&self) -> String {
        if self.name.trim().is_empty() {
            self.id.clone()
        } else {
            self.name.clone()
        }
    }
}

/// Per-zone evaluation context. Built once per (zone, refresh).
pub struct ConditionCtx<'a> {
    pub i: &'a Inputs,
    pub zone: &'a ZoneSoil,
}

/// Aggregate effect of a zone's custom rules. Augment-only by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionOutcome {
    /// First skip-action rule that fired: (id, reason).
    pub skip: Option<(String, String)>,
    /// Product of every fired AdjustMultiplier, clamped to [0.5, 1.5].
    pub multiplier: f64,
    /// Any fired Extend action.
    pub extend: bool,
    /// Provenance for every enabled, in-scope rule walked.
    pub fired: Vec<RuleEval>,
}

impl Default for ConditionOutcome {
    fn default() -> Self {
        Self {
            skip: None,
            multiplier: 1.0,
            extend: false,
            fired: Vec::new(),
        }
    }
}

fn metric_value(m: Metric, i: &Inputs, zone: &ZoneSoil) -> Option<f64> {
    Some(match m {
        Metric::RainProbTomorrow => i.rain_tomorrow_prob_pct as f64,
        Metric::RainNext4hIn => i.rain_next_4h_in,
        Metric::RainTodayIn => i.rain_today_in,
        Metric::Rain3dayWeightedIn => i.rain_3day_weighted_in,
        Metric::WindNowMph => i.wind_now_mph,
        Metric::WindMaxTodayMph => i.wind_max_today_mph,
        Metric::TempNowF => i.temp_now_f,
        Metric::TempMin24hF => i.temp_min_24h_f,
        Metric::TempMax3dayF => i.temp_max_3day_f,
        Metric::HumidityNowPct => i.humidity_now_pct,
        Metric::DaysSinceRain => i.days_since_significant_rain as f64,
        // None when the probe is offline / unassigned → compare false.
        Metric::ZoneSoilPct => return zone.pct,
    })
}

/// Evaluate one condition tree against a zone context. A `Compare` over a
/// metric with no value is `false` (fail-safe).
pub fn eval_expr(e: &ConditionExpr, ctx: &ConditionCtx) -> bool {
    match e {
        ConditionExpr::Compare { metric, op, value } => {
            match metric_value(*metric, ctx.i, ctx.zone) {
                Some(v) => op.apply(v, *value),
                None => false,
            }
        }
        ConditionExpr::All(xs) => xs.iter().all(|x| eval_expr(x, ctx)),
        ConditionExpr::Any(xs) => xs.iter().any(|x| eval_expr(x, ctx)),
        ConditionExpr::Not(x) => !eval_expr(x, ctx),
    }
}

/// Run every enabled, in-scope rule for one zone and fold their effects.
/// The first Skip wins (later skips are still recorded but don't change
/// the reason). Multipliers compose then clamp.
pub fn apply_zone_rules(rules: &[ConditionRule], ctx: &ConditionCtx) -> ConditionOutcome {
    let mut out = ConditionOutcome::default();
    for rule in rules {
        if !rule.enabled || !rule.scope.includes(&ctx.zone.slug) {
            continue;
        }
        let fired = eval_expr(&rule.condition, ctx);
        let label = rule.label();
        out.fired.push(RuleEval {
            id: rule.id.clone(),
            label: label.clone(),
            category: "condition".into(),
            detail: describe(&rule.action),
            outcome: if fired { "fired" } else { "passed" }.into(),
            verdict: if fired {
                Some(action_verdict(&rule.action).into())
            } else {
                None
            },
        });
        if fired {
            match &rule.action {
                RuleAction::Skip => {
                    if out.skip.is_none() {
                        out.skip = Some((rule.id.clone(), label));
                    }
                }
                RuleAction::Extend => out.extend = true,
                RuleAction::AdjustMultiplier { factor } => {
                    out.multiplier *= factor.clamp(0.5, 1.5);
                }
            }
        }
    }
    out.multiplier = out.multiplier.clamp(0.5, 1.5);
    out
}

fn action_verdict(a: &RuleAction) -> &'static str {
    match a {
        RuleAction::Skip => "skip",
        RuleAction::Extend => "run_extended",
        RuleAction::AdjustMultiplier { .. } => "run",
    }
}

fn describe(a: &RuleAction) -> String {
    match a {
        RuleAction::Skip => "→ skip".into(),
        RuleAction::Extend => "→ extend run".into(),
        RuleAction::AdjustMultiplier { factor } => {
            format!("→ ×{:.2} run", factor.clamp(0.5, 1.5))
        }
    }
}

/// Human-readable single-line summary of a condition tree, for traces /
/// UI fallbacks. Mirrors the operator symbols the editor shows.
pub fn describe_expr(e: &ConditionExpr) -> String {
    match e {
        ConditionExpr::Compare { metric, op, value } => {
            format!("{:?} {} {}", metric, op.symbol(), value)
        }
        ConditionExpr::All(xs) => {
            let parts: Vec<_> = xs.iter().map(describe_expr).collect();
            format!("({})", parts.join(" AND "))
        }
        ConditionExpr::Any(xs) => {
            let parts: Vec<_> = xs.iter().map(describe_expr).collect();
            format!("({})", parts.join(" OR "))
        }
        ConditionExpr::Not(x) => format!("NOT {}", describe_expr(x)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(slug: &str, pct: Option<f64>) -> ZoneSoil {
        ZoneSoil {
            slug: slug.into(),
            name: slug.into(),
            pct,
            saturation_pct: 70.0,
            target_min_pct: 30.0,
        }
    }

    fn ctx_for<'a>(i: &'a Inputs, z: &'a ZoneSoil) -> ConditionCtx<'a> {
        ConditionCtx { i, zone: z }
    }

    fn rule(id: &str, condition: ConditionExpr, action: RuleAction) -> ConditionRule {
        ConditionRule {
            id: id.into(),
            name: String::new(),
            enabled: true,
            scope: RuleScope::AllZones,
            condition,
            action,
        }
    }

    #[test]
    fn compare_ops() {
        let i = Inputs {
            wind_now_mph: 12.0,
            ..Default::default()
        };
        let z = zone("a", None);
        let c = ctx_for(&i, &z);
        let cmp = |op| {
            eval_expr(
                &ConditionExpr::Compare {
                    metric: Metric::WindNowMph,
                    op,
                    value: 10.0,
                },
                &c,
            )
        };
        assert!(cmp(CmpOp::Gt));
        assert!(cmp(CmpOp::Gte));
        assert!(!cmp(CmpOp::Lt));
        assert!(!cmp(CmpOp::Lte));
    }

    #[test]
    fn all_any_not_tree() {
        let i = Inputs {
            wind_now_mph: 12.0,
            humidity_now_pct: 40.0,
            ..Default::default()
        };
        let z = zone("a", None);
        let c = ctx_for(&i, &z);
        let windy = ConditionExpr::Compare {
            metric: Metric::WindNowMph,
            op: CmpOp::Gt,
            value: 10.0,
        };
        let humid = ConditionExpr::Compare {
            metric: Metric::HumidityNowPct,
            op: CmpOp::Gt,
            value: 50.0,
        };
        assert!(eval_expr(
            &ConditionExpr::Any(vec![windy.clone(), humid.clone()]),
            &c
        ));
        assert!(!eval_expr(
            &ConditionExpr::All(vec![windy.clone(), humid.clone()]),
            &c
        ));
        assert!(eval_expr(&ConditionExpr::Not(Box::new(humid)), &c));
        // Empty All is vacuously true; empty Any is false.
        assert!(eval_expr(&ConditionExpr::All(vec![]), &c));
        assert!(!eval_expr(&ConditionExpr::Any(vec![]), &c));
    }

    #[test]
    fn zone_soil_none_is_false() {
        let i = Inputs::default();
        let z = zone("a", None);
        let c = ctx_for(&i, &z);
        // Any comparison on a missing soil reading must be false, in both
        // directions — missing data never causes a skip.
        for op in [CmpOp::Gt, CmpOp::Gte, CmpOp::Lt, CmpOp::Lte] {
            assert!(!eval_expr(
                &ConditionExpr::Compare {
                    metric: Metric::ZoneSoilPct,
                    op,
                    value: 50.0,
                },
                &c
            ));
        }
    }

    #[test]
    fn zone_soil_present_compares() {
        let i = Inputs::default();
        let z = zone("a", Some(75.0));
        let c = ctx_for(&i, &z);
        assert!(eval_expr(
            &ConditionExpr::Compare {
                metric: Metric::ZoneSoilPct,
                op: CmpOp::Gt,
                value: 70.0,
            },
            &c
        ));
    }

    #[test]
    fn skip_action_records_and_sets_reason() {
        let i = Inputs::default();
        let z = zone("a", Some(80.0));
        let c = ctx_for(&i, &z);
        let r = rule(
            "wet_skip",
            ConditionExpr::Compare {
                metric: Metric::ZoneSoilPct,
                op: CmpOp::Gte,
                value: 70.0,
            },
            RuleAction::Skip,
        );
        let out = apply_zone_rules(std::slice::from_ref(&r), &c);
        assert!(out.skip.is_some());
        assert_eq!(out.skip.unwrap().0, "wet_skip");
        assert_eq!(out.fired.iter().filter(|e| e.outcome == "fired").count(), 1);
    }

    #[test]
    fn multiplier_hard_clamped() {
        let i = Inputs {
            temp_now_f: 100.0,
            ..Default::default()
        };
        let z = zone("a", None);
        let c = ctx_for(&i, &z);
        let cond = ConditionExpr::Compare {
            metric: Metric::TempNowF,
            op: CmpOp::Gt,
            value: 50.0,
        };
        let hi = rule(
            "hi",
            cond.clone(),
            RuleAction::AdjustMultiplier { factor: 9.0 },
        );
        assert!((apply_zone_rules(std::slice::from_ref(&hi), &c).multiplier - 1.5).abs() < 1e-9);
        let lo = rule("lo", cond, RuleAction::AdjustMultiplier { factor: 0.01 });
        assert!((apply_zone_rules(std::slice::from_ref(&lo), &c).multiplier - 0.5).abs() < 1e-9);
    }

    #[test]
    fn disabled_and_scope_skip_evaluation() {
        let i = Inputs {
            temp_now_f: 100.0,
            ..Default::default()
        };
        let z = zone("front_yard", None);
        let c = ctx_for(&i, &z);
        let always = ConditionExpr::Compare {
            metric: Metric::TempNowF,
            op: CmpOp::Gt,
            value: 0.0,
        };
        // Disabled rule does nothing.
        let mut disabled = rule("d", always.clone(), RuleAction::Skip);
        disabled.enabled = false;
        assert!(apply_zone_rules(std::slice::from_ref(&disabled), &c)
            .skip
            .is_none());
        // Out-of-scope rule does nothing.
        let mut scoped = rule("s", always, RuleAction::Skip);
        scoped.scope = RuleScope::Zones(vec!["back_yard".into()]);
        let out = apply_zone_rules(std::slice::from_ref(&scoped), &c);
        assert!(out.skip.is_none());
        assert!(out.fired.is_empty(), "out-of-scope rule not walked");
    }

    #[test]
    fn roundtrips_through_json() {
        let r = rule(
            "x",
            ConditionExpr::All(vec![
                ConditionExpr::Compare {
                    metric: Metric::ZoneSoilPct,
                    op: CmpOp::Gte,
                    value: 65.0,
                },
                ConditionExpr::Not(Box::new(ConditionExpr::Compare {
                    metric: Metric::RainProbTomorrow,
                    op: CmpOp::Gt,
                    value: 60.0,
                })),
            ]),
            RuleAction::AdjustMultiplier { factor: 0.8 },
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: ConditionRule = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
