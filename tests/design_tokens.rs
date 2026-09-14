// Contrast on the DESIGN.md surface tokens, measured. The text tokens on
// every surface they sit on, in the dark, light and high-contrast themes,
// must clear WCAG AA for normal text (4.5:1); the faint tier, which the
// stylesheet reserves for de-emphasized labels, must clear 3:1.

use std::collections::BTreeMap;

/// Every partial, concatenated in the order main.scss @use's them.
///
/// The stylesheet was split in C5, so main.scss is now a table of
/// contents and holds no rules. Reading the partials in @use order means
/// these checks see the same cascade the browser does; deriving the list
/// from main.scss rather than from a directory listing means a partial
/// that is added but never loaded is not silently measured, and one that
/// is loaded but missing is a panic rather than a pass.
fn scss() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("style");
    let toc = std::fs::read_to_string(dir.join("main.scss")).unwrap();
    let mut names: Vec<&str> = Vec::new();
    for line in toc.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("@use \"") else {
            continue;
        };
        let Some(name) = rest.split('"').next() else {
            continue;
        };
        names.push(name);
    }
    assert!(
        names.len() >= 10,
        "main.scss should be a @use list; found {} entries",
        names.len()
    );
    let mut out = String::new();
    for name in names {
        let path = dir.join(format!("_{name}.scss"));
        out.push_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("main.scss @use's {name}, which will not read: {e}")),
        );
        out.push('\n');
    }
    out
}

/// `--name: value;` lines of one block, from its opening line to the
/// matching close brace.
fn tokens_of(src: &str, opener: &str) -> BTreeMap<String, String> {
    let start = src.find(opener).unwrap_or_else(|| panic!("block {opener}"));
    let body = &src[start..];
    let open = body.find('{').unwrap();
    let mut depth = 0;
    let mut end = open;
    for (i, c) in body[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for line in body[open..end].lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("--") {
            if let Some((k, v)) = rest.split_once(':') {
                let v = v
                    .split("//")
                    .next()
                    .unwrap()
                    .trim()
                    .trim_end_matches(';')
                    .trim();
                out.insert(format!("--{}", k.trim()), v.to_string());
            }
        }
    }
    out
}

fn resolve(tokens: &[&BTreeMap<String, String>], name: &str, depth: u8) -> Option<[f64; 3]> {
    if depth > 8 {
        return None;
    }
    let raw = tokens.iter().find_map(|t| t.get(name))?;
    color(tokens, raw, depth)
}

fn color(tokens: &[&BTreeMap<String, String>], raw: &str, depth: u8) -> Option<[f64; 3]> {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix("var(").and_then(|r| r.strip_suffix(')')) {
        return resolve(tokens, inner.split(',').next().unwrap().trim(), depth + 1);
    }
    if let Some(hex) = raw.strip_prefix('#') {
        let hex = if hex.len() == 3 {
            hex.chars().flat_map(|c| [c, c]).collect::<String>()
        } else {
            hex.to_string()
        };
        if hex.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(&hex, 16).ok()?;
        return Some([
            ((v >> 16) & 0xff) as f64 / 255.0,
            ((v >> 8) & 0xff) as f64 / 255.0,
            (v & 0xff) as f64 / 255.0,
        ]);
    }
    None
}

