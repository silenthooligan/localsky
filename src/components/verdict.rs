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
