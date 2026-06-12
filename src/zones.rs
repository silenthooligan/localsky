// Single source of truth for the irrigation zone list.
//
// The runtime learns the zones at boot from one of three sources, in
// priority order (resolved in main.rs, passed into spawn_refresher):
//
//   1. The config file (config.zones) when /data/localsky.toml exists.
//      The wizard writes this on first-run; `from_pairs` normalizes the
//      keys (hyphens -> underscores) so the list matches the snapshot
//      and scheduler slugs.
//   2. The LOCALSKY_ZONES env var. CSV of either bare slugs
//      ("back_yard,front_yard") or "slug:Display Name" pairs
//      ("back_yard:Back Yard,front_yard:Front Yard"). Lets operators
//      override without a recompile.
//   3. Nothing. A fresh unconfigured install resolves zero zones; the
//      UI shows empty states until the wizard writes the config.

use std::env;

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

fn parse_env(raw: &str) -> Vec<ZoneIdent> {
    raw.split(',')
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            if let Some((slug, name)) = entry.split_once(':') {
                ZoneIdent::new(slug.trim(), name.trim())
            } else {
                ZoneIdent::new(entry, humanize(entry))
            }
        })
        .collect()
}

/// Resolve the active zone list from the environment. Reads LOCALSKY_ZONES;
/// returns an empty list when unset (fresh unconfigured install). Callers
/// with a parsed config should prefer `from_pairs(config.zones)`.
pub fn configured() -> Vec<ZoneIdent> {
    match env::var("LOCALSKY_ZONES") {
        Ok(s) => parse_env(&s),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_handles_bare_slugs() {
        let zs = parse_env("back_yard,front_yard");
        assert_eq!(zs.len(), 2);
        assert_eq!(zs[0].slug, "back_yard");
        assert_eq!(zs[0].display_name, "Back Yard");
        assert_eq!(zs[1].slug, "front_yard");
        assert_eq!(zs[1].display_name, "Front Yard");
    }

    #[test]
    fn parse_env_handles_slug_name_pairs() {
        let zs = parse_env("back:Backyard,drip_xeri:Drip / Xeriscape");
        assert_eq!(zs.len(), 2);
        assert_eq!(zs[0].slug, "back");
        assert_eq!(zs[0].display_name, "Backyard");
        assert_eq!(zs[1].slug, "drip_xeri");
        assert_eq!(zs[1].display_name, "Drip / Xeriscape");
    }

    #[test]
    fn parse_env_trims_whitespace() {
        let zs = parse_env("  back_yard  ,  front_yard:Front Yard  ");
        assert_eq!(zs.len(), 2);
        assert_eq!(zs[0].slug, "back_yard");
        assert_eq!(zs[1].slug, "front_yard");
        assert_eq!(zs[1].display_name, "Front Yard");
    }

    #[test]
    fn parse_env_skips_empty_entries() {
        let zs = parse_env(",back_yard,,front_yard,");
        assert_eq!(zs.len(), 2);
    }

    #[test]
    fn humanize_handles_underscores() {
        assert_eq!(humanize("back_yard"), "Back Yard");
        assert_eq!(humanize("back_yard_shrubs"), "Back Yard Shrubs");
        assert_eq!(humanize("zone1"), "Zone1");
    }
}