fn luminance(c: [f64; 3]) -> f64 {
    let lin = |v: f64| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

const TEXT: &[(&str, f64)] = &[
    ("--text", 4.5),
    ("--text-soft", 4.5),
    ("--text-dim", 4.5),
    ("--accent-text", 4.5),
    ("--text-faint", 3.0),
];
const SURFACES: &[&str] = &[
    "--bg-deep",
    "--bg-panel",
    "--elev-0",
    "--elev-1",
    "--elev-2",
    // A field fill is a surface too: text sits on it, and so does the
    // field's own border.
    "--input-bg",
];

/// A field's border is what separates it from the surface holding it, so
/// it is a non-text contrast under WCAG 1.4.11 and its floor is 3:1. The
/// binding case is the resting border on the lightest surface a field can
/// sit on, which in dark measures 3.42:1.
const BORDERS: &[(&str, f64)] = &[
    ("--input-border", 3.0),
    ("--input-border-hover", 3.0),
    ("--input-border-focus", 3.0),
    ("--input-border-invalid", 3.0),
];

fn check(theme: &str, tokens: &[&BTreeMap<String, String>]) -> Vec<String> {
    let mut measured = 0;
    let mut failures = Vec::new();
    for (text, floor) in TEXT {
        let Some(fg) = resolve(tokens, text, 0) else {
            continue;
        };
        for surface in SURFACES {
            let Some(bg) = resolve(tokens, surface, 0) else {
                continue;
            };
            measured += 1;
            let ratio = contrast(fg, bg);
            if ratio < *floor {
                failures.push(format!(
                    "{theme}: {text} on {surface} = {ratio:.2}:1 (< {floor}:1)"
                ));
            }
        }
    }
    assert!(
        measured >= 8,
        "{theme}: only {measured} pairs resolved to a color"
    );
    // Same shape for the field borders, with its own counter. Without one
    // a renamed token would resolve to None, `continue` past every check,
    // and pass on having measured nothing at all.
    let mut border_pairs = 0;
    for (border, floor) in BORDERS {
        let Some(fg) = resolve(tokens, border, 0) else {
            continue;
        };
        for surface in SURFACES {
            let Some(bg) = resolve(tokens, surface, 0) else {
                continue;
            };
            border_pairs += 1;
            let ratio = contrast(fg, bg);
            if ratio < *floor {
                failures.push(format!(
                    "{theme}: {border} on {surface} = {ratio:.2}:1 (< {floor}:1)"
                ));
            }
        }
    }
    assert!(
        border_pairs >= 12,
        "{theme}: only {border_pairs} field-border pairs resolved to a color"
    );
    failures
}

/// Stacking is a named ladder, not a race of ad-hoc 9999s.
///
/// The exceptions are real and each one is here because the ladder cannot
/// express it: two elements that must sit at or below the base layer, one
/// negative decoration, and the radar's chip/drawer pair, which is a
/// deliberate one-step offset inside a single panel and is also written
/// from radar.js on its fallback path.
#[test]
fn stacking_goes_through_the_ladder() {
    const ALLOWED: &[&str] = &[
        "z-index: 0;",  // the aurora backdrop, and the zone-card hit target
        "z-index: -1;", // the panel's specular highlight
        "z-index: 2; // above .radar-map's z-index:1 stacking context",
        "z-index: 3; // over the chip while open",
    ];
    let src = scss();
    let mut raw = Vec::new();
    // Find the property wherever it sits, not only at the start of a line.
    // This file writes plenty of rules as one-liners, and two earlier cuts
    // of this test anchored at the line start and at a `;` boundary; both
    // sailed past `.x { a: b; z-index: 5; }`, which is exactly the shape
    // the chart tooltip was already written in.
    for (n, line) in src.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let code = t.split("//").next().unwrap_or(t);
        for (at, _) in code.match_indices("z-index:") {
            let tail = &code[at..];
            let value = tail.split(';').next().unwrap_or(tail).trim();
            let decl = format!("{value};");
            let commented = match t.split_once("//") {
                Some((_, c)) => format!("{decl} //{c}"),
                None => decl.clone(),
            };
            if value.contains("var(--z-")
                || ALLOWED.contains(&decl.as_str())
                || ALLOWED.contains(&commented.as_str())
            {
                continue;
            }
            raw.push(format!("style/ (concatenated line {}): {decl}", n + 1));
        }
    }
    assert!(
        raw.is_empty(),
        "raw z-index outside the allowlist; use the --z-* ladder:\n{}",
        raw.join("\n")
    );
    let used = src.matches("var(--z-").count();
    assert!(used >= 20, "the ladder is barely used ({used} references)");
}

#[test]
fn the_text_tokens_clear_wcag_aa_on_every_surface() {
    let src = scss();
    let brand = tokens_of(&src, ":root {");
    let dark = tokens_of(&src, ":root {");
    let light = tokens_of(&src, "@mixin localsky-light-tokens {");
    let hc = tokens_of(&src, "[data-theme=\"hc\"] {");
    let mut failures = Vec::new();
    failures.extend(check("dark", &[&dark, &brand]));
    failures.extend(check("light", &[&light, &brand]));
    failures.extend(check("high-contrast", &[&hc, &dark, &brand]));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
