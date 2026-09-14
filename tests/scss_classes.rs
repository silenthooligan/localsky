// Stylesheet liveness: every rule in style/*.scss must be reachable from
// markup the app emits. A rule is dead when each of its selectors names a
// class that no Rust string literal (class lists, `class:` syntax) and no
// public/*.js literal ever produces. Dynamic families built with format!
// are covered by DYNAMIC_PREFIXES; keep that list honest, the second test
// checks each prefix is still built somewhere.
//
// Why a test and not a lint: the 0.9.0 review found about 2,000 lines of
// SCSS styling markup nothing rendered, accumulated one refactor at a time.
// Failing the build on the first orphaned rule keeps that from recurring.

use std::path::{Path, PathBuf};

/// Class prefixes that Rust or JS builds at runtime (format!("wind-bar-{}")).
/// A class starting with one of these counts as referenced.
const DYNAMIC_PREFIXES: &[&str] = &[
    "btn--",
    "cloud-hero__rollup--",
    "cloud-hero__trust--",
    "cloud-row__keytier--",
    "cloud-word--",
    "daily-card-rainchar--",
    "entity-badge--",
    "entity-stripe--",
    "ha-card__icon--",
    "ha-chip--",
    "kind-",
    "trend-",
    "ui-skel--",
    "wind-bar-",
    "wk-legend__item--",
    "wk-row--",
    "zone-card--",
    // Leaflet's own DOM (vendored under public/vendor/leaflet).
    "leaflet-",
];

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

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------- SCSS

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Root,
    Sel,
    At,
    Keyframes,
    Frame,
    Mixin,
}

#[derive(Debug)]
struct Block {
    kind: Kind,
    prelude: String,
    line: usize,
    has_decls: bool,
    extends: Vec<String>,
    full: Vec<String>,
    parent: usize,
    children: Vec<usize>,
}

fn strip_scss_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut q: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(qq) = q {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == qq {
                q = None;
            }
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            q = Some(c);
            out.push(c as char);
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            while i < b.len() && !b[i..].starts_with(b"*/") {
                out.push(if b[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            out.push_str("  ");
            i = (i + 2).min(b.len());
            continue;
        }
        // keep multi-byte characters intact
        let ch_len = utf8_len(c);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

fn split_list(sel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in sel.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if ch == ',' && depth == 0 {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn parse(text: &str) -> Vec<Block> {
    let clean = strip_scss_comments(text);
    let mut blocks = vec![Block {
        kind: Kind::Root,
        prelude: String::new(),
        line: 0,
        has_decls: false,
        extends: Vec::new(),
        full: vec![String::new()],
        parent: 0,
        children: Vec::new(),
    }];
    let mut cur = 0usize;
    let mut buf = String::new();
    let mut line = 1usize;
    let mut buf_line = 1usize;
    let mut paren = 0i32;
    let mut q: Option<char> = None;
    let mut chars = clean.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            line += 1;
        }
        if let Some(qq) = q {
            buf.push(ch);
            if ch == '\\' {
                if let Some(n) = chars.next() {
                    buf.push(n);
                }
            } else if ch == qq {
                q = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                q = Some(ch);
                buf.push(ch);
            }
            '(' => {
                paren += 1;
                buf.push(ch);
            }
            ')' => {
                paren -= 1;
                buf.push(ch);
            }
            '{' if paren == 0 => {
                let prelude = buf.trim().to_string();
                let kind = if prelude.starts_with("@keyframes")
                    || prelude.starts_with("@-webkit-keyframes")
                {
                    Kind::Keyframes
                } else if prelude.starts_with("@mixin") || prelude.starts_with("@function") {
                    Kind::Mixin
                } else if blocks[cur].kind == Kind::Keyframes {
                    Kind::Frame
                } else if prelude.starts_with('@') {
                    Kind::At
                } else {
                    Kind::Sel
                };
                let idx = blocks.len();
                blocks.push(Block {
                    kind,
                    prelude,
                    line: buf_line,
                    has_decls: false,
                    extends: Vec::new(),
                    full: Vec::new(),
                    parent: cur,
                    children: Vec::new(),
                });
                blocks[cur].children.push(idx);
                cur = idx;
                buf.clear();
                buf_line = line;
            }
            '}' if paren == 0 => {
                if !buf.trim().is_empty() {
                    note_decl(&mut blocks[cur], buf.trim());
                }
                buf.clear();
                cur = blocks[cur].parent;
                buf_line = line;
            }
            ';' if paren == 0 => {
                if !buf.trim().is_empty() {
                    note_decl(&mut blocks[cur], buf.trim());
                }
                buf.clear();
                buf_line = line;
            }
            _ => {
                if buf.trim().is_empty() && !ch.is_whitespace() {
                    buf_line = line;
                }
                buf.push(ch);
            }
        }
    }
    // resolve full selectors, parents before children (arena order)
    for i in 1..blocks.len() {
        let p = blocks[i].parent;
        let pf: Vec<String> = if blocks[p].full.is_empty() {
            vec![String::new()]
        } else {
            blocks[p].full.clone()
        };
        let full = match blocks[i].kind {
            Kind::Sel => {
                let own = split_list(&blocks[i].prelude);
                let mut res = Vec::new();
                for ps in &pf {
                    for o in &own {
                        if o.contains('&') {
                            res.push(if ps.is_empty() {
                                o.replace('&', "").trim().to_string()
                            } else {
                                o.replace('&', ps)
                            });
                        } else {
                            res.push(format!("{ps} {o}").trim().to_string());
                        }
                    }
                }
                res
            }
            Kind::Mixin => vec![String::new()],
            _ => pf,
        };
        blocks[i].full = full;
    }
    blocks
}

fn note_decl(b: &mut Block, d: &str) {
    if let Some(rest) = d.strip_prefix("@extend") {
        b.extends.push(rest.trim().to_string());
    }
    b.has_decls = true;
}

/// Drop `:not(...)`, `:is(...)`, `:where(...)`, `:has(...)` arguments: a class
/// inside them does not need to exist for the rule to match.
fn strip_pseudo_fns(sel: &str) -> String {
    let mut s = sel.to_string();
    loop {
        let Some(pos) = ["not(", "is(", "where(", "has("]
            .iter()
            .filter_map(|p| s.find(&format!(":{p}")))
            .min()
        else {
            return s;
        };
        let open = s[pos..].find('(').unwrap() + pos;
        let mut depth = 0i32;
        let mut end = open;
        for (k, ch) in s[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + k + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end == open {
            return s;
        }
        s.replace_range(pos..end, "");
    }
}

fn classes_in(sel: &str) -> Vec<String> {
    let s = strip_pseudo_fns(sel);
    let mut out = Vec::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '.'
            && i + 1 < b.len()
            && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == '_' || b[i + 1] == '-')
        {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_' || b[j] == '-') {
                j += 1;
            }
            out.push(b[i + 1..j].iter().collect());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- corpus

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>, ext: &str) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out, ext);
        } else if p.extension().and_then(|s| s.to_str()) == Some(ext) {
            out.push(p);
        }
    }
}

