// The v1 deprecation record, enforced.
//
// Every field api.md lists as deprecated carries a DEPRECATED note on
// its struct field, and the number of places in src/ that read each one
// is pinned: a new reader of a deprecated field fails this test, which
// is the plan's rule for this release ("no new consumer of a deprecated
// field is added by any commit"). Removing a reader lowers the pin.

use std::path::Path;

const DEPRECATED: &[(&str, usize)] = &[
    // (field, readers in src/ outside the declaring file, as of 0.9.0)
    ("hex", 0),
    ("iu_enabled", 2),
    ("iu_suspended", 1),
    ("ha_reachable", 10),
    ("override_helpers_present", 3),
    ("ha_adoption_awaiting_config", 0),
    ("mode_active", 0),
    ("today_run_minutes", 0),
];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Lines that read `.field` (a member access), comments stripped.
fn readers(field: &str) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    let needle = format!(".{field}");
    let mut hits = Vec::new();
    for f in files {
        if f.ends_with("snapshot.rs") && f.parent().is_some_and(|p| p.ends_with("model")) {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        for (i, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap();
            let mut rest = code;
            while let Some(k) = rest.find(&needle) {
                let after = &rest[k + needle.len()..];
                let boundary = after
                    .chars()
                    .next()
                    .map(|c| !(c.is_alphanumeric() || c == '_' || c == '('))
                    .unwrap_or(true);
                if boundary {
                    hits.push(format!(
                        "{}:{}",
                        f.strip_prefix(root()).unwrap().display(),
                        i + 1
                    ));
                    break;
                }
                rest = after;
            }
        }
    }
    hits
}

#[test]
fn every_deprecated_field_is_marked_in_the_source() {
    let snap = std::fs::read_to_string(root().join("src/model/snapshot.rs")).unwrap();
    for (field, _) in DEPRECATED {
        let decl = format!("pub {field}:");
        let i = snap
            .find(&decl)
            .unwrap_or_else(|| panic!("{field} declared"));
        let above = &snap[..i];
        let doc_start = above.rfind("\n\n").unwrap_or(0);
        assert!(
            above[doc_start..].contains("DEPRECATED (0.9.0)"),
            "{field} carries no DEPRECATED note above its declaration"
        );
    }
}

#[test]
fn every_deprecated_field_is_in_the_api_docs_table() {
    let api = std::fs::read_to_string(root().join("docs/src/api.md")).unwrap();
    let table = api
        .split("### Deprecated on v1")
        .nth(1)
        .expect("the table exists");
    let table = table.split("### Migration notes").next().unwrap();
    for (field, _) in DEPRECATED {
        assert!(
            table.contains(&format!("`{field}`")) || table.contains(&format!("[].{field}`")),
            "{field} missing from the table"
        );
    }
    assert!(
        table.contains("| future removal |"),
        "the table names the separately deferred removal"
    );
}

#[test]
fn no_new_reader_of_a_deprecated_field() {
    for (field, pinned) in DEPRECATED {
        let hits = readers(field);
        assert!(
            hits.len() <= *pinned,
            "{field}: {} readers, pinned at {pinned}; a deprecated field gained a consumer:\n{}",
            hits.len(),
            hits.join("\n")
        );
        assert_eq!(
            hits.len(),
            *pinned,
            "{field}: readers dropped to {}; lower the pin",
            hits.len()
        );
    }
}
