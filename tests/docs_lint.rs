// The docs lint: the numbers the documentation states are the code's
// numbers, and nothing shipped is still called "Planned".
//
// docs/src uses tokens the publish step substitutes ({{LOCALSKY_VERSION}},
// {{LOCALSKY_API_VERSION}}, {{LOCALSKY_DB_MIGRATIONS}},
// {{LOCALSKY_SKIP_RULES}}); README.public.md is read on GitHub as-is, so
// it carries literals, and this lint fails when they drift.

use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn docs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root().join("docs/src"))
        .unwrap()
        .flatten()
    {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "md") {
            out.push((
                p.display().to_string(),
                std::fs::read_to_string(&p).unwrap(),
            ));
        }
    }
    out
}

fn skip_rule_count() -> usize {
    localsky::gates_catalog::builtin_rule_catalog().len()
}

fn migration_count() -> usize {
    std::fs::read_dir(root().join("src/persistence/migrations"))
        .unwrap()
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with('M') && n.ends_with(".sql")
        })
        .count()
}

/// The shell one-liners the Dockerfile and the publish workflow use to
/// substitute the tokens must count the same things this crate does.
#[test]
fn the_substitution_recipes_agree_with_the_code() {
    let catalog = std::fs::read_to_string(root().join("src/gates_catalog.rs")).unwrap();
    let grep_c = catalog.lines().filter(|l| *l == "        (").count();
    assert_eq!(
        grep_c,
        skip_rule_count(),
        "grep -c '^        ($' src/gates_catalog.rs"
    );
    assert!(migration_count() >= 19);
    let info = std::fs::read_to_string(root().join("src/api/info.rs")).unwrap();
    let line = info
        .lines()
        .find(|l| l.contains("pub const API_VERSION"))
        .unwrap();
    let cut = line.split('"').nth(1).unwrap();
    assert_eq!(cut, localsky::api::info::API_VERSION);
}

/// Every token in docs/src is one the publish step substitutes, and the
/// numbers the docs state in prose are tokens, not literals that drift.
#[test]
fn docs_use_tokens_for_the_numbers_that_drift() {
    let known = [
        "{{LOCALSKY_VERSION}}",
        "{{LOCALSKY_API_VERSION}}",
        "{{LOCALSKY_DB_MIGRATIONS}}",
        "{{LOCALSKY_SKIP_RULES}}",
    ];
    for (path, text) in docs() {
        for (i, line) in text.lines().enumerate() {
            let mut rest = line;
            while let Some(k) = rest.find("{{LOCALSKY_") {
                let tail = &rest[k..];
                let end = tail.find("}}").map(|e| e + 2).unwrap_or(tail.len());
                let tok = &tail[..end];
                assert!(
                    known.contains(&tok),
                    "{path}:{}: unknown token {tok}",
                    i + 1
                );
                rest = &tail[end..];
            }
            assert!(
                !line.contains("\"api_version\": \"1."),
                "{path}:{}: a literal api_version; use {{{{LOCALSKY_API_VERSION}}}}",
                i + 1
            );
            assert!(
                !line.contains("-rule skip ladder") || line.contains("{{LOCALSKY_SKIP_RULES}}"),
                "{path}:{}: a literal rule count; use {{{{LOCALSKY_SKIP_RULES}}}}",
                i + 1
            );
        }
    }
}

fn public_readme() -> String {
    std::fs::read_to_string(root().join("README.public.md"))
        .or_else(|_| std::fs::read_to_string(root().join("README.md")))
        .expect("public README must be present in the canonical or sanitized tree")
}

/// The guide owns the rule inventory. Any count mentioned in the public
/// README must still match the code, but the product overview need not list it.
#[test]
fn the_rule_reference_uses_the_catalog_and_readme_counts_cannot_drift() {
    let readme = public_readme();
    let reference = std::fs::read_to_string(root().join("docs/src/skip-rules.md")).unwrap();
    assert!(
        reference.contains("{{LOCALSKY_SKIP_RULES}}"),
        "the rule reference derives its count from the catalog"
    );
    for (i, line) in readme.lines().enumerate() {
        // Every "N-rule" in the README is the catalog's N.
        let mut rest = line;
        while let Some(k) = rest.find("-rule") {
            let digits: String = rest[..k]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if !digits.is_empty() {
                assert_eq!(
                    digits.parse::<usize>().unwrap(),
                    skip_rule_count(),
                    "README.public.md:{}: stale rule count: {line}",
                    i + 1
                );
            }
            rest = &rest[k + 5..];
        }
    }
}

/// A "Planned" row or sentence must not name something the product
/// registers: a controller kind on a line about controllers, a
/// notification sink on a line about notifications. (An ESPHome sensor
/// adapter can be planned while the ESPHome controller ships.)
#[test]
fn nothing_registered_is_still_planned() {
    let controllers: &[&str] = &[
        "rachio",
        "hydrawise",
        "b-hyve",
        "bhyve",
        "rain bird",
        "rainbird",
        "opensprinkler",
        "esphome",
        "mqtt",
    ];
    let sinks: &[&str] = &["ntfy", "slack", "web push"];
    let mut files = docs();
    files.push(("README.public.md".into(), public_readme()));
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            if !(line.contains("Planned") || line.contains("(planned)")) {
                continue;
            }
            let lower = line.to_lowercase();
            if lower.contains("controller") {
                for r in controllers {
                    assert!(
                        !lower.contains(r),
                        "{path}:{}: the {r} controller is registered but the line says Planned: {line}",
                        i + 1
                    );
                }
            }
            if lower.contains("push") || lower.contains("sink") || lower.contains("notification") {
                for r in sinks {
                    assert!(
                        !lower.contains(r),
                        "{path}:{}: the {r} sink is registered but the line says Planned: {line}",
                        i + 1
                    );
                }
            }
        }
    }
}

/// The pages load no script or stylesheet from another origin: a LAN-only
/// install renders the radar page with nothing fetched from a CDN.
#[test]
fn no_page_loads_a_script_or_stylesheet_from_another_origin() {
    let app = std::fs::read_to_string(root().join("src/app.rs")).unwrap();
    for (i, line) in app.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        let external = (line.contains("<script") || line.contains("stylesheet"))
            && (line.contains("https://") || line.contains("http://"));
        assert!(
            !external,
            "src/app.rs:{}: loads from another origin: {line}",
            i + 1
        );
    }
    for f in ["leaflet.js", "leaflet.css", "images/marker-icon.png"] {
        assert!(
            root().join("public/vendor/leaflet").join(f).exists(),
            "vendored {f}"
        );
    }
}
