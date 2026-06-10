// DeviceRegistry. Mirrors SourceRegistry/ControllerRegistry: holds the
// active device set behind arc-swap so a config hot-reload (PUT /api/config)
// can rebuild and swap it atomically. The set is derived (built from the
// configured sources + controllers by `build_devices`), so the registry is
// a cache of topology rather than an owner of hardware connections.

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::Device;

/// Two independent slices that `all()` merges: config-derived native devices
/// (rebuilt on config reload) and HA-imported devices (refreshed by a
/// background task). Kept separate so each can be replaced without touching
/// the other.
#[derive(Default)]
struct RegistryState {
    config: Vec<Device>,
    ha: Vec<Device>,
}

#[derive(Clone)]
pub struct DeviceRegistry {
    inner: Arc<ArcSwap<RegistryState>>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(RegistryState::default())),
        }
    }

    /// Replace the config-derived (native) device set. Kept sorted by the
    /// builder so reads have a stable order.
    pub fn set(&self, devices: Vec<Device>) {
        let cur = self.inner.load();
        self.inner.store(Arc::new(RegistryState {
            config: devices,
            ha: cur.ha.clone(),
        }));
    }

    /// Replace the HA-imported device set (Phase F1b background refresh).
    pub fn set_ha(&self, devices: Vec<Device>) {
        let cur = self.inner.load();
        self.inner.store(Arc::new(RegistryState {
            config: cur.config.clone(),
            ha: devices,
        }));
    }

    /// Snapshot of every device: native + HA-imported, reconciled (F3) so a
    /// physical device present on both sides shows once (native, badged
    /// also_in_ha) rather than twice. Sorted by id.
    pub fn all(&self) -> Vec<Device> {
        let s = self.inner.load();
        super::reconcile::reconcile(&s.config, &s.ha)
    }

    /// One device by id.
    pub fn get(&self, id: &str) -> Option<Device> {
        self.all().into_iter().find(|d| d.id == id)
    }

    pub fn len(&self) -> usize {
        let s = self.inner.load();
        s.config.len() + s.ha.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{DeviceKind, DeviceOrigin};

    fn dev(id: &str) -> Device {
        Device {
            id: id.to_string(),
            kind: DeviceKind::Virtual,
            name: id.to_string(),
            model: None,
            identity: None,
            origin: DeviceOrigin::Native,
            source_id: None,
            online: None,
            last_seen_epoch: None,
            also_in_ha: false,
            children: Vec::new(),
        }
    }

    #[test]
    fn set_then_get_and_all() {
        let r = DeviceRegistry::new();
        assert!(r.is_empty());
        r.set(vec![dev("source:a"), dev("controller:b")]);
        assert_eq!(r.len(), 2);
        assert!(r.get("source:a").is_some());
        assert!(r.get("controller:b").is_some());
        assert!(r.get("missing").is_none());
        assert_eq!(r.all().len(), 2);
    }

    #[test]
    fn set_replaces_atomically() {
        let r = DeviceRegistry::new();
        r.set(vec![dev("old")]);
        r.set(vec![dev("new")]);
        assert!(r.get("old").is_none());
        assert!(r.get("new").is_some());
    }

    #[test]
    fn config_and_ha_sets_merge_independently() {
        let r = DeviceRegistry::new();
        r.set(vec![dev("source:a")]);
        r.set_ha(vec![dev("ha:x"), dev("ha:y")]);
        assert_eq!(r.len(), 3);
        assert!(r.get("source:a").is_some());
        assert!(r.get("ha:x").is_some());
        // Replacing HA devices leaves config devices intact, and vice versa.
        r.set_ha(vec![dev("ha:z")]);
        assert!(r.get("source:a").is_some());
        assert!(r.get("ha:x").is_none());
        assert!(r.get("ha:z").is_some());
        assert_eq!(r.len(), 2);
        // all() is sorted by id.
        let ids: Vec<_> = r.all().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["ha:z".to_string(), "source:a".to_string()]);
    }
}
