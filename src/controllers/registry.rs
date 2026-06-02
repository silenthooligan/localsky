// ControllerRegistry. Holds the configured set of IrrigationController
// instances and exposes default_or_named() for dispatch. Construction
// is driven by config.controllers[]; the registry is hot-swappable so a
// PUT /api/config that changes the default controller takes effect on
// the next dispatch without restarting the runtime.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::ports::irrigation_controller::IrrigationController;

#[derive(Clone)]
pub struct ControllerRegistry {
    inner: Arc<ArcSwap<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    by_id: HashMap<String, Arc<dyn IrrigationController>>,
    default_id: Option<String>,
}

impl ControllerRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(RegistryState::default())),
        }
    }

    /// Replace the registry contents atomically. Used by the hot-reload
    /// path when config.controllers changes.
    pub fn set(&self, controllers: Vec<(Arc<dyn IrrigationController>, bool /* default */)>) {
        let mut by_id = HashMap::new();
        let mut default_id = None;
        for (c, is_default) in controllers {
            if is_default {
                default_id = Some(c.id().to_string());
            }
            by_id.insert(c.id().to_string(), c);
        }
        // If no explicit default, pick the first one (deterministic via sort).
        if default_id.is_none() {
            let mut keys: Vec<&String> = by_id.keys().collect();
            keys.sort();
            default_id = keys.into_iter().next().cloned();
        }
        self.inner
            .store(Arc::new(RegistryState { by_id, default_id }));
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn IrrigationController>> {
        self.inner.load().by_id.get(id).cloned()
    }

    pub fn default(&self) -> Option<Arc<dyn IrrigationController>> {
        let s = self.inner.load();
        s.default_id
            .as_ref()
            .and_then(|id| s.by_id.get(id).cloned())
    }

    /// Pick `id` if provided, otherwise the default. Convenience for the
    /// /api/irrigation dispatch path which accepts an optional controller
    /// override in the request payload.
    pub fn default_or_named(&self, id: Option<&str>) -> Option<Arc<dyn IrrigationController>> {
        match id {
            Some(name) => self.get(name),
            None => self.default(),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.load().by_id.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl Default for ControllerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DryRunConfig;
    use crate::controllers::dry_run::DryRunController;

    fn dry(id: &str) -> Arc<dyn IrrigationController> {
        Arc::new(DryRunController::new(
            id,
            DryRunConfig {
                simulate_runs: false,
            },
            None,
        ))
    }

    #[test]
    fn default_returns_marked_controller() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("a"), false), (dry("b"), true), (dry("c"), false)]);
        assert_eq!(r.default().unwrap().id(), "b");
    }

    #[test]
    fn default_falls_back_to_first_sorted_id() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("b"), false), (dry("a"), false)]);
        // No explicit default -> first sorted id wins.
        assert_eq!(r.default().unwrap().id(), "a");
    }

    #[test]
    fn default_or_named_routes_correctly() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("primary"), true), (dry("backup"), false)]);
        assert_eq!(r.default_or_named(None).unwrap().id(), "primary");
        assert_eq!(r.default_or_named(Some("backup")).unwrap().id(), "backup");
        assert!(r.default_or_named(Some("missing")).is_none());
    }

    #[test]
    fn set_replaces_atomic() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("a"), true)]);
        assert_eq!(r.default().unwrap().id(), "a");
        r.set(vec![(dry("b"), true)]);
        assert_eq!(r.default().unwrap().id(), "b");
        assert!(r.get("a").is_none());
    }

    #[test]
    fn ids_returns_sorted() {
        let r = ControllerRegistry::new();
        r.set(vec![
            (dry("z"), false),
            (dry("a"), false),
            (dry("m"), false),
        ]);
        assert_eq!(
            r.ids(),
            vec!["a".to_string(), "m".to_string(), "z".to_string()]
        );
    }
}
