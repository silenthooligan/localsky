// Single source of truth for the irrigation zone list.
//
// The runtime learns the zones at boot from one of three sources, in
// priority order:
//
//   1. The config file (config.zones) when /data/localsky.toml exists.
//      The wizard writes this on first-run. Future iterations will
//      consume the per-zone metadata directly; today the refresher
//      uses only the keys.
//   2. The LOCALSKY_ZONES env var. CSV of either bare slugs
//      ("back_yard,front_yard") or "slug:Display Name" pairs
//      ("back_yard:Back Yard,front_yard:Front Yard"). Operators with
//      more or fewer than the default four zones can override without
//      a recompile.
//   3. The four legacy slugs the homelab fleet was originally written
//      against. Used when neither of the above is present so existing
//      deployments stay working.

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

/// Default zone list used when nothing else is configured. Matches the
/// legacy hardcoded fleet.
fn legacy_default() -> Vec<ZoneIdent> {
    vec![
        ZoneIdent::new("back_yard", "Back Yard"),
        ZoneIdent::new("front_yard", "Front Yard"),
        ZoneIdent::new("side_yard", "Side Yard"),
        ZoneIdent::new("back_yard_shrubs", "Back Yard Shrubs"),
    ]
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

/// Resolve the active zone list. Reads LOCALSKY_ZONES first; otherwise
/// returns the legacy default.
pub fn configured() -> Vec<ZoneIdent> {
    match env::var("LOCALSKY_ZONES") {
        Ok(s) => {
            let parsed = parse_env(&s);
            if parsed.is_empty() {
                legacy_default()
            } else {
                parsed
            }
        }
        Err(_) => legacy_default(),
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