/// Strip // and /* */ comments. `js`: every quote style opens a string;
/// Rust: a `'` opens one only when it closes within a few bytes (char).
fn strip_code_comments(t: &str, js: bool) -> String {
    let b = t.as_bytes();
    let mut out = String::with_capacity(t.len());
    let mut i = 0;
    let mut q: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(qq) = q {
            let l = utf8_len(c);
            out.push_str(&t[i..i + l]);
            if c == b'\\' && i + 1 < b.len() {
                let l2 = utf8_len(b[i + 1]);
                out.push_str(&t[i + 1..i + 1 + l2]);
                i += 1 + l2;
                continue;
            }
            if c == qq {
                q = None;
            }
            i += l;
            continue;
        }
        if c == b'"' || c == b'`' && js || c == b'\'' {
            let opens = if c == b'\'' && !js {
                // char literal or lifetime: only a close within 4 bytes counts
                b[i + 1..].iter().take(4).any(|&x| x == b'\'')
            } else {
                true
            };
            if opens {
                q = Some(c);
            }
            out.push(c as char);
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            while i < b.len() && !b[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        let l = utf8_len(c);
        out.push_str(&t[i..i + l]);
        i += l;
    }
    out
}

/// Every string literal body in Rust source (plain and raw), with char
/// literals neutralized so an apostrophe never opens a string.
fn rust_string_literals(t: &str) -> Vec<String> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // char literal: 'x' or '\n' or '\u{..}'
        if c == b'\'' {
            if let Some(close) = b[i + 1..].iter().take(12).position(|&x| x == b'\'') {
                let inner = &b[i + 1..i + 1 + close];
                if !inner.is_empty() && (inner[0] == b'\\' || close <= 4) {
                    i += close + 2;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // raw string r"..." / r#"..."#
        if c == b'r' && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                let close = format!("\"{}", "#".repeat(hashes));
                if let Some(k) = t[j + 1..].find(&close) {
                    out.push(t[j + 1..j + 1 + k].to_string());
                    i = j + 1 + k + close.len();
                    continue;
                }
            }
        }
        if c == b'"' {
            let mut j = i + 1;
            let start = j;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' {
                    break;
                }
                j += 1;
            }
            let j = j.min(b.len());
            out.push(t[start..j].to_string());
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

fn js_string_literals(t: &str) -> Vec<String> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let mut j = i + 1;
            let start = j;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == c {
                    break;
                }
                j += 1;
            }
            let j = j.min(b.len());
            out.push(t[start..j].to_string());
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

