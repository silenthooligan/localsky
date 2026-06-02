// DeviceRegistry. Mirrors SourceRegistry/ControllerRegistry: holds the
// active device set behind arc-swap so a config hot-reload (PUT /api/config)
// can rebuild and swap it atomically. The set is derived (built from the
// configured sources + controllers by `build_devices`), so the registry is
// a cache of topology rather than an owner of hardware connections.

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::Device;

#[derive(Clone)]
pub struct DeviceRegistry {
    inner: Arc<ArcSwap<Vec<Device>>>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(Vec::new())),
        }
    }

    /// Replace the whole device set atomically. The list is kept sorted by
    /// id by the builder so reads have a stable order.
    pub fn set(&self, devices: Vec<Device>) {
        self.inner.store(Arc::new(devices));
    }

    /// Snapshot of every device.
    pub fn all(&self) -> Vec<Device> {
        self.inner.load().as_ref().clone()
    }

    /// One device by id.
    pub fn get(&self, id: &str) -> Option<Device> {
        self.inner.load().iter().find(|d| d.id == id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.load().is_empty()
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
}
