// Single source of truth for the irrigation zone list.
//
// The runtime learns the zones at boot from the config file
// (config.zones) when /data/localsky.toml exists. The wizard writes it on
// first run; `from_pairs` normalizes the keys (hyphens -> underscores) so
// the list matches the snapshot and scheduler slugs. A fresh unconfigured
// install resolves zero zones and the UI shows empty states until then.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneIdent {
    pub slug: String,
    pub display_name: String,
}

impl ZoneIdent {
    pub fn new(slug: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            display_name: display_name.into(),
        }
    }
}

/// Build the zone list from config (slug, display_name) pairs. Slugs are
/// underscore-normalized so the list matches the snapshot + schedulers
/// (config keys may be hyphenated, e.g. "back-yard"). An empty display
/// name falls back to a humanized slug.
pub fn from_pairs<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<ZoneIdent> {
    pairs
        .map(|(slug, name)| {
            let slug = slug.replace('-', "_");
            let display = if name.trim().is_empty() {
                humanize(&slug)
            } else {
                name.trim().to_string()
            };
            ZoneIdent::new(slug, display)
        })
        .collect()
}

/// Derive a human-friendly display name from a slug by replacing
/// underscores with spaces and title-casing each word. Used as a
/// fallback when the env var format omits the display name.
fn humanize(slug: &str) -> String {
    slug.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let rest: String = chars.collect();
                    format!("{}{}", first.to_uppercase(), rest)
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_handles_underscores() {
        assert_eq!(humanize("back_yard"), "Back Yard");
        assert_eq!(humanize("back_yard_shrubs"), "Back Yard Shrubs");
        assert_eq!(humanize("zone1"), "Zone1");
    }
}
