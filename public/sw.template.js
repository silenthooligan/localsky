// LocalSky service worker — push-only (no asset caching).
//
// Why there is no fetch/caching handler: the /pkg JS/WASM/CSS filenames are
// content-hashed (hash-files in Cargo.toml + LEPTOS_HASH_FILES at runtime), so
// every build emits new immutable URLs. The browser's HTTP cache busts them
// automatically and the SSR shell is always fetched fresh, so there is nothing
// for a SW to cache-manage: no stale WASM, no two-reload-after-deploy, and no
// LinkError from a js/wasm pair desyncing across deploys. This replaced a
// version-namespaced caching SW whose only upside was offline rendering, which
// a LAN irrigation dashboard does not need and which kept freezing clients on
// stale bundles.
//
// The SW now exists ONLY for Web Push (showing notifications + routing clicks).
// SW_VERSION is still interpolated per deploy by the Rust /sw.js handler so a
// byte-different /sw.js installs updated push logic immediately.

const VERSION = '__SW_VERSION__';

self.addEventListener('install', () => {
  // Nothing to precache; take over as soon as possible.
  self.skipWaiting();
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    // One-time migration off the old caching SW: delete every cache it left
    // behind (localsky-shell/-assets/-snapshots-*). This SW caches nothing, so
    // any surviving cache is dead weight that could only ever serve stale bytes.
    const keys = await caches.keys();
    await Promise.all(keys.map((k) => caches.delete(k)));
    await self.clients.claim();
  })());
});

// ---- Web Push ----

self.addEventListener('push', (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = { title: 'LocalSky', body: event.data ? event.data.text() : '' };
  }
  const title = payload.title || 'LocalSky';
  const options = {
    body: payload.body || '',
    icon: '/icons/icon-192.png',
    badge: '/icons/icon-192.png',
    tag: payload.tag || 'localsky',
    data: payload.url || '/irrigation',
    renotify: !!payload.renotify,
  };
  event.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const target = event.notification.data || '/irrigation';
  event.waitUntil((async () => {
    const all = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of all) {
      try {
        const u = new URL(client.url);
        if (u.origin === self.location.origin) {
          await client.focus();
          if ('navigate' in client) {
            client.navigate(target).catch(() => {});
          }
          return;
        }
      } catch {}
    }
    await self.clients.openWindow(target);
  })());
});

// Allow the page to ask the SW for its version (nav_log strip / future
// "update available" UI).
self.addEventListener('message', (event) => {
  if (event.data && event.data.type === 'GET_VERSION') {
    event.ports[0]?.postMessage({ version: VERSION });
  }
});
