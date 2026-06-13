// Multi-layer Leaflet bootstrap for the Live Radar panel.
//
// The layer menu is built DYNAMICALLY from two JSON attributes that the
// server renders on #radar-map from the Rust radar catalog
// (src/radar_catalog.rs):
//
//   data-radar-providers : effective provider descriptors (config-resolved
//                          or recommended-by-region). kind "rainviewer"
//                          drives the animated frame machinery; kind "wms"
//                          becomes an L.tileLayer.wms overlay.
//   data-radar-features  : feature descriptors. Known ids:
//                          nowcast          RainViewer forecast frames
//                          warnings_us      NWS active-alert polygons
//                          hurricanes       tropical cyclones, all basins
//                                           (id kept for pref persistence;
//                                           label is basin-localized:
//                                           hurricanes/typhoons/cyclones;
//                                           data via the normalized
//                                           /api/v1/radar/tropical feed)
//                          lightning_tempest lightning strikes (id kept
//                                           for pref persistence; covers
//                                           Tempest rings AND Blitzortung
//                                           positioned dots)
//                          wind             Open-Meteo particle wind flow
//
// Center/zoom default to data-lat / data-lon / data-zoom on #radar-map
// (set by SSR from the configured station location). The lightning layer
// pulls from /api/snapshot. Tempest reports distance to a strike but not
// bearing, so Tempest strikes plot as distance rings centered on the
// station. Strikes carrying real lat/lon (the opt-in Blitzortung
// community source, merged server-side into lightning_recent with a
// source tag) plot as small positioned dots that fade with age. The
// lightning legend copy in the Layers drawer and the Leaflet
// attribution line adapt to whichever networks actually contributed;
// the Blitzortung credit (CC BY-SA 4.0) is required by their terms
// whenever community strikes are shown.
//
// Layer visibility persists per-browser via localStorage (PREFS_KEY
// below), seeded from data-default-layers (config ui.radar.default_layers).
// The stored blob extends naturally to any catalog id; unknown stored ids
// are ignored on read and carried through on write so a pref survives the
// layer temporarily leaving the effective set (e.g. a region change).
//
// Every external source degrades silently to whatever still works: at most
// one console.warn per failed source per page lifetime, never a broken map.

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
  var warningsPollTimer = null;
  var hurricanePollTimer = null;
  var windPollTimer = null;
  var windMoveTimer = null;
  var animationTimer = null;

  // One console.warn per failed source per page lifetime (not per init):
  // a flaky upstream on a 2-minute refresh loop must not spam the console.
  var warnedSources = {};
  function warnOnce(key, msg, err) {
    if (warnedSources[key]) return;
    warnedSources[key] = true;
    console.warn(msg, err || '');
  }

  function teardownExisting() {
    if (animationTimer) { clearTimeout(animationTimer); animationTimer = null; }
    if (radarPollTimer) { clearInterval(radarPollTimer); radarPollTimer = null; }
    if (strikePollTimer) { clearInterval(strikePollTimer); strikePollTimer = null; }
    if (warningsPollTimer) { clearInterval(warningsPollTimer); warningsPollTimer = null; }
    if (hurricanePollTimer) { clearInterval(hurricanePollTimer); hurricanePollTimer = null; }
    if (windPollTimer) { clearInterval(windPollTimer); windPollTimer = null; }
    if (windMoveTimer) { clearTimeout(windMoveTimer); windMoveTimer = null; }
    if (currentMap) {
      try { currentMap.remove(); } catch (e) { /* swallow */ }
      currentMap = null;
    }
  }

  // Descriptor field access tolerant of either serde casing. The Rust
  // catalog serializes camelCase to match the recon catalog, but a
  // snake_case slip on the Rust side must not blank the whole menu.
  function dget(obj, camel, snake) {
    if (obj == null) return undefined;
    if (obj[camel] != null) return obj[camel];
    return obj[snake];
  }

  function normalizeProvider(raw) {
    return {
      id: raw.id,
      label: raw.label || raw.id,
      kind: raw.kind || 'wms',
      coverageLabel: dget(raw, 'coverageLabel', 'coverage_label') || '',
      url: raw.url || '',
      wmsLayer: dget(raw, 'wmsLayer', 'wms_layer') || '',
      attribution: raw.attribution || '',
      crossfade: raw.crossfade === true,
    };
  }

  function normalizeFeature(raw) {
    // The catalog serializes `endpoints` as an array (possibly empty),
    // in a feature-specific order documented per entry in
    // radar_catalog.rs. Tolerate a singular `endpoint` string from
    // older descriptors by promoting it to a one-element array.
    var endpoints = Array.isArray(raw.endpoints)
      ? raw.endpoints.filter(function (e) { return typeof e === 'string' && e.length > 0; })
      : (typeof raw.endpoint === 'string' && raw.endpoint.length > 0 ? [raw.endpoint] : []);
    return {
      id: raw.id,
      label: raw.label || raw.id,
      endpoints: endpoints,
      attribution: raw.attribution || '',
    };
  }

  function parseDescriptorAttr(attrValue, normalize) {
    if (attrValue == null || attrValue === '') return null;
    try {
      var parsed = JSON.parse(attrValue);
      if (!Array.isArray(parsed)) return null;
      return parsed
        .filter(function (d) { return d && typeof d === 'object' && typeof d.id === 'string'; })
        .map(normalize);
    } catch (e) {
      return null;
    }
  }

  // Non-SSR safety net mirroring what the Rust catalog resolves for the
  // stock config at the 40/-75 CONUS fallback coordinates (RainViewer
  // plus both US reflectivity sources) and the full feature catalog.
  // The Rust component renders identical attribute strings for the
  // default config, so this only fires when the attributes are missing
  // or unparseable.
  var FALLBACK_PROVIDERS = [
    {
      id: 'rainviewer',
      label: 'RainViewer (global composite)',
      kind: 'rainviewer',
      coverageLabel: 'Global',
      url: 'https://api.rainviewer.com/public/weather-maps.json',
      attribution: 'RainViewer.com',
      crossfade: false,
    },
    {
      id: 'nexrad_iem',
      label: 'IEM NEXRAD Base Reflectivity (CONUS)',
      kind: 'wms',
      coverageLabel: 'US (CONUS)',
      url: 'https://mesonet.agron.iastate.edu/cgi-bin/wms/nexrad/n0r-t.cgi',
      wmsLayer: 'nexrad-n0r-wmst',
      attribution: 'Iowa Environmental Mesonet / NWS NEXRAD',
      crossfade: true,
    },
    {
      id: 'nowcoast',
      label: 'NOAA nowCOAST Radar Mosaic (US incl. AK/HI/PR/Guam)',
      kind: 'wms',
      coverageLabel: 'US (CONUS, Alaska, Hawaii, Caribbean, Guam)',
      url: 'https://nowcoast.noaa.gov/geoserver/observations/weather_radar/wms',
      wmsLayer: 'base_reflectivity_mosaic',
      attribution: 'NOAA/NWS nowCOAST (MRMS)',
      crossfade: true,
    },
  ];
  var FALLBACK_FEATURES = [
    { id: 'nowcast', label: 'Forecast frames (RainViewer nowcast)', endpoints: [], attribution: 'RainViewer.com' },
    { id: 'warnings_us', label: 'NWS severe weather alerts (US)', endpoints: [], attribution: 'NOAA/NWS' },
    { id: 'hurricanes', label: 'Hurricanes (NOAA NHC)', endpoints: [], attribution: 'NOAA NHC/CPHC · JMA · BOM · JTWC' },
    { id: 'lightning_tempest', label: 'Lightning strikes', endpoints: [], attribution: 'Tempest (local station) · Blitzortung.org contributors (CC BY-SA 4.0)' },
    { id: 'wind', label: 'Wind flow (Open-Meteo)', endpoints: [], attribution: 'Open-Meteo' },
  ];

  // Pre-catalog stored prefs and default lists spoke short ids. Map them
  // onto catalog ids so a returning browser keeps its toggles. "satellite"
  // has no successor (RainViewer's IR arrays come back empty key-free) and
  // falls out as an unknown id like any other.
  var LEGACY_IDS = {
    precip: 'rainviewer',
    nexrad: 'nexrad_iem',
    lightning: 'lightning_tempest',
  };

  // Compact row labels for the Layers drawer, keyed by catalog id.
  // Anything not listed falls back to the descriptor label with the
  // parenthetical stripped, so a brand-new provider still gets a usable
  // row name (the full label lives in the row's legend expander).
  var SHORT_LABELS = {
    rainviewer: 'Precip',
    nexrad_iem: 'NEXRAD',
    nowcoast: 'NOAA Mosaic',
    geomet_ca: 'Radar CA',
    dwd_de: 'Radar DE',
    fmi_fi: 'Radar FI',
    nowcast: 'Forecast',
    warnings_us: 'Alerts',
    // hurricanes is intentionally unlisted: the descriptor label is
    // basin-localized ("Hurricanes (NOAA NHC)", "Typhoons (JMA / RSMC
    // Tokyo)", "Cyclones (BOM)"), so the parenthetical-stripping
    // fallback below yields the right local term for the row.
    lightning_tempest: 'Strikes',
    wind: 'Wind',
  };

  function shortLabelFor(id, label) {
    if (SHORT_LABELS[id]) return SHORT_LABELS[id];
    return String(label || id).replace(/\s*\(.*$/, '');
  }

  // NWS alert etiquette: api.weather.gov wants a meaningful User-Agent
  // identifying the app and a contact. Browsers that treat User-Agent as
  // a forbidden header silently drop the override and send their own UA,
  // which still satisfies the "never empty" requirement.
  var NWS_USER_AGENT = 'LocalSky/1.0 (self-hosted weather app; https://github.com/silenthooligan/localsky)';
  var NWS_ALERTS_URL =
    'https://api.weather.gov/alerts/active?status=actual&severity=Severe,Extreme';

  // LocalSky's own normalized tropical feed. The server fetches every
  // verified basin agency (NOAA NHC/CPHC, JMA RSMC Tokyo, BOM, JTWC
  // fallback basins), normalizes them into ONE GeoJSON FeatureCollection
  // (Point = storm position, LineString = track, Polygon = forecast cone,
  // with term/name/agency/intensity_kt properties on each feature) and
  // caches it ~10 minutes, windgrid-style. The browser never talks to
  // the agencies directly, so basin quirks (JMA's [lat, lon] coordinate
  // order, JTWC's ATCF fixed-width text) stay server-side.
  var TROPICAL_URL = '/api/v1/radar/tropical';

  // Our own wind grid endpoint. The server batches one Open-Meteo call
  // per cache window (~30 min, keyed on the rounded grid) and answers in
  // leaflet-velocity's grib2json-style two-record [U, V] format, so the
  // browser never talks to Open-Meteo directly. Fallback for descriptors
  // that arrive without endpoints, same pattern as the NWS constant.
  var WINDGRID_URL = '/api/v1/radar/windgrid';

  // NWS alert severity ramp. Fill + stroke share the color; opacity keeps
  // the radar readable underneath.
  var SEVERITY_COLORS = {
    Extreme: '#ff4d6d',
    Severe: '#ff9f1c',
    Moderate: '#ffd166',
    Minor: '#8ecae6',
    Unknown: '#9aa0a6',
  };

  function escHtml(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
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

    // Effective layer catalog, resolved server-side (config providers list
    // or recommended-by-region) and serialized onto the element. The
    // fallbacks mirror the stock config at the 40/-75 defaults above so a
    // non-SSR mount stays self-consistent.
    var providers =
      parseDescriptorAttr(el.dataset.radarProviders, normalizeProvider) ||
      FALLBACK_PROVIDERS.map(normalizeProvider);
    var features =
      parseDescriptorAttr(el.dataset.radarFeatures, normalizeFeature) ||
      FALLBACK_FEATURES.map(normalizeFeature);

    function featureById(id) {
      for (var i = 0; i < features.length; i++) {
        if (features[i].id === id) return features[i];
      }
      return null;
    }

    // Config-driven default layer ids (ui.radar.default_layers). The
    // fallback matches the stock config trio (the catalog successors of
    // the old hardcoded precip + NEXRAD + strikes behavior). Only
    // attribute ABSENCE falls back: a deliberately empty configured list
    // renders data-default-layers="" and means start with everything off.
    var defaultLayersAttr = el.dataset.defaultLayers;
    if (defaultLayersAttr == null) {
      defaultLayersAttr = 'rainviewer,nexrad_iem,lightning_tempest';
    }
    var defaultLayerIds = defaultLayersAttr
      .split(',')
      .map(function (s) { return s.trim(); })
      .filter(function (s) { return s.length > 0; })
      .map(function (id) { return LEGACY_IDS[id] || id; });

    // Mobile detection: the bottom-tab breakpoint matches the rest of the
    // app (760px). On a phone we move the zoom control to the bottom-right
    // (easier thumb reach) and turn off `tap` so single-tap-then-drag
    // doesn't get eaten by Leaflet's tap-handler quirk on iOS Safari. The
    // Layers drawer is the same at every width; only its geometry changes
    // (CSS turns it into a full-width sheet under 760px).
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
    // The attribution control aggregates the `attribution` option of every
    // layer currently ON the map, so the credit line tracks the active
    // descriptor set instead of hardcoding any one provider.
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

    currentMap = map;

    // 1x1 transparent PNG, base64. Served when a tile URL fails so
    // we silently degrade instead of showing the upstream error tile.
    var TRANSPARENT_TILE =
      'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=';

    // ---------- Providers: RainViewer animated frames ----------
    //
    // Exactly one rainviewer-kind provider gets the frame machinery (the
    // catalog only defines one; extras would double-animate, so they are
    // skipped). The nowcast FEATURE extends the frame array with the
    // API's forecast frames when toggled on.

    var rainviewerDesc = null;
    for (var pi = 0; pi < providers.length; pi++) {
      if (providers[pi].kind === 'rainviewer') { rainviewerDesc = providers[pi]; break; }
    }

    var radarLayer = rainviewerDesc ? L.layerGroup() : null;
    var radarTiles = {};
    var radarFrames = [];
    var radarPastCount = 0;
    var radarCurrent = 0;
    var radarPlaying = true;
    var lastRadarData = null;
    // Aliases the outer-scope animationTimer so teardownExisting()
    // can clear it on route swap. Same goes for the polling timers
    // assigned at the bottom of init.
    var radarTimer = null;

    // ---------- Providers: WMS overlays ----------
    //
    // Every wms-kind descriptor becomes a tile overlay, no region gating
    // here: the server already resolved the effective set, and a custom
    // config may deliberately enable an out-of-region provider for
    // comparison. Painting nothing over an uncovered area is the worst
    // case and that's fine.

    var wmsProviders = providers
      .filter(function (p) { return p.kind === 'wms' && p.url && p.wmsLayer; })
      .map(function (p) {
        var layer = L.tileLayer.wms(p.url, {
          layers: p.wmsLayer,
          format: 'image/png',
          transparent: true,
          opacity: 0.7,  // Standalone strength; crossfade-flagged sources are re-blended on zoom below.
          zIndex: 95,
          minZoom: 0,
          maxZoom: 19,
          // WMS reprojects to whatever bounding box you ask, so this
          // works at any zoom; past the source's native resolution it
          // just goes soft. The errorTileUrl covers upstream timeouts.
          errorTileUrl: TRANSPARENT_TILE,
          attribution: p.attribution,
        });
        return { desc: p, layer: layer };
      });

    // Zoom-driven crossfade, ONLY between the RainViewer layer and a
    // VISIBLE crossfade-flagged wms provider (the CONUS reflectivity
    // sources). RainViewer's animated tiles cap at z=7 native, so above
    // that they pixelate; the WMS source reprojects at any scale with
    // ~250m native detail. Linear blend between z=6 (RainViewer-only)
    // and z=9 (WMS-only) reads naturally as the user pulls in. With no
    // visible crossfade partner, RainViewer holds full strength at every
    // zoom (pixelated past z=7, but pixelated beats a blank map), and a
    // crossfade-flagged wms shown WITHOUT RainViewer paints at its
    // standalone opacity so it never fades to nothing at low zoom.
    function crossfadeActive() {
      if (!radarLayer || !map.hasLayer(radarLayer)) return false;
      return wmsProviders.some(function (p) {
        return p.desc.crossfade && map.hasLayer(p.layer);
      });
    }
    function rvOpacityForZoom(z) {
      if (!crossfadeActive()) return 0.65;
      var t = Math.max(0, Math.min(1, (z - 6) / 3));
      return 0.65 * (1 - t * 0.78);  // 0.65 at z=6 down to 0.14 at z=9
    }
    function wmsOpacityForZoom(desc, z) {
      if (desc.crossfade && radarLayer && map.hasLayer(radarLayer)) {
        var t = Math.max(0, Math.min(1, (z - 6) / 3));
        return 0.75 * t;             // 0.0 at z=6 up to 0.75 at z=9
      }
      return 0.7;
    }

    // RainViewer's public API caps tile generation at z=7 regardless
    // of tileSize (verified empirically: both 256 and 512 sizes
    // return a "Zoom Level Not Supported" placeholder PNG for z>=8).
    // We use the standard 256 size + maxNativeZoom:7 below so Leaflet
    // stretches the z=7 tile across higher zooms instead of fetching
    // unsupported levels. errorTileUrl handles the rare miss with a
    // transparent 1x1 PNG so users never see the "Not Supported"
    // placeholder text.
    function radarTileUrl(host, frame) {
      return host + frame.path + '/256/{z}/{x}/{y}/2/1_1.png';
    }

    function showRadarFrame(idx) {
      var visibleOp = rvOpacityForZoom(map.getZoom());
      Object.keys(radarTiles).forEach(function (k) {
        radarTiles[k].setOpacity(parseInt(k, 10) === idx ? visibleOp : 0);
      });
      var f = radarFrames[idx];
      if (f) {
        var d = new Date(f.time * 1000);
        var label = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        // Frames past the observed set are RainViewer nowcast: tag them
        // with lead time so forecast frames are never mistaken for
        // observations.
        var tag = '';
        if (idx === radarPastCount - 1) {
          tag = ' (now)';
        } else if (idx >= radarPastCount) {
          var mins = Math.max(1, Math.round((f.time - Date.now() / 1000) / 60));
          tag = ' (+' + mins + 'm forecast)';
        }
        var timeEl = document.getElementById('radar-time');
        if (timeEl) timeEl.textContent = label + tag;
      }
    }

    function radarTick() {
      if (!radarPlaying || radarFrames.length === 0) return;
      radarCurrent = (radarCurrent + 1) % radarFrames.length;
      showRadarFrame(radarCurrent);
      radarTimer = animationTimer = setTimeout(radarTick, 600);
    }

    function loadRainViewer() {
      return fetch(rainviewerDesc.url).then(function (r) { return r.json(); });
    }

    // (Re)build the animated frame set from the cached weather-maps.json
    // payload. The nowcast feature toggle decides whether forecast frames
    // join the loop; recon note: nowcast arrays have been observed empty
    // on the key-free tier, in which case the toggle is a benign no-op.
    function rebuildRadarFrames(data) {
      lastRadarData = data;
      var host = data.host;
      var past = (data.radar && data.radar.past) || [];
      var nowcast = (data.radar && data.radar.nowcast) || [];
      var includeNowcast = nowcastLayer != null && map.hasLayer(nowcastLayer);
      radarPastCount = past.length;
      radarFrames = includeNowcast ? past.concat(nowcast) : past.slice();

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
          // errors). The crossfade WMS source takes over for detail.
          maxNativeZoom: 7,
          errorTileUrl: TRANSPARENT_TILE,
          attribution: rainviewerDesc.attribution,
        });
        radarTiles[i] = t;
        radarLayer.addLayer(t);
      });
      radarCurrent = Math.max(0, radarPastCount - 1);
      showRadarFrame(radarCurrent);
      if (radarTimer) clearTimeout(radarTimer);
      if (radarPlaying) radarTimer = animationTimer = setTimeout(radarTick, 1500);
    }

    // ---------- Feature: RainViewer nowcast (forecast frames) ----------
    //
    // A "virtual" layer: an empty group whose on-map presence flags
    // whether forecast frames are spliced into the animation. Riding the
    // normal overlay machinery keeps the toggle, chips, and persistence
    // uniform with real layers.

    var nowcastDesc = rainviewerDesc ? featureById('nowcast') : null;
    var nowcastLayer = nowcastDesc ? L.layerGroup() : null;

    // ---------- Feature: NWS active alerts ----------
    //
    // Severity-filtered alert polygons, refreshed every 2 minutes while
    // visible. Many NWS alerts are zone-coded with null geometry; only
    // polygon-bearing features are drawn. Popup carries event + headline.

    var warningsDesc = featureById('warnings_us');
    var warningsGroup = warningsDesc ? L.layerGroup() : null;

    function alertStyle(feature) {
      var sev = (feature.properties && feature.properties.severity) || 'Unknown';
      var color = SEVERITY_COLORS[sev] || SEVERITY_COLORS.Unknown;
      return { color: color, weight: 1.4, fillColor: color, fillOpacity: 0.18 };
    }

    function refreshWarnings() {
      if (!warningsGroup || !map.hasLayer(warningsGroup)) return;
      fetch(warningsDesc.endpoints[0] || NWS_ALERTS_URL, {
        headers: {
          Accept: 'application/geo+json',
          'User-Agent': NWS_USER_AGENT,
        },
      })
        .then(function (r) { return r.json(); })
        .then(function (geo) {
          if (!geo || !Array.isArray(geo.features)) return;
          warningsGroup.clearLayers();
          L.geoJSON(geo, {
            filter: function (f) { return !!f.geometry; },
            style: alertStyle,
            attribution: warningsDesc.attribution || 'NOAA/NWS',
            onEachFeature: function (f, layer) {
              var p = f.properties || {};
              layer.bindPopup(
                '<strong>' + escHtml(p.event || 'Alert') + '</strong><br>' +
                escHtml(p.headline || '')
              );
            },
          }).addTo(warningsGroup);
        })
        .catch(function (e) { warnOnce('nws-alerts', 'NWS alerts fetch failed', e); });
    }

    // ---------- Feature: tropical cyclones (all basins) ----------
    //
    // ONE fetch per refresh against LocalSky's own normalized endpoint
    // (TROPICAL_URL above); the same code path serves every basin. The
    // descriptor's endpoints list documents the upstream agency feeds
    // (home basin first) and is deliberately NOT fetched from here: the
    // raw feeds are not browser-friendly, so normalization lives on the
    // server and this layer stays dumb. Geometry type decides rendering:
    //
    //   Polygon / MultiPolygon        forecast cone (translucent fill;
    //                                 only where the agency provides one)
    //   LineString / MultiLineString  track polyline
    //   Point                         storm position marker, tooltip
    //                                 built from the term-aware
    //                                 properties the normalizer attaches
    //                                 (term/name/agency/intensity_kt),
    //                                 e.g. "Typhoon NANMADOL (JMA)"
    //
    // Cones draw first, then tracks, then markers, so positions stay on
    // top. A quiet planet (zero features) renders nothing at all,
    // silently; a failed fetch warns once and keeps whatever is already
    // drawn. 15-minute cadence: advisories move slowly.

    var hurricanesDesc = featureById('hurricanes');
    var hurricanesGroup = hurricanesDesc ? L.layerGroup() : null;

    // "Typhoon NANMADOL (JMA) · 85 kt", degrading gracefully when the
    // normalizer had no term/agency/intensity_kt for a storm. The
    // server sends terms lowercase ("typhoon"); capitalize for display.
    // escHtml because bindTooltip interprets its string as HTML.
    function stormTooltip(p) {
      p = p || {};
      var term = p.term ? p.term.charAt(0).toUpperCase() + p.term.slice(1) + ' ' : '';
      var label = term + (p.name || 'Storm');
      if (p.agency) label += ' (' + p.agency + ')';
      var kt = Number(p.intensity_kt);
      if (isFinite(kt) && kt > 0) label += ' · ' + Math.round(kt) + ' kt';
      return escHtml(label);
    }

    function refreshHurricanes() {
      if (!hurricanesGroup || !map.hasLayer(hurricanesGroup)) return;
      // endpoints[0] is the normalizer per the descriptor contract; the
      // constant covers descriptors that arrive without endpoints
      // (windgrid precedent).
      var url = (hurricanesDesc.endpoints && hurricanesDesc.endpoints[0]) || TROPICAL_URL;
      fetch(url)
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        })
        .then(function (geo) {
          if (tearingDown) return;
          if (!geo || !Array.isArray(geo.features)) return;
          // An all-agencies-down sweep is served (never cached) as an
          // empty collection whose sources are all ok:false; that is a
          // dead feed, not a quiet globe, so keep whatever is already
          // drawn instead of clearing good storms.
          var srcs = Array.isArray(geo.sources) ? geo.sources : [];
          if (srcs.length && !srcs.some(function (s) { return s && s.ok; })) {
            warnOnce('tropical', 'Tropical cyclone feed: all upstream agencies down');
            return;
          }
          hurricanesGroup.clearLayers();
          var attribution = hurricanesDesc.attribution || 'NOAA NHC/CPHC · JMA · BOM · JTWC';
          var cones = [];
          var tracks = [];
          var points = [];
          geo.features.forEach(function (f) {
            var t = (f && f.geometry && f.geometry.type) || '';
            if (t === 'Polygon' || t === 'MultiPolygon') cones.push(f);
            else if (t === 'LineString' || t === 'MultiLineString') tracks.push(f);
            else if (t === 'Point' || t === 'MultiPoint') points.push(f);
          });
          if (cones.length) {
            L.geoJSON({ type: 'FeatureCollection', features: cones }, {
              style: { color: '#ff6b81', weight: 1, fillColor: '#ff6b81', fillOpacity: 0.12 },
              attribution: attribution,
            }).addTo(hurricanesGroup);
          }
          if (tracks.length) {
            L.geoJSON({ type: 'FeatureCollection', features: tracks }, {
              style: { color: '#ff6b81', weight: 2 },
              attribution: attribution,
            }).addTo(hurricanesGroup);
          }
          if (points.length) {
            L.geoJSON({ type: 'FeatureCollection', features: points }, {
              pointToLayer: function (f, latlng) {
                return L.circleMarker(latlng, {
                  radius: 6,
                  color: '#ff4757',
                  fillColor: '#ff4757',
                  fillOpacity: 0.9,
                  weight: 2,
                  attribution: attribution,
                }).bindTooltip(stormTooltip(f.properties), { direction: 'top' });
              },
              attribution: attribution,
            }).addTo(hurricanesGroup);
          }
        })
        .catch(function (e) { warnOnce('tropical', 'Tropical cyclone feed fetch failed', e); });
    }

    // ---------- Feature: lightning strikes ----------
    //
    // Two render modes, decided per strike by what the snapshot reports:
    //
    //   ring  Tempest reports distance-to-strike but not bearing, so the
    //         strike draws as a pulsing ring at the reported radius
    //         around the station.
    //   dot   Blitzortung community strikes (opt-in server source) carry
    //         real lat/lon, so they draw as small positioned markers
    //         that fade as they age toward the 1-hour prune line.
    //
    // Both ride the same snapshot array (lightning_recent); the server
    // tags each strike with `source` ("tempest" / "blitzortung").
    // Strikes missing the tag (older server) are inferred from shape:
    // lat/lon present means blitzortung. Per-marker Leaflet attribution
    // makes the credit line track what is actually on screen; the
    // CC BY-SA credit is mandatory under Blitzortung's terms whenever
    // their strikes are shown.

    var lightningDesc = featureById('lightning_tempest');
    var strikeLayer = lightningDesc ? L.layerGroup() : null;
    // key -> { layer, point } for every strike currently drawn, so dots
    // can be re-faded each poll and aged-out strikes removed one by one
    // instead of nuking the whole group.
    var strikeEntries = {};
    // Fingerprint of the last contributing-source set ('t', 'b', 'tb',
    // or ''), so the legend only rewrites when the networks change.
    var strikeSourceKey = null;

    var TEMPEST_STRIKE_ATTR = 'Lightning: Tempest (local station)';
    var BLITZ_STRIKE_ATTR =
      'Lightning data: <a href="https://www.blitzortung.org/" target="_blank" rel="noopener">Blitzortung.org</a> contributors, CC BY-SA 4.0';

    function strikeRadiusMeters(distanceMi) {
      return Math.max(distanceMi, 0.1) * 1609.34;
    }

    // Subtle age fade for positioned dots: near full strength when
    // fresh, dim but still readable at the 1-hour prune line.
    function dotOpacityForAge(ageMin) {
      var t = Math.max(0, Math.min(1, ageMin / 60));
      return 0.9 - t * 0.65;
    }

    function strikeSource(s, hasPos) {
      if (s.source === 'blitzortung' || s.source === 'tempest') return s.source;
      return hasPos ? 'blitzortung' : 'tempest';
    }

    // Source-aware legend copy. The drawer row's name stays the
    // catalog's generic short label; the row's legend expander is ours
    // and tracks the networks that actually contributed.
    function lightningLegendLabel(sources) {
      if (sources.tempest && sources.blitzortung) {
        return 'Lightning (Tempest station + Blitzortung community network)';
      }
      if (sources.blitzortung) return 'Lightning (Blitzortung community network)';
      if (sources.tempest) return 'Lightning (Tempest station)';
      return lightningDesc ? lightningDesc.label : 'Lightning strikes';
    }

    function lightningLegendText(sources) {
      var ringText = 'Yellow ring = Tempest strike at the reported ' +
        'distance (Tempest gives no bearing).';
      var dotText = 'Yellow dot = Blitzortung community strike at its ' +
        'true position, fading with age. Data: Blitzortung.org ' +
        'contributors, CC BY-SA 4.0.';
      if (sources.tempest && sources.blitzortung) {
        return ringText + ' ' + dotText;
      }
      if (sources.blitzortung) return dotText;
      if (sources.tempest) return ringText;
      // No strikes buffered yet: describe both possible renderings.
      return 'Tempest strikes draw as distance rings around the station ' +
        '(distance, no bearing); Blitzortung community strikes, when ' +
        'that source is enabled, draw as positioned dots that fade ' +
        'with age.';
    }

    // Rewrite the lightning legend block in place when the contributing
    // networks change (legend expanders carry a data-legend-id; see the
    // Layers drawer below). The block is in the DOM whether collapsed
    // or expanded, so the rewrite always lands; missing DOM is a clean
    // no-op. textContent keeps this injection-safe without escaping.
    function updateLightningLegend(sources) {
      var row = document.querySelector(
        '.radar-drawer-legend[data-legend-id="lightning_tempest"]'
      );
      if (!row) return;
      var strongEl = row.querySelector('strong');
      var spanEl = row.querySelector('span');
      if (strongEl) strongEl.textContent = lightningLegendLabel(sources);
      if (spanEl) spanEl.textContent = lightningLegendText(sources);
    }

    function addStrikeRing(s) {
      var miles = s.distance_km * 0.621371;
      var ring = L.circle([lat, lon], {
        radius: strikeRadiusMeters(miles),
        color: '#ffe066',
        weight: 1.4,
        fill: false,
        opacity: 0.0,
        attribution: TEMPEST_STRIKE_ATTR,
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
        miles.toFixed(1) + ' mi · ' +
          new Date(s.time_epoch * 1000).toLocaleTimeString() +
          ' · Tempest station',
        { sticky: true }
      );
      return ring;
    }

    function addStrikeDot(s, ageMin) {
      var settled = dotOpacityForAge(ageMin);
      var dot = L.circleMarker([s.lat, s.lon], {
        radius: 4,
        color: '#ffe066',
        weight: 1,
        fillColor: '#ffe066',
        opacity: settled,
        fillOpacity: settled,
        attribution: BLITZ_STRIKE_ATTR,
      }).addTo(strikeLayer);
      dot.bindTooltip(
        new Date(s.time_epoch * 1000).toLocaleTimeString() +
          ' · Blitzortung community',
        { sticky: true }
      );
      // Flash-in only for strikes that are actually fresh: the first
      // poll can hydrate hundreds of buffered strikes at once, and
      // pulsing them all would just spawn a pile of pointless timers.
      if (ageMin < 2) {
        var t0 = Date.now();
        var pulse = setInterval(function () {
          var p = (Date.now() - t0) / 1200;
          if (p >= 1) {
            dot.setRadius(4);
            dot.setStyle({ opacity: settled, fillOpacity: settled });
            clearInterval(pulse);
          } else {
            var o = 1 - p * (1 - settled);
            dot.setRadius(4 + 3 * (1 - p));
            dot.setStyle({ opacity: o, fillOpacity: o });
          }
        }, 60);
      }
      return dot;
    }

    function refreshStrikes() {
      if (!strikeLayer) return;
      fetch('/api/snapshot')
        .then(function (r) { return r.json(); })
        .then(function (snap) {
          var strikes = snap.lightning_recent || [];
          var seen = new Set();
          var sources = { tempest: false, blitzortung: false };
          var nowSec = Date.now() / 1000;
          strikes.forEach(function (s) {
            var hasPos = isFinite(s.lat) && isFinite(s.lon);
            var source = strikeSource(s, hasPos);
            sources[source] = true;
            var key = source + '_' + s.time_epoch + '_' +
              (hasPos ? s.lat + '_' + s.lon : s.distance_km);
            seen.add(key);
            var ageMin = Math.max(0, (nowSec - s.time_epoch) / 60);
            var entry = strikeEntries[key];
            if (entry) {
              // Re-fade existing dots on every poll so the age fade
              // stays honest between strikes; rings keep their settled
              // opacity.
              if (entry.point) {
                var o = dotOpacityForAge(ageMin);
                entry.layer.setStyle({ opacity: o, fillOpacity: o });
              }
              return;
            }
            strikeEntries[key] = (hasPos && source === 'blitzortung')
              ? { layer: addStrikeDot(s, ageMin), point: true }
              : { layer: addStrikeRing(s), point: false };
          });
          // Drop markers for strikes that aged out of the snapshot
          // buffer (server prunes at 1 hour).
          Object.keys(strikeEntries).forEach(function (k) {
            if (!seen.has(k)) {
              strikeLayer.removeLayer(strikeEntries[k].layer);
              delete strikeEntries[k];
            }
          });
          var srcKey = (sources.tempest ? 't' : '') +
            (sources.blitzortung ? 'b' : '');
          if (srcKey !== strikeSourceKey) {
            strikeSourceKey = srcKey;
            updateLightningLegend(sources);
          }
        })
        .catch(function () {});
    }

    // ---------- Feature: Open-Meteo wind flow ----------
    //
    // Animated particle wind via leaflet-velocity (vendored at
    // /vendor/leaflet-velocity.min.js, loaded after Leaflet in the SSR
    // shell). The toggle handle is an empty group (the nowcast precedent)
    // so the menu/chips/persistence machinery stays uniform; the real
    // L.velocityLayer is built lazily inside it on the first enable, so
    // no windgrid request leaves the browser while the layer is off. The
    // grid tracks the viewport: debounced moveend refetch while visible,
    // plus a 15-minute timer so the current-hour sample doesn't go stale
    // on an idle map. If the vendored script failed to load (no
    // L.velocityLayer), the feature is skipped entirely: no menu entry,
    // no errors, the rest of the map is untouched.

    var windDesc = featureById('wind');
    var windGroup = (windDesc && typeof L.velocityLayer === 'function')
      ? L.layerGroup()
      : null;
    var windVelocity = null;  // the real L.velocityLayer, built on first data
    var windFetchSeq = 0;     // drops out-of-order responses after a pan

    function windGridUrl() {
      // Round to 2 decimals so small pans land on the same server cache
      // entry, and clamp to plausible degrees (worldCopyJump can hand
      // back longitudes past the antimeridian; the server 400s those).
      var b = map.getBounds();
      var minLat = Math.max(-85, Math.min(85, b.getSouth()));
      var maxLat = Math.max(-85, Math.min(85, b.getNorth()));
      var minLon = Math.max(-180, Math.min(180, b.getWest()));
      var maxLon = Math.max(-180, Math.min(180, b.getEast()));
      // Degenerate after clamping (e.g. a fully wrapped view): skip the
      // round trip rather than burn the warnOnce on a known 400.
      if (!(maxLat > minLat && maxLon > minLon)) return null;
      var base = (windDesc.endpoints && windDesc.endpoints[0]) || WINDGRID_URL;
      return base + '?bbox=' +
        [minLon, minLat, maxLon, maxLat]
          .map(function (v) { return v.toFixed(2); })
          .join(',');
    }

    function refreshWind() {
      if (!windGroup || !map.hasLayer(windGroup)) return;
      var url = windGridUrl();
      if (!url) return;
      var seq = ++windFetchSeq;
      fetch(url)
        .then(function (r) {
          if (!r.ok) throw new Error('HTTP ' + r.status);
          return r.json();
        })
        .then(function (data) {
          // A newer fetch superseded this one, or the map is mid
          // route-swap teardown: either way, don't touch layers.
          if (seq !== windFetchSeq || tearingDown) return;
          if (!Array.isArray(data) || data.length < 2 || !data[0] || !data[0].header) return;
          if (windVelocity) {
            windVelocity.setData(data);
            return;
          }
          windVelocity = L.velocityLayer({
            data: data,
            // velocityScale 0.01 is 2x the library default (0.005, which
            // was tuned for whole-globe demos and reads near-static at
            // the regional zooms this map lives at). maxVelocity 15 m/s
            // saturates the color ramp around strong-breeze rather than
            // hurricane scale, so ordinary days still show contrast.
            velocityScale: 0.01,
            minVelocity: 0,
            maxVelocity: 15,
            // No mouseover speed readout: its control defaults to the
            // bottom-left corner the legend + attribution already use.
            displayValues: false,
            attribution: windDesc.attribution || 'Open-Meteo',
          });
          windGroup.addLayer(windVelocity);
        })
        .catch(function (e) { warnOnce('windgrid', 'Wind grid fetch failed', e); });
    }

    if (windGroup) {
      // Viewport changed: refetch for the new bounds, debounced 2s so a
      // pan gesture's intermediate moveends collapse into one request.
      // refreshWind no-ops while the layer is hidden, so a stray timer
      // after toggling off costs nothing and needs no cancellation here.
      map.on('moveend', function () {
        if (!map.hasLayer(windGroup)) return;
        if (windMoveTimer) clearTimeout(windMoveTimer);
        windMoveTimer = setTimeout(refreshWind, 2000);
      });
    }

    // ---------- Layer toggles: the Layers drawer ----------
    //
    // The overlay list is generated from the descriptor arrays: providers
    // first (catalog order, drawer group "imagery"), then features
    // (group "overlays"). The drawer is built from this one list, and
    // persistence speaks descriptor ids so display labels can be
    // reworded freely without invalidating stored prefs.

    var overlayDefs = [];
    if (radarLayer) {
      overlayDefs.push({ id: rainviewerDesc.id, label: rainviewerDesc.label, layer: radarLayer, desc: rainviewerDesc, group: 'imagery' });
    }
    wmsProviders.forEach(function (p) {
      overlayDefs.push({ id: p.desc.id, label: p.desc.label, layer: p.layer, desc: p.desc, group: 'imagery' });
    });
    if (nowcastLayer) {
      overlayDefs.push({ id: nowcastDesc.id, label: nowcastDesc.label, layer: nowcastLayer, desc: nowcastDesc, group: 'overlays' });
    }
    if (warningsGroup) {
      overlayDefs.push({ id: warningsDesc.id, label: warningsDesc.label, layer: warningsGroup, desc: warningsDesc, group: 'overlays' });
    }
    if (hurricanesGroup) {
      overlayDefs.push({ id: hurricanesDesc.id, label: hurricanesDesc.label, layer: hurricanesGroup, desc: hurricanesDesc, group: 'overlays' });
    }
    if (strikeLayer) {
      overlayDefs.push({ id: lightningDesc.id, label: lightningDesc.label, layer: strikeLayer, desc: lightningDesc, group: 'overlays' });
    }
    if (windGroup) {
      overlayDefs.push({ id: windDesc.id, label: windDesc.label, layer: windGroup, desc: windDesc, group: 'overlays' });
    }

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
          // Migrate pre-catalog short ids in place; the legacy keys are
          // dropped on the next save.
          Object.keys(LEGACY_IDS).forEach(function (oldId) {
            var newId = LEGACY_IDS[oldId];
            if (typeof parsed[oldId] === 'boolean' && typeof parsed[newId] !== 'boolean') {
              parsed[newId] = parsed[oldId];
            }
          });
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
      overlayDefs.forEach(function (d) {
        prefs[d.id] = map.hasLayer(d.layer);
      });
      // Carry through stored ids that aren't in the current effective set
      // (e.g. a region-gated provider after a location change) so the pref
      // survives a round trip. Migrated legacy keys are intentionally
      // dropped; anything else unknown rides along untouched.
      if (storedPrefs) {
        Object.keys(storedPrefs).forEach(function (id) {
          if (!(id in prefs) && !(id in LEGACY_IDS) && typeof storedPrefs[id] === 'boolean') {
            prefs[id] = storedPrefs[id];
          }
        });
      }
      try {
        window.localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
      } catch (e) { /* no storage: toggles still work, they just won't stick */ }
    }

    // ---------- Layers drawer ----------
    //
    // ONE glassy "Layers" chip (top-right over the map, with an active-
    // count badge) replaces the old desktop L.control.layers AND the
    // mobile chip row, and the legend rail's per-layer copy moves into
    // an inline expander on each drawer row. Chip and drawer anchor to
    // .radar-map-shell (the positioned wrapper around #radar-map), NOT
    // the map container: the map div carries role="img", whose
    // descendants are presentational to assistive tech, and the drawer
    // is a real dialog. Living outside the Leaflet container also means
    // drawer interactions never reach the map's own handlers, so no
    // DomEvent propagation plumbing is needed.
    //
    // The pills drive the exact addLayer/removeLayer paths the old
    // surfaces used, so the toggle handler below (persistence, lazy
    // fetches, crossfade) and its teardown latch keep working
    // untouched. Open/closed state is deliberately NOT persisted (the
    // drawer always starts closed), and expanded legends reset when
    // the drawer closes.

    // Swatch colors are inline so new catalog entries need no CSS edits.
    var LEGEND_SWATCH = {
      rainviewer: '#58a6ff',
      nowcast: '#58e0ff',
      warnings_us: '#ff9f1c',
      hurricanes: '#ff4757',
      lightning_tempest: '#ffe066',
      wind: '#9d8cff',
    };
    function legendText(d) {
      if (d.id === 'nowcast') {
        return 'Extends the precipitation animation with RainViewer forecast frames, tagged "+Nm forecast" in the time readout.';
      }
      if (d.id === 'warnings_us') {
        return 'NWS active alert polygons. Fill color is severity: red extreme, orange severe. Tap a polygon for the headline. Refreshes every 2 min.';
      }
      if (d.id === 'hurricanes') {
        return 'Active tropical cyclones worldwide, normalized server-side from the responsible agencies (NOAA NHC/CPHC, JMA, BOM, JTWC): position markers, track lines, and forecast cones where the agency provides them. Empty when the basins are quiet.';
      }
      if (d.id === 'lightning_tempest') {
        // Initial render is the no-strikes-yet copy; refreshStrikes
        // rewrites the expander in place once the snapshot says which
        // networks are contributing.
        return lightningLegendText({ tempest: false, blitzortung: false });
      }
      if (d.id === 'wind') {
        return 'Animated particle flow of current 10 m winds, sampled on a grid over the visible map via Open-Meteo. Warmer particle colors = stronger wind. Refetches as you pan and every 15 min.';
      }
      if (d.desc && d.desc.kind === 'rainviewer') {
        return 'Animated recent frames. dBZ scale: blue light · green moderate · yellow to orange to red heavy.';
      }
      if (d.desc && d.desc.kind === 'wms') {
        // No attribution sentence here: every expander gets a dedicated
        // source line from the descriptor in buildDrawerRow below.
        var text = 'Reflectivity mosaic via WMS, coverage: ' + (d.desc.coverageLabel || 'regional') + '.';
        if (d.desc.crossfade) {
          text += ' Crossfades in past z=7 so detail stays sharp at street scale.';
        }
        return text;
      }
      return '';
    }
    function legendSwatchColor(d) {
      if (LEGEND_SWATCH[d.id]) return LEGEND_SWATCH[d.id];
      if (d.desc && d.desc.kind === 'wms') return '#7ed957';
      return '#9aa0a6';
    }

    var shell = el.closest('.radar-map-shell') || el;

    var drawer = document.createElement('div');
    drawer.id = 'radar-layers-drawer';
    drawer.className = 'radar-drawer';
    drawer.setAttribute('role', 'dialog');
    drawer.setAttribute('aria-label', 'Radar layers');
    drawer.setAttribute('aria-hidden', 'true');

    var drawerHead = document.createElement('div');
    drawerHead.className = 'radar-drawer-head';
    var drawerTitle = document.createElement('span');
    drawerTitle.className = 'radar-drawer-title';
    drawerTitle.textContent = 'Layers';
    var closeBtn = document.createElement('button');
    closeBtn.type = 'button';
    closeBtn.className = 'radar-drawer-close';
    closeBtn.setAttribute('aria-label', 'Close layers');
    closeBtn.textContent = '×';
    drawerHead.appendChild(drawerTitle);
    drawerHead.appendChild(closeBtn);
    drawer.appendChild(drawerHead);

    var drawerBody = document.createElement('div');
    drawerBody.className = 'radar-drawer-body';
    drawer.appendChild(drawerBody);

    // Footer: a standing link to Settings > Radar so operators jump
    // straight to the provider/default-layer catalog from the picker
    // instead of hunting through Settings. A real anchor to a same-
    // origin SSR route: leptos_router intercepts it for client-side nav
    // when mounted, and it still resolves as a normal navigation
    // otherwise, so both paths are correct. Pinned below the scrolling
    // body by the drawer's flex column.
    var drawerFoot = document.createElement('div');
    drawerFoot.className = 'radar-drawer-foot';
    var settingsLink = document.createElement('a');
    settingsLink.className = 'radar-drawer-settings';
    settingsLink.href = '/settings/radar';
    settingsLink.innerHTML =
      '<span class="radar-drawer-settings-icon" aria-hidden="true">' +
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
      'stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">' +
      '<line x1="21" x2="14" y1="4" y2="4"></line>' +
      '<line x1="10" x2="3" y1="4" y2="4"></line>' +
      '<line x1="21" x2="12" y1="12" y2="12"></line>' +
      '<line x1="8" x2="3" y1="12" y2="12"></line>' +
      '<line x1="21" x2="16" y1="20" y2="20"></line>' +
      '<line x1="12" x2="3" y1="20" y2="20"></line>' +
      '<line x1="14" x2="14" y1="2" y2="6"></line>' +
      '<line x1="8" x2="8" y1="10" y2="14"></line>' +
      '<line x1="16" x2="16" y1="18" y2="22"></line></svg></span>' +
      '<span class="radar-drawer-settings-text">' +
      '<span class="radar-drawer-settings-label">Radar settings</span>' +
      '<span class="radar-drawer-settings-sub">Add providers, set default layers</span>' +
      '</span>' +
      '<span class="radar-drawer-settings-go" aria-hidden="true">' +
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
      'stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
      '<polyline points="9 18 15 12 9 6"></polyline></svg></span>';
    drawerFoot.appendChild(settingsLink);
    drawer.appendChild(drawerFoot);

    var rowSyncs = [];          // per-row pill repaints, run by syncLayersUi
    var legendCollapsers = [];  // per-row expander resets, run on close

    function buildDrawerRow(d) {
      var row = document.createElement('div');
      row.className = 'radar-drawer-row';

      var main = document.createElement('div');
      main.className = 'radar-drawer-row-main';

      var swatch = document.createElement('span');
      swatch.className = 'radar-drawer-swatch';
      swatch.style.background = legendSwatchColor(d);

      // Short name on the row (chip-label heritage); the expander's
      // <strong> carries the full descriptor label.
      var label = document.createElement('span');
      label.className = 'radar-drawer-label';
      label.id = 'radar-drawer-label-' + d.id;
      label.textContent = shortLabelFor(d.id, d.label);

      var legend = document.createElement('div');
      legend.className = 'radar-drawer-legend';
      legend.id = 'radar-drawer-legend-' + d.id;
      // data-legend-id lets dynamic features (lightning's source-aware
      // copy) find and rewrite their own expander.
      legend.setAttribute('data-legend-id', d.id);
      legend.hidden = true;
      var legendStrong = document.createElement('strong');
      legendStrong.textContent = d.label;
      var legendSpan = document.createElement('span');
      legendSpan.textContent = legendText(d);
      legend.appendChild(legendStrong);
      legend.appendChild(legendSpan);
      if (d.desc && d.desc.attribution) {
        var attr = document.createElement('span');
        attr.className = 'radar-drawer-attr';
        attr.textContent = 'Source: ' + d.desc.attribution;
        legend.appendChild(attr);
      }

      var info = document.createElement('button');
      info.type = 'button';
      info.className = 'radar-drawer-info';
      info.setAttribute('aria-expanded', 'false');
      info.setAttribute('aria-controls', legend.id);
      info.setAttribute('aria-label', 'About ' + d.label);
      info.textContent = 'i';

      function setLegendExpanded(open) {
        legend.hidden = !open;
        info.setAttribute('aria-expanded', open ? 'true' : 'false');
      }
      info.addEventListener('click', function () {
        setLegendExpanded(legend.hidden);
      });
      legendCollapsers.push(function () { setLegendExpanded(false); });

      // House toggle idiom: the segmented On|Off pill from main.scss,
      // same markup the settings pages render.
      var pill = document.createElement('button');
      pill.type = 'button';
      pill.className = 'toggle-pill';
      pill.setAttribute('role', 'switch');
      pill.setAttribute('aria-checked', 'false');
      pill.setAttribute('aria-labelledby', label.id);
      var optOn = document.createElement('span');
      optOn.className = 'toggle-pill__opt toggle-pill__opt--on';
      optOn.textContent = 'On';
      var optOff = document.createElement('span');
      optOff.className = 'toggle-pill__opt toggle-pill__opt--off';
      optOff.textContent = 'Off';
      pill.appendChild(optOn);
      pill.appendChild(optOff);
      pill.addEventListener('click', function () {
        // Same add/remove calls the old surfaces made; the map-level
        // toggle handler below does persistence, lazy fetches, and
        // crossfade, and syncLayersUi repaints this pill.
        if (map.hasLayer(d.layer)) {
          map.removeLayer(d.layer);
        } else {
          d.layer.addTo(map);
        }
      });

      rowSyncs.push(function () {
        var on = map.hasLayer(d.layer);
        pill.setAttribute('aria-checked', on ? 'true' : 'false');
        optOn.classList.toggle('is-active', on);
        optOff.classList.toggle('is-active', !on);
      });

      main.appendChild(swatch);
      main.appendChild(label);
      main.appendChild(info);
      main.appendChild(pill);
      row.appendChild(main);
      row.appendChild(legend);
      return row;
    }

    [
      { key: 'imagery', title: 'Imagery' },
      { key: 'overlays', title: 'Overlays' },
    ].forEach(function (g) {
      var defs = overlayDefs.filter(function (d) { return d.group === g.key; });
      if (defs.length === 0) return;
      var section = document.createElement('div');
      section.className = 'radar-drawer-group';
      var sectionTitle = document.createElement('h3');
      sectionTitle.className = 'radar-drawer-group-title';
      sectionTitle.textContent = g.title;
      section.appendChild(sectionTitle);
      defs.forEach(function (d) { section.appendChild(buildDrawerRow(d)); });
      drawerBody.appendChild(section);
    });

    // The chip. aria-label (set in syncLayersUi) carries the count for
    // screen readers; the visual badge is hidden from them so the name
    // isn't read twice.
    var btnWrap = document.createElement('div');
    btnWrap.className = 'radar-layers-anchor';
    var layersBtn = document.createElement('button');
    layersBtn.type = 'button';
    layersBtn.className = 'radar-layers-btn';
    layersBtn.setAttribute('aria-expanded', 'false');
    layersBtn.setAttribute('aria-controls', drawer.id);
    layersBtn.setAttribute('aria-haspopup', 'dialog');
    // Stacked-sheets glyph (Lucide "layers"): the chip reads as a layer
    // picker at a glance, not just a word. Presentational, so hidden
    // from assistive tech (the button's aria-label already names it).
    var layersBtnIcon = document.createElement('span');
    layersBtnIcon.className = 'radar-layers-icon';
    layersBtnIcon.setAttribute('aria-hidden', 'true');
    layersBtnIcon.innerHTML =
      '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
      'stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">' +
      '<polygon points="12 2 2 7 12 12 22 7 12 2"></polygon>' +
      '<polyline points="2 17 12 22 22 17"></polyline>' +
      '<polyline points="2 12 12 17 22 12"></polyline></svg>';
    var layersBtnText = document.createElement('span');
    layersBtnText.textContent = 'Layers';
    var layersBtnCount = document.createElement('span');
    layersBtnCount.className = 'radar-layers-count';
    layersBtnCount.setAttribute('aria-hidden', 'true');
    layersBtn.appendChild(layersBtnIcon);
    layersBtn.appendChild(layersBtnText);
    layersBtn.appendChild(layersBtnCount);
    btnWrap.appendChild(layersBtn);

    var drawerOpen = false;
    function setDrawerOpen(open) {
      if (open === drawerOpen) return;
      drawerOpen = open;
      layersBtn.setAttribute('aria-expanded', open ? 'true' : 'false');
      if (open) {
        drawer.classList.add('is-open');
        drawer.setAttribute('aria-hidden', 'false');
        // Focus moves into the dialog: the close affordance is its
        // first control in tab order.
        closeBtn.focus();
      } else {
        // Hand focus back to the chip BEFORE aria-hidden flips so the
        // hidden subtree never holds focus. Only when focus was inside
        // the drawer: a click-outside already moved it where the user
        // aimed, and yanking it back would be hostile.
        if (drawer.contains(document.activeElement)) layersBtn.focus();
        drawer.classList.remove('is-open');
        drawer.setAttribute('aria-hidden', 'true');
        legendCollapsers.forEach(function (f) { f(); });
      }
    }

    layersBtn.addEventListener('click', function () { setDrawerOpen(!drawerOpen); });
    closeBtn.addEventListener('click', function () { setDrawerOpen(false); });
    // Esc + click-outside close, both document-level. Esc cannot live
    // on the drawer alone: Firefox and Safari don't move focus to a
    // button on click, so a pill tap can leave focus on <body> and a
    // drawer-scoped keydown would never hear the key. Both listeners
    // no-op while closed and are removed on the map's own unload so
    // SPA route swaps don't stack them (the same event the teardown
    // latch below rides).
    function onDocumentKeydown(ev) {
      if (!drawerOpen) return;
      if (ev.key === 'Escape' || ev.key === 'Esc') setDrawerOpen(false);
    }
    function onDocumentClick(ev) {
      if (!drawerOpen) return;
      if (drawer.contains(ev.target) || btnWrap.contains(ev.target)) return;
      setDrawerOpen(false);
    }
    document.addEventListener('keydown', onDocumentKeydown);
    document.addEventListener('click', onDocumentClick);
    map.on('unload', function () {
      document.removeEventListener('keydown', onDocumentKeydown);
      document.removeEventListener('click', onDocumentClick);
    });

    // Same-element re-init defense (the old chip row reset its
    // container's innerHTML for the same reason): if a prior init's
    // chip + drawer are still hanging off this shell, drop them before
    // appending the fresh pair so the UI can't stack.
    var staleUi = shell.querySelectorAll('.radar-layers-anchor, .radar-drawer');
    for (var sui = 0; sui < staleUi.length; sui++) {
      staleUi[sui].parentNode.removeChild(staleUi[sui]);
    }
    shell.appendChild(btnWrap);
    shell.appendChild(drawer);
    if (shell === el) {
      // Shell missing (markup older than this script): the UI fell back
      // to living inside the Leaflet container, so keep its clicks and
      // wheel away from the map's handlers, and out-z-index Leaflet's
      // panes (100-700) and controls (800-1000). The stylesheet's 2/3
      // assume the shell stacking context; inside the map container
      // they'd paint underneath the tiles.
      L.DomEvent.disableClickPropagation(btnWrap);
      L.DomEvent.disableClickPropagation(drawer);
      L.DomEvent.disableScrollPropagation(drawer);
      btnWrap.style.zIndex = '1100';
      drawer.style.zIndex = '1101';
    }

    // Badge + pill repaints. LayerGroup children (radar frames, strike
    // rings) also fire layeradd/layerremove on the map; the resync is
    // just a handful of hasLayer checks, so no filtering (the old chip
    // row took the same shortcut). Registered before the initial layer
    // set below so first paint lands the right states.
    function syncLayersUi() {
      rowSyncs.forEach(function (f) { f(); });
      var n = 0;
      overlayDefs.forEach(function (d) { if (map.hasLayer(d.layer)) n++; });
      // Accent count badge; muted when nothing's on so the chip still
      // invites a click rather than reading as a stat.
      layersBtnCount.textContent = n;
      layersBtnCount.classList.toggle('is-empty', n === 0);
      layersBtn.setAttribute('aria-label', 'Layers, ' + n + ' active');
    }
    map.on('layeradd layerremove', syncLayersUi);
    syncLayersUi();

    // Initial layer set, resolved per id: a stored pref wins when present
    // (the user's own toggles, written on every change below), else the
    // SSR'd config default (ui.radar.default_layers). Per-id fallback
    // matters: a provider added to the catalog after the user last saved
    // gets its config default instead of silently starting off. Stored or
    // defaulted ids with no layer in the current effective set are
    // silently ignored, since they never made it into overlayDefs.
    overlayDefs.forEach(function (d) {
      var on = (storedPrefs && typeof storedPrefs[d.id] === 'boolean')
        ? storedPrefs[d.id]
        : defaultLayerIds.indexOf(d.id) !== -1;
      if (on) d.layer.addTo(map);
    });

    // Refresh blend opacities whenever the map zoom settles. Also
    // run once on init so the initial render uses the right values.
    function applyZoomBlend() {
      var z = map.getZoom();
      wmsProviders.forEach(function (p) {
        if (map.hasLayer(p.layer)) {
          p.layer.setOpacity(wmsOpacityForZoom(p.desc, z));
        }
      });
      if (radarLayer && map.hasLayer(radarLayer) && radarFrames.length > 0) {
        // Re-apply opacity to the currently-visible RainViewer frame.
        // showRadarFrame computes from rvOpacityForZoom internally.
        showRadarFrame(radarCurrent);
      }
    }
    map.on('zoomend', applyZoomBlend);
    applyZoomBlend();

    // Persist + re-blend on every layer toggle. The drawer pills call
    // addLayer/removeLayer directly, which fires layeradd/layerremove
    // on the map (the overlayadd/overlayremove pair died with
    // L.control.layers). LayerGroup children (radar frames, strike
    // rings) also fire layeradd on the map and the filter drops them.
    // Registered after the initial set is applied above so first paint
    // doesn't count as a user toggle.
    function isOverlayLayer(layer) {
      return overlayDefs.some(function (d) { return d.layer === layer; });
    }
    // Leaflet's map.remove() (the route-swap teardown path) detaches
    // every layer one by one with this handler still attached, so
    // without a guard each SPA nav away from the radar would persist a
    // progressively-emptier snapshot and end by clobbering the real
    // prefs with all-false. The map fires 'unload' before that removal
    // loop; latch it and stop persisting.
    var tearingDown = false;
    map.on('unload', function () { tearingDown = true; });
    map.on('layeradd layerremove', function (e) {
      if (tearingDown || !isOverlayLayer(e.layer)) return;
      // Nowcast toggled: splice the forecast frames in or out of the
      // cached payload; the next animation tick picks up the new set.
      if (nowcastLayer && e.layer === nowcastLayer && lastRadarData) {
        rebuildRadarFrames(lastRadarData);
      }
      // Data features fetch lazily: nothing is requested while the layer
      // is hidden, so kick a refresh when one comes back on.
      if (warningsGroup && e.layer === warningsGroup && map.hasLayer(warningsGroup)) {
        refreshWarnings();
      }
      if (hurricanesGroup && e.layer === hurricanesGroup && map.hasLayer(hurricanesGroup)) {
        refreshHurricanes();
      }
      // First enable is also where the velocity layer gets built: the
      // group goes on empty and refreshWind populates it from the grid.
      if (windGroup && e.layer === windGroup && map.hasLayer(windGroup)) {
        refreshWind();
      }
      // Re-apply blend so reflectivity layers coming back pick up the
      // right zoom-based opacity instead of the layer's default.
      applyZoomBlend();
      saveLayerPrefs();
    });

    // ---------- Bootstrap ----------

    function refreshRainViewer() {
      if (!rainviewerDesc) return;
      loadRainViewer()
        .then(function (data) { rebuildRadarFrames(data); })
        .catch(function (e) { warnOnce('rainviewer', 'RainViewer load failed', e); });
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

    refreshRainViewer();
    refreshStrikes();
    refreshWarnings();
    refreshHurricanes();
    // Covers a stored "wind: on" pref: the initial layer set is applied
    // before the toggle handler exists, so the lazy build has to be
    // kicked here. No-op when the layer starts hidden (the default).
    refreshWind();
    if (rainviewerDesc) {
      radarPollTimer = setInterval(refreshRainViewer, 5 * 60 * 1000);
    }
    if (strikeLayer) {
      // Strike poll: 60s. Was 30s, doubled to halve the per-IP request
      // pressure on the OAuth-gated /api/snapshot endpoint. CF's bot
      // challenges can fire on a remote IP that issues many cookie-bearing
      // requests in quick succession, and the live SSE stream already
      // delivers the same Tempest snapshot in real time; this polling
      // loop is only the radar.js fallback path for environments where
      // the SSE stream isn't open yet (cold load on the weather route).
      strikePollTimer = setInterval(refreshStrikes, 60 * 1000);
    }
    if (warningsGroup) {
      // Alert cadence per the catalog contract: 2 minutes. refreshWarnings
      // no-ops while the layer is hidden so a toggled-off layer costs
      // api.weather.gov nothing.
      warningsPollTimer = setInterval(refreshWarnings, 2 * 60 * 1000);
    }
    if (hurricanesGroup) {
      hurricanePollTimer = setInterval(refreshHurricanes, 15 * 60 * 1000);
    }
    if (windGroup) {
      // The windgrid serves the current hour and the server caches the
      // grid ~30 minutes; a 15-minute client cadence stays ahead of the
      // hour rollover without hammering anything. refreshWind no-ops
      // while the layer is hidden, so a toggled-off layer costs nothing.
      windPollTimer = setInterval(refreshWind, 15 * 60 * 1000);
    }
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
