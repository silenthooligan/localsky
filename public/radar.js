// Multi-layer Leaflet bootstrap for the Live Radar panel.
//
// Layers:
//   • Precipitation (RainViewer animated tiles, current + nowcast)
//   • Satellite IR (RainViewer most-recent IR frame)
//   • NEXRAD reflectivity (Iowa Mesonet WMS, US-only but high-res)
//   • Tempest strike rings (local, sourced from /api/snapshot)
//
// Center/zoom default to data-lat / data-lon / data-zoom on #radar-map
// (set by SSR from the WEATHER_APP_LAT/LON/ZOOM env vars). The Tempest
// strike layer pulls from /api/snapshot every 30s — Tempest reports
// distance to a strike but not bearing, so each strike is plotted as a
// distance ring centered on the station rather than a point.
//
// Layer visibility persists per-browser via localStorage (PREFS_KEY
// below), seeded from data-default-layers (config ui.radar.default_layers).
// NEXRAD is region-gated server-side via data-nexrad: IEM's n0r composite
// covers the contiguous US only, so outside that box the layer is never
// even constructed.

(function () {
  // The radar lives on a Leptos route that gets mounted/unmounted on
  // client-side navigation. Plain DOMContentLoaded-once init was fine
  // when every nav was a full page reload, but after switching the
  // top nav to <A> (router-driven swap), the second visit to /
  // showed a dead map: the script tag is deduped by URL, the IIFE
  // already ran, and the new #radar-map element never gets bootstrapped.
  //
  // Fix: a MutationObserver watches the body for the element appearing
  // and disappearing, calling init/teardown so the lifecycle tracks
  // route changes. Multiple visits work; multiple maps don't pile up.

  var currentMap = null;
  var radarPollTimer = null;
  var strikePollTimer = null;
  var animationTimer = null;

  function teardownExisting() {
    if (animationTimer) { clearTimeout(animationTimer); animationTimer = null; }
    if (radarPollTimer) { clearInterval(radarPollTimer); radarPollTimer = null; }
    if (strikePollTimer) { clearInterval(strikePollTimer); strikePollTimer = null; }
    if (currentMap) {
      try { currentMap.remove(); } catch (e) { /* swallow */ }
      currentMap = null;
    }
  }

  function init() {
    var el = document.getElementById('radar-map');
    if (!el) return;
    if (typeof L === 'undefined') {
      // Leaflet's <script> hasn't finished loading yet. Try again.
      setTimeout(init, 250);
      return;
    }
    // If the element we're handed is the SAME one that holds an
    // already-running map, leave it alone. If it's a fresh element
    // (post-route-swap), tear down any prior map and re-init.
    if (currentMap && currentMap.getContainer() === el) return;
    teardownExisting();
    el.dataset.bootstrapped = 'yes';

    var lat = parseFloat(el.dataset.lat || '40.0');
    var lon = parseFloat(el.dataset.lon || '-75.0');
    var zoom = parseInt(el.dataset.zoom || '8', 10);

    // Server-side region verdict: data-nexrad="0" means the configured
    // location sits outside IEM's n0r composite footprint (contiguous US
    // only; Alaska, Hawaii, and Puerto Rico don't count). The "1"
    // fallback mirrors the 40/-75 CONUS defaults above so a non-SSR
    // mount stays self-consistent.
    var nexradOk = (el.dataset.nexrad || '1') !== '0';
    // Config-driven default layer ids (ui.radar.default_layers). The
    // fallback matches the stock config, which in turn matches the old
    // hardcoded precip + NEXRAD + strikes behavior. Only attribute
    // ABSENCE falls back: a deliberately empty configured list renders
    // data-default-layers="" and means start with everything off.
    var defaultLayersAttr = el.dataset.defaultLayers;
    if (defaultLayersAttr == null) defaultLayersAttr = 'precip,nexrad,lightning';
    var defaultLayerIds = defaultLayersAttr
      .split(',')
      .map(function (s) { return s.trim(); })
      .filter(function (s) { return s.length > 0; });

    // Mobile detection: the bottom-tab breakpoint matches the rest of the
    // app (760px). On a phone we move the zoom control to the bottom-right
    // (easier thumb reach), tighten the layer toggle (collapsed by default
    // — at full width it covered most of the map), and turn off `tap` so
    // single-tap-then-drag doesn't get eaten by Leaflet's tap-handler quirk
    // on iOS Safari. attributionControl moves to bottom-right out of the
    // way of the play button.
    var isMobile = (typeof window.matchMedia === 'function')
      ? window.matchMedia('(max-width: 760px)').matches
      : false;

    var map = L.map(el, {
      zoomControl: false,
      attributionControl: false,
      preferCanvas: true,
      // Allow zoom 0-19 across the full Leaflet range so the user can
      // pull back to continental scale or punch in to street level.
      // Each overlay below explicitly inherits these bounds via its
      // own minNativeZoom / maxNativeZoom so tiles stretch at extreme
      // zooms instead of failing to load.
      minZoom: 0,
      maxZoom: 19,
      worldCopyJump: true,
      // Mobile gesture polish: `tap: false` skips Leaflet's custom tap
      // handler, which on iOS Safari can swallow the second tap of a
      // double-tap-zoom. `tapTolerance` higher tolerates fat-finger
      // jitter. `inertia: true` (default) gives momentum-pan, which
      // feels native on touch.
      tap: !isMobile,
      tapTolerance: isMobile ? 24 : 15,
      // Bigger zoom delta on touch so each tap of +/- moves a meaningful
      // amount; pinch-zoom is unaffected.
      zoomSnap: isMobile ? 1 : 0.5,
    }).setView([lat, lon], zoom);

    // Recompute tile coverage whenever the .radar-map element resizes. On
    // wide screens the radar lives in a flex-grown side column so its
    // height tracks the left column's content; without invalidateSize()
    // Leaflet only paints tiles at the initial container size and the
    // fresh whitespace below falls back to leaflet.css's default
    // .leaflet-container background.
    if (typeof ResizeObserver !== 'undefined') {
      new ResizeObserver(function () { map.invalidateSize(); }).observe(el);
    }

    L.control
      .zoom({ position: isMobile ? 'bottomright' : 'topleft' })
      .addTo(map);
    L.control
      .attribution({ position: 'bottomleft', prefix: false })
      .addTo(map);

    L.tileLayer(
      'https://{s}.basemaps.cartocdn.com/dark_nolabels/{z}/{x}/{y}{r}.png',
      {
        attribution: '© OpenStreetMap · © CARTO',
        subdomains: 'abcd',
        minZoom: 0,
        maxZoom: 19,
      }
    ).addTo(map);

    var stationMarker = L.circleMarker([lat, lon], {
      radius: 7,
      color: '#7ed957',
      fillColor: '#7ed957',
      fillOpacity: 0.85,
      weight: 2,
    })
      .bindTooltip('Tempest', { permanent: false, direction: 'top' })
      .addTo(map);

    // Authoritative center. The data-* attrs are correct on a server-
    // rendered load but fall back to a default on client-side (SPA)
    // navigation, so the map could open on the wrong region until a
    // refresh. Fetch the configured location (config-or-env) and recenter
    // the map + station marker; updating lat/lon here also means any
    // strike rings drawn afterward use the true center.
    fetch('/api/v1/location')
      .then(function (r) { return r.json(); })
      .then(function (d) {
        if (d && isFinite(d.lat) && isFinite(d.lon)) {
          lat = d.lat;
          lon = d.lon;
          if (isFinite(d.zoom)) zoom = d.zoom;
          map.setView([lat, lon], zoom);
          stationMarker.setLatLng([lat, lon]);
        }
      })
      .catch(function () { /* keep the data-* center on failure */ });

    // ---------- Layer 1: Precipitation (RainViewer animated) ----------

    currentMap = map;

    var radarLayer = L.layerGroup();
    var radarTiles = {};
    var radarFrames = [];
    var radarPastCount = 0;
    var radarCurrent = 0;
    var radarPlaying = true;
    // Aliases the outer-scope animationTimer so teardownExisting()
    // can clear it on route swap. Same goes for the polling timers
    // assigned at the bottom of init.
    var radarTimer = null;

    // Zoom-driven opacity: at low zoom RainViewer's animated reflectivity
    // is dominant (it caps at z=7 native, so above that it'd just be
    // pixelated). At high zoom NEXRAD takes over because Iowa Mesonet's
    // WMS reprojects at any scale with native ~250m source detail.
    // Linear blend between z=6 (RainViewer-only) and z=9 (NEXRAD-only)
    // gives a 3-zoom-level crossfade that reads naturally as the user
    // pulls in. Outside that range the layer with no detail is faded
    // to near-zero so it doesn't muddy the picture.
    function rvOpacityForZoom(z) {
      // Outside NEXRAD coverage there is nothing to crossfade into, so
      // RainViewer holds full strength at every zoom (pixelated past
      // z=7, but pixelated beats a blank map).
      if (!nexradOk) return 0.65;
      var t = Math.max(0, Math.min(1, (z - 6) / 3));
      return 0.65 * (1 - t * 0.78);  // 0.65 at z=6 → 0.14 at z=9
    }
    function nxOpacityForZoom(z) {
      if (!nexradOk) return 0;
      var t = Math.max(0, Math.min(1, (z - 6) / 3));
      return 0.75 * t;                // 0.0 at z=6 → 0.75 at z=9
    }

    // RainViewer's public API caps tile generation at z=7 regardless
    // of tileSize (verified empirically: both 256 and 512 sizes
    // return a "Zoom Level Not Supported" placeholder PNG for z≥8).
    // We use the standard 256 size + maxNativeZoom:7 below so Leaflet
    // stretches the z=7 tile across higher zooms instead of fetching
    // unsupported levels. errorTileUrl handles the rare miss with a
    // transparent 1x1 PNG so users never see the "Not Supported"
    // placeholder text.
    function radarTileUrl(host, frame) {
      return host + frame.path + '/256/{z}/{x}/{y}/2/1_1.png';
    }

    // 1x1 transparent PNG, base64. Served when a tile URL fails so
    // we silently degrade instead of showing the upstream error tile.
    var TRANSPARENT_TILE =
      'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=';

    function showRadarFrame(idx) {
      var visibleOp = rvOpacityForZoom(map.getZoom());
      Object.keys(radarTiles).forEach(function (k) {
        radarTiles[k].setOpacity(parseInt(k, 10) === idx ? visibleOp : 0);
      });
      var f = radarFrames[idx];
      if (f) {
        var d = new Date(f.time * 1000);
        var label = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        var tag =
          idx === radarPastCount - 1
            ? ' (now)'
            : f.time > Date.now() / 1000
            ? ' (forecast)'
            : '';
        var timeEl = document.getElementById('radar-time');
        if (timeEl) timeEl.textContent = label + tag;
      }
    }

    function radarTick() {
      if (!radarPlaying) return;
      radarCurrent = (radarCurrent + 1) % radarFrames.length;
      showRadarFrame(radarCurrent);
      radarTimer = animationTimer = setTimeout(radarTick, 600);
    }

    function loadRainViewer() {
      return fetch('https://api.rainviewer.com/public/weather-maps.json').then(function (r) {
        return r.json();
      });
    }

    function rebuildRadarFrames(data) {
      var host = data.host;
      var past = (data.radar && data.radar.past) || [];
      var nowcast = (data.radar && data.radar.nowcast) || [];
      radarPastCount = past.length;
      radarFrames = past.concat(nowcast);

      Object.keys(radarTiles).forEach(function (k) {
        radarLayer.removeLayer(radarTiles[k]);
      });
      radarTiles = {};
      radarFrames.forEach(function (frame, i) {
        var t = L.tileLayer(radarTileUrl(host, frame), {
          opacity: 0,
          zIndex: 100,
          minZoom: 0,
          maxZoom: 19,
          // RainViewer caps real tile data at z=7. At map zoom > 7
          // Leaflet upscales the z=7 tile (gets pixelated but never
          // errors). Switch to NEXRAD for high-res US detail.
          maxNativeZoom: 7,
          errorTileUrl: TRANSPARENT_TILE,
        });
        radarTiles[i] = t;
        radarLayer.addLayer(t);
      });
      radarCurrent = Math.max(0, past.length - 1);
      showRadarFrame(radarCurrent);
      if (radarTimer) clearTimeout(radarTimer);
      if (radarPlaying) radarTimer = animationTimer = setTimeout(radarTick, 1500);
    }

    // ---------- Layer 2: Satellite IR (RainViewer most-recent) ----------

    var satLayer = L.layerGroup();
    var satTile = null;

    function rebuildSat(data) {
      var host = data.host;
      var ir = (data.satellite && data.satellite.infrared) || [];
      if (!ir.length) return;
      var latest = ir[ir.length - 1];
      if (satTile) satLayer.removeLayer(satTile);
      satTile = L.tileLayer(host + latest.path + '/256/{z}/{x}/{y}/0/0_0.png', {
        opacity: 0.6,
        zIndex: 90,
        minZoom: 0,
        maxZoom: 19,
        // RainViewer IR also caps at z=7; same upscale behavior.
        maxNativeZoom: 7,
        errorTileUrl: TRANSPARENT_TILE,
      });
      satLayer.addLayer(satTile);
    }

    // ---------- Layer 3: NEXRAD via Iowa Environmental Mesonet WMS ----------

    // Outside the CONUS box the layer is never constructed: the WMS
    // would just burn requests to paint nothing. Gating at construction
    // (rather than hiding a control entry) also keeps it out of the
    // chips and turns the zoom crossfade above into a no-op.
    var nexradLayer = null;
    if (nexradOk) {
      nexradLayer = L.tileLayer.wms(
        'https://mesonet.agron.iastate.edu/cgi-bin/wms/nexrad/n0r-t.cgi',
        {
          layers: 'nexrad-n0r-wmst',
          format: 'image/png',
          transparent: true,
          opacity: 0,  // Zoom-blended below; starts hidden, fades in at z>=7.
          minZoom: 0,
          maxZoom: 19,
          // WMS reprojects to whatever bounding box you ask, so this
          // works at any zoom, but at z>=10 the source pixels are
          // already past native NEXRAD resolution. The errorTileUrl
          // covers the occasional Iowa Mesonet timeout.
          errorTileUrl: TRANSPARENT_TILE,
          attribution: 'NEXRAD via Iowa Mesonet',
        }
      );
    }

    // ---------- Layer 4: Local Tempest strike rings ----------
    //
    // Tempest reports distance-to-strike but not bearing, so we draw a
    // pulsing ring at the reported radius around the station for each
    // strike from the last hour. The newest strike is highlighted.

    var strikeLayer = L.layerGroup();
    var lastStrikeIds = new Set();

    function strikeRadiusMeters(distanceMi) {
      return Math.max(distanceMi, 0.1) * 1609.34;
    }

    function refreshStrikes() {
      fetch('/api/snapshot')
        .then(function (r) { return r.json(); })
        .then(function (snap) {
          var strikes = snap.lightning_recent || [];
          var seen = new Set();
          strikes.forEach(function (s, i) {
            var key = s.time_epoch + '_' + s.distance_km;
            seen.add(key);
            if (lastStrikeIds.has(key)) return;
            lastStrikeIds.add(key);
            var miles = s.distance_km * 0.621371;
            var ring = L.circle([lat, lon], {
              radius: strikeRadiusMeters(miles),
              color: '#ffe066',
              weight: 1.4,
              fill: false,
              opacity: 0.0,
            }).addTo(strikeLayer);
            // Pulse-in then settle.
            var t0 = Date.now();
            var pulse = setInterval(function () {
              var age = (Date.now() - t0) / 1500;
              if (age >= 1) {
                ring.setStyle({ opacity: 0.55 });
                clearInterval(pulse);
              } else {
                ring.setStyle({ opacity: 0.85 - age * 0.3 });
              }
            }, 60);
            ring.bindTooltip(
              miles.toFixed(1) + ' mi · ' + new Date(s.time_epoch * 1000).toLocaleTimeString(),
              { sticky: true }
            );
          });
          // Drop rings for strikes that aged out of the last-hour buffer.
          if (seen.size < lastStrikeIds.size) {
            strikeLayer.clearLayers();
            lastStrikeIds = seen;
          }
        })
        .catch(function () {});
    }

    // ---------- Layer toggle + legend ----------
    //
    // Each overlay name is explanatory enough that the toggle label
    // doubles as documentation. The dedicated legend control below
    // explains the dBZ color ramp + glyph meaning in one place.

    // Stable short ids per overlay label. Persistence below and the
    // SSR'd default list both speak these ids, so the long display
    // labels can be reworded freely without invalidating stored prefs.
    var LAYER_IDS = {
      'Precipitation (RainViewer · animated nowcast)': 'precip',
      'NEXRAD reflectivity (US · Iowa Mesonet)': 'nexrad',
      'Satellite IR (cloud cover, synoptic scale)': 'satellite',
      'Tempest lightning rings ⚡ (last hour)': 'lightning',
    };

    var overlays = {
      'Precipitation (RainViewer · animated nowcast)': radarLayer,
    };
    if (nexradLayer) {
      overlays['NEXRAD reflectivity (US · Iowa Mesonet)'] = nexradLayer;
    }
    overlays['Satellite IR (cloud cover, synoptic scale)'] = satLayer;
    overlays['Tempest lightning rings ⚡ (last hour)'] = strikeLayer;

    // Layer visibility prefs, persisted per-browser. The key carries a
    // .v1 suffix so a future schema change can move to .v2 and start
    // clean instead of migrating (or mis-parsing) old blobs. Storage is
    // best-effort throughout: private browsing throws on access, and a
    // map with amnesia beats a map that crashed.
    var PREFS_KEY = 'localsky.radar.layers.v1';

    function loadLayerPrefs() {
      try {
        var raw = window.localStorage.getItem(PREFS_KEY);
        if (!raw) return null;
        var parsed = JSON.parse(raw);
        // Shape check: must be a plain {id: bool} object. Arrays also
        // typeof 'object' and would read every id as undefined (all
        // layers off), so they fall back to defaults like any other
        // malformed blob.
        if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
          return parsed;
        }
      } catch (e) { /* unavailable or unparseable: fall back to defaults */ }
      return null;
    }

    var storedPrefs = loadLayerPrefs();

    function saveLayerPrefs() {
      // Write the full {id: bool} map, not a diff, so the stored blob
      // is always a complete, self-describing snapshot.
      var prefs = {};
      Object.keys(LAYER_IDS).forEach(function (label) {
        var id = LAYER_IDS[label];
        if (overlays[label]) {
          prefs[id] = map.hasLayer(overlays[label]);
        } else if (storedPrefs && typeof storedPrefs[id] === 'boolean') {
          // Region-gated layer (NEXRAD outside CONUS): carry the stored
          // value through untouched so the pref survives a location
          // change back into coverage.
          prefs[id] = storedPrefs[id];
        } else {
          prefs[id] = false;
        }
      });
      try {
        window.localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
      } catch (e) { /* no storage: toggles still work, they just won't stick */ }
    }
    // On mobile we ditch the in-map L.control.layers entirely (even
    // collapsed it covered the map when the user tapped the toggle, which
    // defeats the purpose of an interactive map). Instead we render a
    // horizontal chip row in #radar-layer-chips below the map — same
    // toggles, just outside the canvas where they don't obscure anything.
    // Desktop keeps the in-map control because the side margin makes it
    // free real estate.
    if (!isMobile) {
      L.control
        .layers(null, overlays, {
          position: 'topright',
          collapsed: false,
        })
        .addTo(map);
    } else {
      var chipsContainer = document.getElementById('radar-layer-chips');
      if (chipsContainer) {
        // Build one chip per overlay. Short label drives display; the
        // long label from `overlays` becomes the aria-label so screen
        // readers still get the context. Each chip toggles between
        // .is-on / not-on, mirroring the actual map state, and addLayer
        // / removeLayer drive the visibility.
        var shortLabels = {
          'Precipitation (RainViewer · animated nowcast)': '🌧 Precip',
          'NEXRAD reflectivity (US · Iowa Mesonet)':       '⌬ NEXRAD',
          'Satellite IR (cloud cover, synoptic scale)':    '☁ Sat IR',
          'Tempest lightning rings ⚡ (last hour)':         '⚡ Strikes',
        };
        chipsContainer.innerHTML = '';
        Object.keys(overlays).forEach(function (longLabel) {
          var layer = overlays[longLabel];
          var btn = document.createElement('button');
          btn.type = 'button';
          btn.className = 'radar-layer-chip';
          btn.setAttribute('aria-label', longLabel);
          btn.setAttribute('aria-pressed', 'false');
          btn.textContent = shortLabels[longLabel] || longLabel;

          function syncOn() {
            var on = map.hasLayer(layer);
            btn.classList.toggle('is-on', on);
            btn.setAttribute('aria-pressed', on ? 'true' : 'false');
          }

          btn.addEventListener('click', function () {
            if (map.hasLayer(layer)) {
              map.removeLayer(layer);
            } else {
              layer.addTo(map);
            }
            syncOn();
          });

          // Re-sync when the map adds/removes the layer from elsewhere
          // (e.g. defaults below). Bound to the map so we capture both
          // directions without the chip listening on every layer.
          map.on('layeradd layerremove', syncOn);
          chipsContainer.appendChild(btn);

          // Initial state: layer not yet added at construction time;
          // the syncOn after defaults below picks up the right state.
        });
      }
    }

    // Initial layer set: stored prefs win when present and parseable
    // (the user's own toggles, written on every change below), else the
    // SSR'd config defaults (ui.radar.default_layers; stock config is
    // precip + NEXRAD + strikes). IR normally stays off because it's a
    // different visualization (cloud-top temp, not precipitation) and
    // stacking it on top muddies the reflectivity colors. The two
    // reflectivity layers crossfade by zoom (RainViewer dominant at
    // z<=7 for animation, NEXRAD dominant at z>=9 for detail) so the
    // radar stays sharp wherever the user pans/zooms. A stored or
    // defaulted nexrad pref is silently ignored when the layer is
    // region-gated off, since it never made it into `overlays`.
    Object.keys(overlays).forEach(function (label) {
      var id = LAYER_IDS[label];
      var on = storedPrefs
        ? storedPrefs[id] === true
        : defaultLayerIds.indexOf(id) !== -1;
      if (on) overlays[label].addTo(map);
    });

    // Refresh blend opacities whenever the map zoom settles. Also
    // run once on init so the initial render uses the right values.
    function applyZoomBlend() {
      var z = map.getZoom();
      if (nexradLayer && map.hasLayer(nexradLayer)) {
        nexradLayer.setOpacity(nxOpacityForZoom(z));
      }
      if (map.hasLayer(radarLayer) && radarFrames.length > 0) {
        // Re-apply opacity to the currently-visible RainViewer frame.
        // showRadarFrame computes from rvOpacityForZoom internally.
        showRadarFrame(radarCurrent);
      }
    }
    map.on('zoomend', applyZoomBlend);
    applyZoomBlend();

    // Persist + re-blend on every layer toggle. Desktop's layer control
    // fires the map-level overlayadd/overlayremove pair; the mobile
    // chips call addLayer/removeLayer directly, which only guarantees
    // layeradd/layerremove (overlay* is emitted by L.control.layers,
    // which mobile doesn't build). Listening to all four and filtering
    // to our own overlays covers both paths; LayerGroup children (radar
    // frames, strike rings) also fire layeradd on the map and the
    // filter drops them. Registered after the initial set is applied
    // above so first paint doesn't count as a user toggle.
    function isOverlayLayer(layer) {
      return Object.keys(overlays).some(function (label) {
        return overlays[label] === layer;
      });
    }
    // Leaflet's map.remove() (the route-swap teardown path) detaches
    // every layer one by one with this handler still attached, so
    // without a guard each SPA nav away from the radar would persist a
    // progressively-emptier snapshot and end by clobbering the real
    // prefs with all-false. The map fires 'unload' before that removal
    // loop; latch it and stop persisting.
    var tearingDown = false;
    map.on('unload', function () { tearingDown = true; });
    map.on('overlayadd overlayremove layeradd layerremove', function (e) {
      if (tearingDown || !isOverlayLayer(e.layer)) return;
      // Re-apply blend when a reflectivity layer comes back so it picks
      // up the right zoom-based opacity instead of the layer's default.
      if (e.layer === nexradLayer || e.layer === radarLayer) applyZoomBlend();
      saveLayerPrefs();
    });

    // Custom legend control. Always visible, bottom-left on desktop —
    // briefly collapsible if it gets in the way of mobile zoom controls.
    // On mobile we skip it entirely; the chip row's labels are enough,
    // and the legend's verbose dBZ-ramp prose was just covering the map.
    // (If a future revision wants the legend on mobile, render it as a
    // <details> element below the chip row instead of overlaying the map.)
    var Legend = L.Control.extend({
      options: { position: 'bottomleft' },
      onAdd: function () {
        var div = L.DomUtil.create('div', 'radar-legend');
        div.innerHTML = ''
          + '<div class="radar-legend-head">'
          +   '<span>Legend</span>'
          +   '<button type="button" class="radar-legend-toggle" aria-label="Toggle legend">−</button>'
          + '</div>'
          + '<div class="radar-legend-body">'
          +   '<div class="radar-legend-row">'
          +     '<span class="radar-legend-swatch radar-swatch-precip"></span>'
          +     '<div class="radar-legend-text">'
          +       '<strong>Precipitation</strong>'
          +       '<span>RainViewer animated past + nowcast. dBZ scale: blue light · green moderate · yellow → orange → red heavy. Dominant at zoom ≤ 7.</span>'
          +     '</div>'
          +   '</div>'
          +   '<div class="radar-legend-row">'
          +     '<span class="radar-legend-swatch radar-swatch-nexrad"></span>'
          +     '<div class="radar-legend-text">'
          +       '<strong>NEXRAD reflectivity</strong>'
          +       '<span>Iowa Mesonet WMS. US-only, higher native resolution; same dBZ ramp. Crossfades in as you zoom past z=7 so detail stays sharp at street scale.</span>'
          +     '</div>'
          +   '</div>'
          +   '<div class="radar-legend-row">'
          +     '<span class="radar-legend-swatch radar-swatch-ir"></span>'
          +     '<div class="radar-legend-text">'
          +       '<strong>Satellite IR</strong>'
          +       '<span>Cloud-top temperature. Whiter = colder = taller storm tops. Continental-scale only.</span>'
          +     '</div>'
          +   '</div>'
          +   '<div class="radar-legend-row">'
          +     '<span class="radar-legend-swatch radar-swatch-strike"></span>'
          +     '<div class="radar-legend-text">'
          +       '<strong>Tempest lightning</strong>'
          +       '<span>Yellow ring = strike from your station. Tempest reports distance, not bearing, so each strike is a ring at the reported radius.</span>'
          +     '</div>'
          +   '</div>'
          + '</div>';

        // Collapse toggle.
        L.DomEvent.disableClickPropagation(div);
        var btn = div.querySelector('.radar-legend-toggle');
        var body = div.querySelector('.radar-legend-body');
        btn.addEventListener('click', function () {
          var collapsed = div.classList.toggle('is-collapsed');
          btn.textContent = collapsed ? '+' : '−';
          body.style.display = collapsed ? 'none' : '';
        });
        return div;
      },
    });
    if (!isMobile) {
      new Legend().addTo(map);
    }

    // ---------- Bootstrap ----------

    function refreshAll() {
      loadRainViewer()
        .then(function (data) {
          rebuildRadarFrames(data);
          rebuildSat(data);
        })
        .catch(function (e) { console.error('rainviewer load failed', e); });
    }

    var btn = document.getElementById('radar-play');
    if (btn) {
      btn.addEventListener('click', function () {
        radarPlaying = !radarPlaying;
        btn.textContent = radarPlaying ? '⏸ pause' : '▶ play';
        if (radarPlaying) radarTick();
        else if (radarTimer) clearTimeout(radarTimer);
      });
    }

    refreshAll();
    refreshStrikes();
    radarPollTimer = setInterval(refreshAll, 5 * 60 * 1000);
    // Strike poll: 60s. Was 30s, doubled to halve the per-IP request
    // pressure on the OAuth-gated /api/snapshot endpoint. CF's bot
    // challenges can fire on a remote IP that issues many cookie-bearing
    // requests in quick succession, and the live SSE stream already
    // delivers the same Tempest snapshot in real time — this polling
    // loop is only the radar.js fallback path for environments where
    // the SSE stream isn't open yet (cold load on the weather route).
    strikePollTimer = setInterval(refreshStrikes, 60 * 1000);
  }

  // Watch for the radar element appearing/disappearing as Leptos
  // mounts/unmounts the weather route. The observer is cheap (it
  // only checks one document.getElementById on each batch); the
  // bootstrapped guard inside init() prevents double-init for
  // unrelated DOM mutations on the same route.
  function startObserver() {
    var observer = new MutationObserver(function () {
      var el = document.getElementById('radar-map');
      if (el && (!currentMap || currentMap.getContainer() !== el)) {
        init();
      } else if (!el && currentMap) {
        teardownExisting();
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
  }

  function bootstrap() {
    startObserver();
    init();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', bootstrap);
  } else {
    bootstrap();
  }
})();
