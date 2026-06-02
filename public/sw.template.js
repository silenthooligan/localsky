// LocalSky service worker.
//
// Strategy summary (the rationale lives in this file because every line here
// is load-bearing — get one wrong and the PWA either bricks itself or quietly
// stops getting fresh data):
//
// - SW_VERSION is interpolated at request time by the Rust /sw.js handler from
//   CARGO_PKG_VERSION + GITEA_SHA. Every deploy gets a new SW which forces
//   install -> activate -> old caches deleted, so we never serve stale JS/WASM.
// - Caches are namespaced by version, never reused across versions. Old caches
//   are nuked on activate. No "fall through to a previous version" logic.
// - Action POSTs (/api/irrigation/action) are never touched: method !== 'GET'
//   short-circuits the entire fetch handler.
// - SSE streams (/api/*/stream) are never touched: returning without calling
//   event.respondWith() lets the browser open EventSource as if the SW weren't
//   here. Touching them with respondWith() will buffer the stream and break
//   real-time updates.
// - Navigations are network-first. We refuse to cache redirected/opaqueredirect
//   responses because a reverse proxy 302s to an auth provider on session expiry, and the
//   *worst* PWA failure mode is a SW that caches the auth-redirect HTML and
//   serves it forever.
// - /pkg/* and /icons/* are stale-while-revalidate so the app boots fast on
//   the second visit, and a fresh deploy is picked up on the visit after that.
// - /api/* (snapshots only, not streams) are network-first with cached
//   fallback, so offline-mode renders the most recent snapshot the user saw.

const VERSION       = '__SW_VERSION__';
const SHELL_CACHE   = `localsky-shell-${VERSION}`;
const ASSET_CACHE   = `localsky-assets-${VERSION}`;
const SNAPSHOT_CACHE = `localsky-snapshots-${VERSION}`;

// What we precache on install. Keep this list small and stable: anything in
// here must be reachable without auth (Caddy public-asset exemption) and
// must not 404, or `cache.addAll` rejects and the install fails.
const SHELL_URLS = [
  '/manifest.webmanifest',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
  '/icons/apple-touch-180.png',
];

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL_CACHE);
    // Use individual put() calls instead of addAll so a single 404 doesn't
    // abort the install. Each one logged so we can see what's missing.
    for (const url of SHELL_URLS) {
      try {
        const resp = await fetch(url, { credentials: 'same-origin' });
        if (resp.ok && !resp.redirected) {
          await cache.put(url, resp);
        } else {
          console.warn('[sw] precache skip', url, resp.status, resp.redirected);
        }
      } catch (e) {
        console.warn('[sw] precache fail', url, e);
      }
    }
    await self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    const keys = await caches.keys();
    await Promise.all(
      keys
        .filter((k) => !k.endsWith(`-${VERSION}`))
        .map((k) => caches.delete(k))
    );
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const req = event.request;

  // Never intercept anything but GET. Action POSTs to /api/irrigation/action
  // (and any future POST/PUT/DELETE) must hit network unmodified.
  if (req.method !== 'GET') return;

  let url;
  try {
    url = new URL(req.url);
  } catch {
    return;
  }

  // Cross-origin: bypass entirely. Leaflet CSS, push endpoints, anything
  // hosted off our origin is the browser's problem, not ours.
  if (url.origin !== self.location.origin) return;

  // SSE: bypass entirely. Anything ending in /stream is a long-lived
  // text/event-stream that must not pass through respondWith().
  if (url.pathname.endsWith('/stream')) return;

  // Static build output and icons: stale-while-revalidate.
  if (url.pathname.startsWith('/pkg/') || url.pathname.startsWith('/icons/')) {
    event.respondWith(staleWhileRevalidate(req, ASSET_CACHE));
    return;
  }

  // Manifest: cache-first (rarely changes, version namespace handles
  // invalidation across deploys).
  if (url.pathname === '/manifest.webmanifest') {
    event.respondWith(cacheFirst(req, SHELL_CACHE));
    return;
  }

  // API snapshots (NOT streams): network-first with a cached fallback for
  // offline mode. We only cache /snapshot endpoints; explanation/anomaly/
  // history/action all bypass the cache.
  if (url.pathname.startsWith('/api/') && url.pathname.endsWith('/snapshot')) {
    event.respondWith(networkFirst(req, SNAPSHOT_CACHE));
    return;
  }

  // Navigations: network-first with redirect-aware guards. If network fails
  // entirely, fall back to a cached shell (only after we've successfully
  // visited the route once before).
  if (req.mode === 'navigate') {
    event.respondWith(navigationStrategy(req, SHELL_CACHE));
    return;
  }

  // Everything else: pass through to the network without intercepting.
});

async function staleWhileRevalidate(req, cacheName) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(req);
  const network = fetch(req)
    .then((resp) => {
      if (resp && resp.ok && !resp.redirected && resp.status === 200) {
        cache.put(req, resp.clone()).catch(() => {});
      }
      return resp;
    })
    .catch(() => cached);
  return cached || network;
}

async function cacheFirst(req, cacheName) {
  const cache = await caches.open(cacheName);
  const cached = await cache.match(req);
  if (cached) return cached;
  const resp = await fetch(req);
  if (resp && resp.ok && !resp.redirected && resp.status === 200) {
    cache.put(req, resp.clone()).catch(() => {});
  }
  return resp;
}

async function networkFirst(req, cacheName) {
  const cache = await caches.open(cacheName);
  try {
    const resp = await fetch(req);
    if (resp && resp.ok && !resp.redirected && resp.status === 200) {
      cache.put(req, resp.clone()).catch(() => {});
    }
    return resp;
  } catch (e) {
    const cached = await cache.match(req);
    if (cached) return cached;
    throw e;
  }
}

async function navigationStrategy(req, cacheName) {
  const cache = await caches.open(cacheName);
  try {
    const resp = await fetch(req);
    // Refuse to cache anything that looks like an auth redirect or non-200.
    // type === 'opaqueredirect' happens with redirect: 'manual'; redirected
    // === true happens with the default redirect: 'follow' when we've been
    // bounced through OAuth.
    if (
      resp &&
      resp.ok &&
      !resp.redirected &&
      resp.type !== 'opaqueredirect' &&
      resp.status === 200
    ) {
      cache.put(req, resp.clone()).catch(() => {});
    }
    return resp;
  } catch (e) {
    const cached = await cache.match(req);
    if (cached) return cached;
    // Last resort: try to serve any cached navigation to give the user
    // *something* rather than a generic browser error.
    const anyCached = await cache.match('/') || await cache.match('/irrigation');
    if (anyCached) return anyCached;
    throw e;
  }
}

// ---- Web Push (wired in Phase 5; handlers live here so Phase 1b doesn't
// need a second SW deploy when push lands) ----

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

// Allow the page to ask the SW for its version (useful for the nav_log strip
// and for "Update available" UI later).
self.addEventListener('message', (event) => {
  if (event.data && event.data.type === 'GET_VERSION') {
    event.ports[0]?.postMessage({ version: VERSION });
  }
});