/// Remove `{...}` (format! placeholders) and `${...}` (JS templates).
fn without_placeholders(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in s.chars() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                out.push(' ');
            }
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.replace('$', " ")
}

struct Corpus {
    tokens: std::collections::HashSet<String>,
}

impl Corpus {
    fn load() -> Self {
        let mut tokens = std::collections::HashSet::new();
        let mut files = Vec::new();
        rust_files(&root().join("src"), &mut files, "rs");
        for f in &files {
            let t = strip_code_comments(&std::fs::read_to_string(f).unwrap(), false);
            for lit in rust_string_literals(&t) {
                let body = without_placeholders(&lit);
                if body.chars().all(|c| is_word_char(c) || c.is_whitespace()) {
                    tokens.extend(words(&body));
                }
            }
            // class:is-active=... syntax
            for (k, _) in t.match_indices("class:") {
                let rest = &t[k + 6..];
                let w: String = rest.chars().take_while(|c| is_word_char(*c)).collect();
                if !w.is_empty() {
                    tokens.insert(w);
                }
            }
        }
        let mut js = Vec::new();
        rust_files(&root().join("public"), &mut js, "js");
        for f in &js {
            let t = strip_code_comments(&std::fs::read_to_string(f).unwrap(), true);
            for lit in js_string_literals(&t) {
                tokens.extend(words(&without_placeholders(&lit)));
            }
        }
        Corpus { tokens }
    }

    fn referenced(&self, class: &str, extend_targets: &std::collections::HashSet<String>) -> bool {
        self.tokens.contains(class)
            || extend_targets.contains(class)
            || DYNAMIC_PREFIXES
                .iter()
                .any(|p| class.starts_with(p) && class.len() > p.len())
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_scss_rule_styles_markup_something_emits() {
    let scss = scss();
    let blocks = parse(&scss);
    let corpus = Corpus::load();
    let mut extend_targets = std::collections::HashSet::new();
    for b in &blocks {
        for e in &b.extends {
            extend_targets.extend(classes_in(e));
        }
    }
    let mut dead = Vec::new();
    for b in &blocks {
        if b.kind != Kind::Sel || !b.has_decls {
            continue;
        }
        let mut all_dead = true;
        let mut missing = Vec::new();
        for sel in &b.full {
            let cs = classes_in(sel);
            let unref: Vec<&String> = cs
                .iter()
                .filter(|c| !corpus.referenced(c, &extend_targets))
                .collect();
            if cs.is_empty() || unref.is_empty() {
                all_dead = false;
                break;
            }
            missing.extend(unref.into_iter().cloned());
        }
        if all_dead {
            missing.sort();
            missing.dedup();
            dead.push(format!(
                "style/ (concatenated line {})  {}  (no markup emits: {})",
                b.line,
                b.prelude.replace('\n', " "),
                missing.join(", ")
            ));
        }
    }
    assert!(
        dead.is_empty(),
        "{} SCSS rule(s) style classes nothing renders. Delete the rule, or add the \
         class to DYNAMIC_PREFIXES if it is built at runtime:\n{}",
        dead.len(),
        dead.join("\n")
    );
}

#[test]
fn every_dynamic_prefix_is_still_built_somewhere() {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files, "rs");
    rust_files(&root().join("public"), &mut files, "js");
    let text: String = files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let stale: Vec<&str> = DYNAMIC_PREFIXES
        .iter()
        .copied()
        .filter(|p| *p != "leaflet-")
        .filter(|p| !text.contains(&format!("{p}{{")) && !text.contains(&format!("{p}\" ")))
        .collect();
    assert!(
        stale.is_empty(),
        "DYNAMIC_PREFIXES entries nothing builds any more: {stale:?}"
    );
}

#[test]
fn the_parser_resolves_nesting_the_way_sass_does() {
    let blocks = parse(
        ".card, .tile {\n  color: red;\n  &__title { x: 1; }\n  .kid & { y: 2; }\n  @media (max-width: 1px) { z: 3; }\n}\n",
    );
    let full: Vec<&Vec<String>> = blocks.iter().skip(1).map(|b| &b.full).collect();
    assert_eq!(full[0], &vec![".card".to_string(), ".tile".to_string()]);
    assert_eq!(
        full[1],
        &vec![".card__title".to_string(), ".tile__title".to_string()]
    );
    assert_eq!(
        full[2],
        &vec![".kid .card".to_string(), ".kid .tile".to_string()]
    );
    assert_eq!(full[3], &vec![".card".to_string(), ".tile".to_string()]);
    assert_eq!(classes_in(".a:not(.b) .c:is(.d)"), vec!["a", "c"]);
}
