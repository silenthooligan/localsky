// SourceRegistry. Mirrors ControllerRegistry: holds the active set of
// Arc<dyn WeatherSource> behind arc-swap so hot-reload via PUT
// /api/config replaces sources atomically.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::ports::weather_source::WeatherSource;

#[derive(Clone)]
pub struct SourceRegistry {
    inner: Arc<ArcSwap<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    by_id: HashMap<String, Arc<dyn WeatherSource>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(RegistryState::default())),
        }
    }

    pub fn set(&self, sources: Vec<Arc<dyn WeatherSource>>) {
        let mut by_id = HashMap::new();
        for s in sources {
            by_id.insert(s.id().to_string(), s);
        }
        self.inner.store(Arc::new(RegistryState { by_id }));
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn WeatherSource>> {
        self.inner.load().by_id.get(id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<dyn WeatherSource>> {
        self.inner.load().by_id.values().cloned().collect()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.load().by_id.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DemoReplayConfig;
    use crate::sources::demo_replay::DemoReplay;

    fn demo(id: &str) -> Arc<dyn WeatherSource> {
        Arc::new(DemoReplay::new(
            id,
            DemoReplayConfig {
                rate: 10.0,
                replay_path: None,
            },
        ))
    }

    #[test]
    fn set_then_get_roundtrips() {
        let r = SourceRegistry::new();
        r.set(vec![demo("a"), demo("b")]);
        assert!(r.get("a").is_some());
        assert!(r.get("b").is_some());
        assert!(r.get("c").is_none());
    }

    #[test]
    fn all_returns_every_source() {
        let r = SourceRegistry::new();
        r.set(vec![demo("a"), demo("b"), demo("c")]);
        assert_eq!(r.all().len(), 3);
    }

    #[test]
    fn ids_sorted() {
        let r = SourceRegistry::new();
        r.set(vec![demo("z"), demo("a"), demo("m")]);
        assert_eq!(r.ids(), vec!["a".to_string(), "m".to_string(), "z".to_string()]);
    }

    #[test]
    fn set_replaces_atomically() {
        let r = SourceRegistry::new();
        r.set(vec![demo("old")]);
        r.set(vec![demo("new")]);
        assert!(r.get("old").is_none());
        assert!(r.get("new").is_some());
    }
}
