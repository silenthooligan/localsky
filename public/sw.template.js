// LocalSky service worker: Web Push + a single offline shell.
//
// CACHING POLICY (deliberately tiny):
//   - The ONLY thing this SW precaches is the branded offline shell
//     (/offline.html) plus the PWA manifest + app icons. These are versioned
//     static files that are safe to serve stale.
//   - /pkg/* (the content-hashed JS/WASM/CSS) is NEVER touched by the SW: no
//     precache, no cache-first, no respondWith. hash-files (Cargo.toml +
//     LEPTOS_HASH_FILES) gives every build immutable /pkg URLs, so the browser
//     busts them itself. Caching /pkg here is exactly the bug we fought
//     (LinkError from a stale js/wasm pair desyncing across a deploy); do not
//     reintroduce it.
//   - Every other request (HTML navigations, /api, etc.) is network-first /
//     pass-through. Navigations only fall back to the cached offline shell when
//     the network throws (genuine offline / server unreachable), so an installed
//     PWA shows an on-brand page instead of the raw browser dino.
//
// SW_VERSION is interpolated per deploy by the Rust /sw.js handler so a
// byte-different /sw.js installs updated logic immediately. Bumping the cache
// name with it makes a deploy re-fetch a fresh offline shell.

const VERSION = '__SW_VERSION__';
const SHELL_CACHE = `localsky-shell-${VERSION}`;
const OFFLINE_URL = '/offline.html';
// Small, stable, safe-to-serve-stale static assets. NOT /pkg/* (hashed) and
// NOT any HTML route other than the dedicated offline shell.
const PRECACHE = [
  OFFLINE_URL,
  '/manifest.webmanifest',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
];

// Cache a precache entry only when the response is a real 200 and was not
// redirected (a redirect to /login under the auth gate must never be stored as
// the offline shell). Failures are swallowed so one missing icon can't abort
// the whole install.
async function precacheSafe(cache, url) {
  try {
    const res = await fetch(url, { cache: 'no-cache' });
    if (res && res.ok && !res.redirected) {
      await cache.put(url, res.clone());
    }
  } catch {
    /* offline at install time, or asset missing: skip, never block install */
  }
}

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL_CACHE);
    await Promise.all(PRECACHE.map((url) => precacheSafe(cache, url)));
    self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    // Drop every cache that is not the current shell cache. This both migrates
    // off the old caching SW (localsky-assets/-snapshots-*) and prunes prior
    // shell caches from earlier SW_VERSIONs, so only one fresh shell survives.
    const keys = await caches.keys();
    await Promise.all(keys.filter((k) => k !== SHELL_CACHE).map((k) => caches.delete(k)));
    await self.clients.claim();
  })());
});

// ---- Offline shell (navigations only) ----
//
// Network-first: always try the real server. Only when the fetch rejects (the
// device is offline / the server is unreachable) do we serve the cached offline
// shell. We scope respondWith to navigation requests so /pkg, /api, images, and
// everything else stay pure pass-through and keep their normal caching/hashing.
self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET' || req.mode !== 'navigate') {
    return; // pass-through: SW does not touch non-navigation requests
  }
  event.respondWith((async () => {
    try {
      return await fetch(req);
    } catch {
      const cache = await caches.open(SHELL_CACHE);
      const shell = await cache.match(OFFLINE_URL);
      return (
        shell ||
        new Response('You are offline.', {
          status: 503,
          headers: { 'Content-Type': 'text/plain; charset=utf-8' },
        })
      );
    }
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
