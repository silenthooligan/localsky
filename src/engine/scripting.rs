// User-defined skip rules via embedded Rhai. AUGMENT-ONLY: the engine
// evaluates these once even when a built-in gate already holds watering.
// A script can ADD a hold but can never clear a freeze / wind / restriction /
// rain-now gate. The independent result reaches every scheduled watering path,
// including schedules that explicitly waive a weather gate.
//
// An enabled script that cannot be evaluated holds watering with a named
// reason. Only a valid false or empty-string result means no added hold;
// an error cannot silently discard the owner's rule.
//
// Sandboxed: full stdlib minus `eval`, no module imports, bounded
// operation count + call depth + string size, so a pathological script
// can't hang or blow memory.

use rhai::{Dynamic, Engine, Scope, AST};

use crate::config::schema::ScriptRule;
use crate::engine::skip_rules::Inputs;

/// A compiled, ready-to-run set of user skip rules. `sync` feature makes
/// this Send+Sync so it can move into the refresher task.
pub struct CompiledScripts {
    engine: Engine,
    rules: Vec<CompiledRule>,
}

struct CompiledRule {
    id: String,
    name: String,
    ast: Option<AST>,
}

/// The shared, serializable script decision carried by snapshots.
pub use crate::model::ScriptHold as UserSkip;

impl Default for CompiledScripts {
    fn default() -> Self {
        Self {
            engine: sandboxed_engine(),
            rules: Vec::new(),
        }
    }
}

fn sandboxed_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(50_000);
    engine.set_max_call_levels(16);
    engine.set_max_expr_depths(64, 64);
    engine.set_max_string_size(4_000);
    engine.set_max_array_size(1_000);
    engine.set_max_map_size(1_000);
    engine.set_max_modules(0);
    // No dynamic re-eval of strings.
    engine.disable_symbol("eval");
    engine
}

