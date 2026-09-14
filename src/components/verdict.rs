// Shared verdict presentation helpers, so the Rule Lab ladder, the Zones
// cards, and the zone detail all render a watering verdict identically
// (same color token + label).

/// CSS color token for a verdict string ("run" | "run_extended" | skip).
pub fn verdict_token(verdict: &str) -> &'static str {
    match verdict {
        "run" => "var(--verdict-run)",
        "run_extended" => "var(--verdict-extend)",
        _ => "var(--verdict-skip)",
    }
}

/// Short uppercase label for a verdict string.
pub fn verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "run" => "WATER",
        "run_extended" => "WATER +",
        _ => "SKIP",
    }
}

#[cfg(test)]
mod copy_guards {
    fn components_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/components")
    }

    /// One term for the run the engine is about to make: "next run". The
    /// yard may water before dawn or after sunrise, and "tonight" was
    /// wrong for half of them.
    #[test]
    fn no_component_says_tonight_or_nightly() {
        let banned = ["tonight", "nightly", "evening verdict"];
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            for (n, line) in crate::engine::clock::strings_only(&src).lines().enumerate() {
                let lower = line.to_lowercase();
                for b in banned {
                    if lower.contains(b) {
                        hits.push(format!("{}:{}: {b}", path.display(), n + 1));
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "components say something other than 'next run':\n{}",
            hits.join("\n")
        );
    }

    /// Every literal that follows `marker`, with line continuations
    /// folded the way the compiler folds them.
    fn literals_after(src: &str, marker: &str) -> Vec<String> {
        let b: Vec<char> = src.chars().collect();
        let m: Vec<char> = marker.chars().collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + m.len() < b.len() {
            if b[i..i + m.len()] != m[..] {
                i += 1;
                continue;
            }
            let mut k = i + m.len();
            let mut lit = String::new();
            while k < b.len() && b[k] != '"' {
                if b[k] == '\\' && k + 1 < b.len() {
                    if b[k + 1] == '\n' {
                        k += 2;
                        while k < b.len() && (b[k] == ' ' || b[k] == '\t') {
                            k += 1;
                        }
                        continue;
                    }
                    lit.push(b[k + 1]);
                    k += 2;
                    continue;
                }
                lit.push(b[k]);
                k += 1;
            }
            out.push(lit);
            i = k + 1;
        }
        out
    }

    /// Every help topic a panel names resolves to real help.
    ///
    /// A topic has two halves: the sentence in the popover and the page
    /// behind its link. The Engine page asked for "irrigation-engine",
    /// which the table did not carry, so everyone who opened that
    /// popover read "(no help topic configured)" instead of help.
    #[test]
    fn every_help_topic_has_a_body_and_a_page() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut missing = Vec::new();
        let mut topics_seen = 0usize;
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            // `help_topic="x"` on a Panel ends in the same marker.
            for topic in literals_after(code, "topic=\"") {
                if topic.is_empty() || topic.contains(' ') {
                    continue;
                }
                topics_seen += 1;
                if crate::components::ui::help_hint::help_topic(&topic)
                    == "(no help topic configured)"
                {
                    missing.push(format!("{}: no help body for {topic}", path.display()));
                }
                if !root.join(format!("docs/src/{topic}.md")).exists() {
                    missing.push(format!("{}: no docs page for {topic}", path.display()));
                }
            }
        }
        assert!(
            topics_seen > 15,
            "the walk found the topics ({topics_seen})"
        );
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    /// A save confirmation comes from `voice`, not from the page.
    ///
    /// Fourteen settings pages each had their own version of this
    /// sentence, naming whichever subsystem picked the change up: the
    /// engine's next tick, the registry hot-reloading, the advisor's
    /// next call. The operator is asking one thing, which is whether
    /// they have to do anything else.
    #[test]
    fn a_save_confirmation_comes_from_voice() {
        let allowed = [
            crate::voice::SAVED_LIVE,
            crate::voice::SAVED_NEEDS_RESTART,
            crate::voice::SAVED_ON_THIS_DEVICE,
            crate::voice::SAVED_TO_DRAFT,
        ];
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            for line in crate::engine::clock::strings_only(&src).lines() {
                let t = line.trim();
                if t.starts_with("Saved.") && !allowed.contains(&t) {
                    hits.push(format!("{}: {t}", path.display()));
                }
            }
        }
        assert!(
            hits.is_empty(),
            "a save says one of the sentences in voice.rs:\n{}",
            hits.join("\n")
        );
    }

    /// Helptext is a hint, not a paragraph.
    ///
    /// A ratchet: these are the numbers the tree has today, so the copy
    /// may only get shorter and a new string may not arrive longer than
    /// the worst one already here. The target was an average under
    /// eleven words; the settings rewrite reached 10.8, so the ratchet
    /// now holds that instead of aiming at it.
    #[test]
    fn helptext_stays_short() {
        /// Words in the longest helptext in the tree today.
        const LONGEST: usize = 28;
        /// Ten times the mean, so the check needs no floating point.
        /// Moved 108 -> 109 on 2026-09-09, and the reason is recorded
        /// rather than absorbed: the manual-schedule weather waiver added a
        /// field, and a new field longer than the current mean moves the
        /// mean. The alternative was trimming unrelated copy to make room,
        /// which games the guard instead of respecting it. LONGEST is
        /// untouched at 28, which is the half that protects the reader.
        const MEAN_X10: usize = 109;
        let mut counts = Vec::new();
        let mut worst = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            for lit in literals_after(code, "helptext=\"") {
                let n = lit.split_whitespace().count();
                counts.push(n);
                if n > LONGEST {
                    worst.push(format!(
                        "{}: {n} words: {}",
                        path.display(),
                        &lit[..60.min(lit.len())]
                    ));
                }
            }
        }
        assert!(counts.len() > 100, "the walk found the helptext");
        assert!(
            worst.is_empty(),
            "helptext got longer:\n{}",
            worst.join("\n")
        );
        let mean_x10 = counts.iter().sum::<usize>() * 10 / counts.len();
        assert!(
            mean_x10 <= MEAN_X10,
            "helptext got wordier on average: {mean_x10} vs {MEAN_X10} (tenths of a word)"
        );
    }

    /// A paragraph in front of an operator stays a paragraph.
    ///
    /// A ratchet with a ceiling per area, because the areas are being
    /// rewritten one at a time: an area that has been through the pass
    /// keeps its new worst case, and the ones still queued hold the
    /// worst they have today. The target everywhere is forty words, the
    /// point past which nobody standing at a valve is still reading.
    #[test]
    fn no_paragraph_runs_long() {
        // Longest single string a component may put in front of someone,
        // by the directory it lives in. Most specific prefix wins.
        const CEILINGS: &[(&str, usize)] = &[
            // The wizard, rewritten in B3, holds the tighter number it
            // earned: a step someone is reading for the first time.
            ("setup", 30),
            // Everywhere else, after B4. Forty is the point past which
            // nobody standing at a valve is still reading.
            ("", 40),
        ];
        let mut over = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let rel = path
                .strip_prefix(components_root())
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let ceiling = CEILINGS
                .iter()
                .find(|(dir, _)| rel.starts_with(dir))
                .map(|(_, n)| *n)
                .unwrap_or(usize::MAX);
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            for lit in literals_after(code, "\"") {
                if !reads_as_prose(&lit) {
                    continue;
                }
                let n = lit.split_whitespace().count();
                if n > ceiling {
                    over.push(format!(
                        "{rel}: {n} words (ceiling {ceiling}): {}",
                        lit.chars().take(50).collect::<String>()
                    ));
                }
            }
        }
        assert!(over.is_empty(), "{}", over.join("\n"));
    }

    /// Whether a string literal is something a person reads, as against
    /// a class name, a path, an inline style or a code fragment.
    fn reads_as_prose(lit: &str) -> bool {
        lit.split_whitespace().count() >= 5
            && !lit.starts_with('/')
            && !lit.starts_with("http")
            && !lit.starts_with("var(--")
            && !lit.contains("__")
            && !lit.contains("::")
            && !lit.chars().take(20).any(|c| c == ':')
    }

    /// LocalSky does not have a character called "the engine".
    ///
    /// The copy said it thirty-eight times: the engine decides, the
    /// engine skips, the engine picks up your binding, the engine
    /// thinks the lawn needed more. An operator does not care which
    /// part of the program did something, and naming an internal
    /// component makes the product sound like it is describing itself
    /// rather than their yard. Say what happens.
    ///
    /// What is left names the Engine settings PAGE, which is a place
    /// they can go rather than an actor doing things at them.
    #[test]
    fn the_engine_is_not_a_character() {
        const ALLOWED: usize = 4;
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap();
            for lit in literals_after(code, "\"") {
                if !reads_as_prose(&lit) {
                    continue;
                }
                if lit.to_lowercase().contains("the engine") {
                    hits.push(format!("{}: {}", path.display(), &lit[..60.min(lit.len())]));
                }
            }
        }
        assert!(
            hits.len() <= ALLOWED,
            "{} places say \"the engine\"; say what happens instead:
{}",
            hits.len(),
            hits.join(
                "
"
            )
        );
    }

    /// No component asks a question with a native browser dialog.
    ///
    /// `window.confirm()` is a modal the app does not own: it cannot be
    /// styled, cannot be themed, cannot say what the confirm button
    /// actually does (it says OK), and on a phone it arrives as a system
    /// alert bearing the origin rather than as part of LocalSky. Twelve
    /// of them were the last places the product changed character
    /// mid-sentence.
    ///
    /// `alert()` and `prompt()` are here for the same reason: nothing
    /// stops the next one arriving the same way.
    #[test]
    fn no_component_asks_with_a_browser_dialog() {
        const BANNED: &[&str] = &[
            "confirm_with_message",
            ".confirm()",
            "alert_with_message",
            "prompt_with_message",
        ];
        let mut found = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            // Skip test modules, or this guard's own banned list is the
            // first thing it finds.
            let code = src.split("#[cfg(test)]").next().unwrap();
            for (n, line) in code.lines().enumerate() {
                let t = line.trim();
                if t.starts_with("//") {
                    continue;
                }
                for bad in BANNED {
                    if t.contains(bad) {
                        found.push(format!("{}:{}: {t}", path.display(), n + 1));
                    }
                }
            }
        }
        assert!(
            found.is_empty(),
            "use ui::ConfirmSheet, not a browser dialog:
{}",
            found.join(
                "
"
            )
        );
    }

    /// Every primitive the kit exports is used by something.
    ///
    /// Six were not: Card, ListItem, weather_glyph, FeatureStub,
    /// DeltaSense, and Panel's `flush` (which emitted a class that had no
    /// CSS at all, so it did nothing even when set). An unused primitive
    /// is worse than a missing one. The next person needing a list row
    /// finds ListItem, uses it, and discovers it was never finished for
    /// the case they have.
    ///
    /// This reads the re-export list rather than the directory, because
    /// the export list is the kit's public surface: a module that exists
    /// but is not exported is an implementation detail, and a name that
    /// is exported is a promise.
    #[test]
    fn the_primitive_kit_has_no_unused_members() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mod_rs = root.join("src/components/ui/mod.rs");
        let src = std::fs::read_to_string(&mod_rs).unwrap();
        let mut names = Vec::new();
        for line in src.lines() {
            let Some(rest) = line.trim().strip_prefix("pub use ") else {
                continue;
            };
            let Some(tail) = rest.split("::").nth(1) else {
                continue;
            };
            let tail = tail.trim_end_matches(';');
            if let Some(inner) = tail.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
                names.extend(inner.split(',').map(|n| n.trim().to_string()));
            } else {
                names.push(tail.trim().to_string());
            }
        }
        assert!(names.len() > 15, "the walk found the kit ({})", names.len());

        // Where a primitive is allowed to be used only by other
        // primitives, and why.
        const KIT_ONLY: &[(&str, &str)] = &[
            (
                "Series",
                "the data type LineChart takes; a caller names it via the chart",
            ),
            ("ToastHub", "mounted once by the app shell, not by a page"),
            ("ToastViewport", "same: the shell mounts it"),
            (
                "ToastKind",
                "the toast API's own enum, reached through use_toast",
            ),
        ];

        let mut unused = Vec::new();
        for name in &names {
            if KIT_ONLY.iter().any(|(n, _)| n == name) {
                continue;
            }
            let mut seen = false;
            for path in crate::engine::clock::rust_sources(&root.join("src")) {
                if path.starts_with(root.join("src/components/ui")) {
                    continue;
                }
                let body = std::fs::read_to_string(&path).unwrap();
                let code = body.split("#[cfg(test)]").next().unwrap();
                // Three shapes: a component in a view, a function call,
                // and an enum reached by path (SheetVariant::Drawer).
                if code.contains(&format!("<{name}"))
                    || code.contains(&format!("{name}("))
                    || code.contains(&format!("{name}::"))
                {
                    seen = true;
                    break;
                }
            }
            if !seen {
                unused.push(name.clone());
            }
        }
        assert!(
            unused.is_empty(),
            "ui/mod.rs exports primitives nothing outside the kit uses;              delete them or use them: {unused:?}"
        );
    }

    /// Implementation words stay out of the operator's way.
    ///
    /// Deliberately a short list. Most terms that look like jargon are
    /// the right word for the person reading them: someone configuring
    /// MQTT knows what a payload is, and the About page names the
    /// libraries it credits. These are the ones that describe how
    /// LocalSky is built rather than what the operator is doing, and
    /// they never belong in front of anyone.
    #[test]
    fn no_implementation_words_in_the_operators_way() {
        let banned = [
            "hot-reload",
            "hot reload",
            "arc-swap",
            "mutex",
            "deserialize",
            "serialize",
            "idempotent",
            "nullable",
            "stdout",
            "stderr",
            "daemon",
        ];
        // Advanced is where an operator goes to see the machinery, and
        // About credits the libraries by name.
        let speaks_plainly = ["advanced.rs", "about.rs"];
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if speaks_plainly.contains(&name.as_str()) {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let prose = crate::engine::clock::strings_only(&src).to_lowercase();
            for b in banned {
                if prose.contains(b) {
                    hits.push(format!("{}: {b}", path.display()));
                }
            }
        }
        assert!(
            hits.is_empty(),
            "say what the operator sees, not what the code does:\n{}",
            hits.join("\n")
        );
    }

    /// A call to action lands on a page that can do the thing. The three
    /// legacy settings routes only redirect, and /settings/devices drops
    /// the section rail; the devices hub inside the settings shell is the
    /// place.
    #[test]
    fn no_link_points_at_a_redirect_route() {
        let redirects = [
            "\"/settings/sources\"",
            "\"/settings/data-sources\"",
            "\"/settings/controllers\"",
        ];
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&components_root()) {
            let src = std::fs::read_to_string(&path).unwrap();
            let prose = src.split("#[cfg(test)]").next().unwrap_or("");
            for (n, line) in prose.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let is_link = line.contains("href=") || line.contains("cta_href");
                if is_link && redirects.iter().any(|r| line.contains(r)) {
                    hits.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            hits.is_empty(),
            "links point at redirect-only routes:\n{}",
            hits.join("\n")
        );
    }
}
