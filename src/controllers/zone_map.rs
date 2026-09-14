// The zone-to-station binding every controller carries: a LocalSky zone
// slug maps to whatever the board or cloud calls the valve (a station
// index, a relay number, a vendor zone id). Four adapters each had a
// `station_for` copy and two reverse lookups; this is the one.

use std::collections::HashMap;

use crate::ports::irrigation_controller::ControllerError;

#[derive(Debug, Clone, Default)]
pub struct ZoneMap<S: Clone> {
    forward: HashMap<String, S>,
}

impl<S: Clone + PartialEq> ZoneMap<S> {
    pub fn new(forward: HashMap<String, S>) -> Self {
        Self { forward }
    }

    /// The station bound to a zone, or `ZoneUnknown` with the slug.
    pub fn station_for(&self, slug: &str) -> Result<S, ControllerError> {
        self.forward
            .get(slug)
            .cloned()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    /// The zone bound to a station, when one is.
    pub fn slug_for(&self, station: &S) -> Option<&str> {
        self.forward
            .iter()
            .find(|(_, s)| *s == station)
            .map(|(slug, _)| slug.as_str())
    }

    /// Every bound zone slug, sorted, for the unknown-zone error body.
    pub fn slugs(&self) -> Vec<String> {
        let mut v: Vec<String> = self.forward.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    pub fn len(&self) -> usize {
        self.forward.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &S)> {
        self.forward.iter()
    }

    pub fn as_map(&self) -> &HashMap<String, S> {
        &self.forward
    }
}

impl<S: Clone + PartialEq> From<HashMap<String, S>> for ZoneMap<S> {
    fn from(m: HashMap<String, S>) -> Self {
        Self::new(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_reverse_and_the_unknown_error() {
        let m: ZoneMap<u32> =
            HashMap::from([("front".to_string(), 1u32), ("back".to_string(), 2)]).into();
        assert_eq!(m.station_for("front").unwrap(), 1);
        assert_eq!(m.slug_for(&2), Some("back"));
        assert_eq!(m.slug_for(&9), None);
        assert_eq!(m.slugs(), vec!["back", "front"]);
        assert!(matches!(
            m.station_for("nope"),
            Err(ControllerError::ZoneUnknown(s)) if s == "nope"
        ));
    }
}
