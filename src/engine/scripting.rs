// User-defined skip rules via embedded Rhai. AUGMENT-ONLY: the engine
// consults these only when the built-in deterministic ladder already
// returned "run", so a script can ADD a skip but can never clear a
// freeze / wind / restriction / rain-now gate. That boundary is enforced
// by the caller (it doesn't call this on a "skip" verdict); this module
// just evaluates rules and reports the first one that asks to skip.
//
// Fail-safe by construction: a script that errors, times out, has invalid
// syntax, or returns anything other than `true` / a non-empty string is a
// no-op (no skip). The worst a buggy script can do is withhold watering
// (recoverable), never water during a freeze (unrecoverable).
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
    ast: AST,
}

/// The outcome when a user rule asks to skip.
#[derive(Debug, Clone, PartialEq)]
pub struct UserSkip {
    pub id: String,
    pub name: String,
    pub reason: String,
}

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
    /// Compile every enabled rule. Rules that fail to compile are logged
    /// and dropped (fail-safe); valid rules are kept.
    pub fn compile(rules: &[ScriptRule]) -> Self {
        let engine = sandboxed_engine();
        let mut compiled = Vec::new();
        for r in rules.iter().filter(|r| r.enabled) {
            match engine.compile(&r.script) {
                Ok(ast) => compiled.push(CompiledRule {
                    id: r.id.clone(),
                    name: if r.name.is_empty() {
                        r.id.clone()
                    } else {
                        r.name.clone()
                    },
                    ast,
                }),
                Err(e) => {
                    tracing::warn!(rule = %r.id, error = %e, "skip-rule script failed to compile; ignoring");
                }
            }
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
    /// Errors are logged and treated as no-skip.
    pub fn apply_user_skip(&self, i: &Inputs) -> Option<UserSkip> {
        for rule in &self.rules {
            let mut scope = build_scope(i);
            match self
                .engine
                .eval_ast_with_scope::<Dynamic>(&mut scope, &rule.ast)
            {
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
                    }
                }
                Err(e) => {
                    tracing::warn!(rule = %rule.id, error = %e, "skip-rule script errored; ignoring");
                }
            }
        }
        None
    }
}

/// Expose the decision inputs to the script as plain float/int variables.
fn build_scope(i: &Inputs) -> Scope<'static> {
    let mut s = Scope::new();
    s.push("temp_now_f", i.temp_now_f);
    s.push("wind_now_mph", i.wind_now_mph);
    s.push("rain_today_in", i.rain_today_in);
    s.push("rain_intensity_now_in_hr", i.rain_intensity_now_in_hr);
    s.push("humidity_now_pct", i.humidity_now_pct);
    s.push("forecast_in", i.forecast_in);
    s.push("rain_tomorrow_prob_pct", i.rain_tomorrow_prob_pct as i64);
    s.push("rain_next_4h_in", i.rain_next_4h_in);
    s.push("wind_max_today_mph", i.wind_max_today_mph);
    s.push("temp_min_24h_f", i.temp_min_24h_f);
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
    fn invalid_syntax_is_dropped_at_compile() {
        // Garbage compiles to nothing; no rules, no skip, no panic.
        let s = CompiledScripts::compile(&[rule("bad", "this is not (valid rhai")]);
        assert!(s.is_empty());
        assert!(s.apply_user_skip(&inputs()).is_none());
    }

    #[test]
    fn runtime_error_is_ignored() {
        // Calls an unknown function -> eval error -> treated as no-skip.
        let s = CompiledScripts::compile(&[rule("oops", "no_such_fn(wind_now_mph)")]);
        assert!(s.apply_user_skip(&inputs()).is_none());
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
        // (ignored), not hang the test.
        let s = CompiledScripts::compile(&[rule("loop", "let x = 0; while true { x += 1; } x")]);
        assert!(s.apply_user_skip(&inputs()).is_none());
    }
}