impl CompiledScripts {
    /// Retain every enabled rule, including a failed compilation, so an
    /// invalid owner rule cannot disappear from the watering decision.
    pub fn compile(rules: &[ScriptRule]) -> Self {
        let engine = sandboxed_engine();
        let mut compiled = Vec::new();
        for r in rules.iter().filter(|r| r.enabled) {
            let ast = match engine.compile(&r.script) {
                Ok(ast) => Some(ast),
                Err(e) => {
                    tracing::warn!(rule = %r.id, error = %e, "skip-rule script failed to compile; watering held");
                    None
                }
            };
            compiled.push(CompiledRule {
                id: r.id.clone(),
                name: if r.name.is_empty() {
                    r.id.clone()
                } else {
                    r.name.clone()
                },
                ast,
            });
        }
        Self {
            engine,
            rules: compiled,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Run the rules against the current inputs. Returns the FIRST rule
    /// that asks to skip (true, or a non-empty reason string), or None.
    /// Evaluation errors and unsupported result types add a named hold.
    pub fn apply_user_skip(&self, i: &Inputs) -> Option<UserSkip> {
        for rule in &self.rules {
            let Some(ast) = &rule.ast else {
                return Some(rule.unavailable());
            };
            let mut scope = build_scope(i);
            match self.engine.eval_ast_with_scope::<Dynamic>(&mut scope, ast) {
                Ok(d) => {
                    if let Ok(b) = d.as_bool() {
                        if b {
                            return Some(UserSkip {
                                id: rule.id.clone(),
                                name: rule.name.clone(),
                                reason: rule.name.clone(),
                            });
                        }
                    } else if d.is_string() {
                        let s = d.into_string().unwrap_or_default();
                        if !s.trim().is_empty() {
                            return Some(UserSkip {
                                id: rule.id.clone(),
                                name: rule.name.clone(),
                                reason: s,
                            });
                        }
                    } else {
                        return Some(rule.unavailable());
                    }
                }
                Err(e) => {
                    tracing::warn!(rule = %rule.id, error = %e, "skip-rule script errored; watering held");
                    return Some(rule.unavailable());
                }
            }
        }
        None
    }
}

impl CompiledRule {
    fn unavailable(&self) -> UserSkip {
        UserSkip {
            id: self.id.clone(),
            name: self.name.clone(),
            reason: format!(
                "Watering held: rule '{}' could not be evaluated; fix or disable it",
                self.name
            ),
        }
    }
}

/// Expose known decision inputs as numbers. Missing forecast rain is Rhai unit
/// (`()`), so an unguarded numeric expression fails closed instead of seeing zero.
fn build_scope(i: &Inputs) -> Scope<'static> {
    let mut s = Scope::new();
    s.push("temp_now_f", i.temp_now_f);
    s.push("wind_now_mph", i.wind_now_mph);
    s.push("rain_today_in", i.rain_today_in);
    s.push_dynamic(
        "rain_intensity_now_in_hr",
        i.rain_intensity_now_in_hr
            .map(rhai::Dynamic::from)
            .unwrap_or(rhai::Dynamic::UNIT),
    );
    s.push("humidity_now_pct", i.humidity_now_pct);
    s.push_dynamic(
        "forecast_in",
        i.forecast_in
            .map(rhai::Dynamic::from)
            .unwrap_or(rhai::Dynamic::UNIT),
    );
    // Scripts see a plain number; an unreported probability scripts as 100
    // to match the engine's full-weight treatment (a "skip if prob > X"
    // rule then errs toward holding water, the same safe direction).
    s.push(
        "rain_tomorrow_prob_pct",
        i64::from(i.rain_tomorrow_prob_pct.unwrap_or(100)),
    );
    s.push_dynamic(
        "rain_next_4h_in",
        i.rain_next_4h_in
            .map(rhai::Dynamic::from)
            .unwrap_or(rhai::Dynamic::UNIT),
    );
    s.push("wind_max_today_mph", i.wind_max_today_mph);
    s.push_dynamic(
        "temp_min_24h_f",
        i.temp_min_24h_f
            .map(rhai::Dynamic::from)
            .unwrap_or(rhai::Dynamic::UNIT),
    );
    s.push("temp_max_3day_f", i.temp_max_3day_f);
    s.push(
        "days_since_significant_rain",
        i.days_since_significant_rain as i64,
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, script: &str) -> ScriptRule {
        ScriptRule {
            id: id.into(),
            name: format!("{id} rule"),
            enabled: true,
            script: script.into(),
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            wind_now_mph: 8.0,
            temp_now_f: 70.0,
            ..Default::default()
        }
    }

    #[test]
    fn bool_true_triggers_skip_with_name_reason() {
        let s = CompiledScripts::compile(&[rule("breezy", "wind_now_mph > 5.0")]);
        let got = s.apply_user_skip(&inputs()).expect("should skip");
        assert_eq!(got.id, "breezy");
        assert_eq!(got.reason, "breezy rule");
    }

    #[test]
    fn bool_false_does_not_skip() {
        let s = CompiledScripts::compile(&[rule("calm", "wind_now_mph > 50.0")]);
        assert!(s.apply_user_skip(&inputs()).is_none());
    }

    #[test]
    fn script_can_distinguish_missing_rain_from_real_zero() {
        let numeric = CompiledScripts::compile(&[rule("rain", "forecast_in + 0.0 > 0.2")]);
        let mut input = inputs();
        input.forecast_in = None;
        assert!(numeric
            .apply_user_skip(&input)
            .unwrap()
            .reason
            .contains("could not be evaluated"));
        let explicit = CompiledScripts::compile(&[rule("missing", "forecast_in == ()")]);
        assert!(explicit.apply_user_skip(&input).is_some());
        input.forecast_in = Some(0.0);
        assert!(numeric.apply_user_skip(&input).is_none());
        assert!(explicit.apply_user_skip(&input).is_none());
    }

    #[test]
    fn script_forecast_low_distinguishes_missing_from_real_freezing_zero() {
        let numeric = CompiledScripts::compile(&[rule("freeze", "temp_min_24h_f + 0.0 < 32.0")]);
        let explicit = CompiledScripts::compile(&[rule("missing", "temp_min_24h_f == ()")]);
        let mut input = inputs();
        input.temp_min_24h_f = None;
        assert!(numeric
            .apply_user_skip(&input)
            .unwrap()
            .reason
            .contains("could not be evaluated"));
        assert!(explicit.apply_user_skip(&input).is_some());
        input.temp_min_24h_f = Some(0.0);
        assert_eq!(
            numeric.apply_user_skip(&input).unwrap().reason,
            "freeze rule"
        );
        assert!(explicit.apply_user_skip(&input).is_none());
    }

    #[test]
    fn string_return_is_custom_reason() {
        let s = CompiledScripts::compile(&[rule(
            "custom",
            r#"if wind_now_mph > 5.0 { "too breezy for the misters" } else { "" }"#,
        )]);
        let got = s.apply_user_skip(&inputs()).expect("should skip");
        assert_eq!(got.reason, "too breezy for the misters");
    }

    #[test]
    fn empty_string_does_not_skip() {
        let s = CompiledScripts::compile(&[rule("noop", r#""""#)]);
        assert!(s.apply_user_skip(&inputs()).is_none());
    }

    #[test]
    fn invalid_syntax_retains_the_owner_rule_as_a_hold() {
        let s = CompiledScripts::compile(&[rule("bad", "this is not (valid rhai")]);
        assert!(!s.is_empty());
        let hold = s.apply_user_skip(&inputs()).unwrap();
        assert_eq!(hold.id, "bad");
        assert!(hold.reason.contains("fix or disable"));
    }

    #[test]
    fn runtime_error_holds_watering() {
        let s = CompiledScripts::compile(&[rule("oops", "no_such_fn(wind_now_mph)")]);
        assert_eq!(s.apply_user_skip(&inputs()).unwrap().id, "oops");
    }

    #[test]
    fn disabled_rule_is_not_compiled() {
        let mut r = rule("off", "wind_now_mph > 0.0");
        r.enabled = false;
        let s = CompiledScripts::compile(&[r]);
        assert!(s.is_empty());
    }

    #[test]
    fn first_firing_rule_wins() {
        let s = CompiledScripts::compile(&[
            rule("a", "wind_now_mph > 50.0"), // no
            rule("b", "temp_now_f > 60.0"),   // yes
            rule("c", "true"),                // also yes, but later
        ]);
        let got = s.apply_user_skip(&inputs()).unwrap();
        assert_eq!(got.id, "b");
    }

    #[test]
    fn operation_limit_caps_runaway_scripts() {
        // An infinite loop must hit the operation cap and error out
        // and hold watering, not hang the test or discard the rule.
        let s = CompiledScripts::compile(&[rule("loop", "let x = 0; while true { x += 1; } x")]);
        assert_eq!(s.apply_user_skip(&inputs()).unwrap().id, "loop");
    }

    #[test]
    fn unsupported_return_type_holds_watering() {
        let s = CompiledScripts::compile(&[rule("number", "42")]);
        assert_eq!(s.apply_user_skip(&inputs()).unwrap().id, "number");
    }
}
