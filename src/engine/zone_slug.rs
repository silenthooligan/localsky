// A zone's identity, normalized once.
//
// Zone keys come from the operator's own TOML table headers, so they
// arrive in whatever spelling they typed: `[zones.back-yard]` and
// `[zones.back_yard]` are the same yard to a person and two different
// strings to a HashMap. The runtime settled on underscores, so every
// consumer normalized on its own, and there are 42 hand-written
// `replace('-', "_")` calls across the tree doing it.
//
// Every one of those is a chance to forget. A lookup that forgets does
// not fail loudly, it misses, and a miss falls through to a default:
// a zone quietly gets catalog agronomy instead of its own, or drops out
// of a per-zone map while staying in the others. That is the same shape
// as the defect this release exists to fix, one level up. Two spellings
// of one identity, held apart, with nothing making disagreement
// impossible.
//
// So identity gets a type whose only constructor normalizes. There is no
// way to build one that skipped the step.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A zone's canonical identity.
///
/// Construct with [`ZoneSlug::new`], which is the only way in, so a
/// `ZoneSlug` is normalized by construction. Ordering and hashing are on
/// the canonical form, so two spellings of one yard are one key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ZoneSlug(String);

/// Deserialize through the constructor, not around it.
///
/// A derived `transparent` impl would hand the inner String straight
/// back, so a slug arriving from stored config or an API payload could
/// skip normalization and reintroduce exactly the two-spellings problem
/// this type exists to remove. There is no way in that does not
/// normalize, including this one.
impl<'de> Deserialize<'de> for ZoneSlug {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::new(String::deserialize(d)?))
    }
}

impl ZoneSlug {
    /// Normalize an operator-supplied key.
    ///
    /// Hyphens become underscores, and surrounding whitespace goes,
    /// because a TOML table header can carry it and the operator cannot
    /// see it. Case is deliberately NOT folded: slugs are already
    /// lowercase by convention, and folding here would silently merge two
    /// zones an operator meant to keep apart.
    pub fn new(raw: impl AsRef<str>) -> Self {
        Self(raw.as_ref().trim().replace('-', "_"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ZoneSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ZoneSlug {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for ZoneSlug {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// Compare against a raw string the same way a lookup would, so callers
/// holding an un-normalized key still match.
impl PartialEq<str> for ZoneSlug {
    fn eq(&self, other: &str) -> bool {
        self.0 == ZoneSlug::new(other).0
    }
}

impl PartialEq<&str> for ZoneSlug {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The two spellings an operator can write are one identity.
    ///
    /// This is not hypothetical: a real deployment's config uses
    /// `[zones.back-yard]` while the runtime keys on `back_yard`.
    #[test]
    fn the_two_spellings_of_one_yard_are_one_key() {
        assert_eq!(ZoneSlug::new("back-yard"), ZoneSlug::new("back_yard"));
        let mut m = HashMap::new();
        m.insert(ZoneSlug::new("back-yard"), 1);
        assert_eq!(m.get(&ZoneSlug::new("back_yard")), Some(&1));
        assert_eq!(m.len(), 1, "one yard, one entry");
    }

    /// Whitespace a TOML header can carry and an operator cannot see.
    #[test]
    fn surrounding_whitespace_does_not_make_a_second_zone() {
        assert_eq!(ZoneSlug::new(" front_yard "), ZoneSlug::new("front_yard"));
    }

    /// Case is NOT folded. Two zones an operator spelled differently on
    /// purpose stay two zones; merging them would lose a yard.
    #[test]
    fn case_is_left_alone() {
        assert_ne!(ZoneSlug::new("Back_Yard"), ZoneSlug::new("back_yard"));
    }

    /// There is no constructor that skips normalization, which is the
    /// whole point. Round-tripping through the wire form normalizes too.
    #[test]
    fn the_wire_form_is_normalized_on_the_way_in() {
        let s: ZoneSlug = serde_json::from_str("\"back-yard\"").expect("parses");
        assert_eq!(s.as_str(), "back_yard");
        // And serializes as a bare string, so stored config and API
        // payloads keep the shape they already had.
        assert_eq!(
            serde_json::to_string(&ZoneSlug::new("back-yard")).expect("serializes"),
            "\"back_yard\""
        );
    }

    /// Comparing against a raw operator string matches the way a lookup
    /// would, so a caller holding an un-normalized key is not surprised.
    #[test]
    fn comparing_against_a_raw_string_normalizes_it_first() {
        assert!(ZoneSlug::new("back_yard") == "back-yard");
        assert!(ZoneSlug::new("back_yard") == "back_yard");
        assert!(ZoneSlug::new("back_yard") != "front_yard");
    }
}
