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
//                          hurricanes       NHC storms + track/cone
//                          lightning_tempest local Tempest strike rings
//
// Center/zoom default to data-lat / data-lon / data-zoom on #radar-map
// (set by SSR from the configured station location). The Tempest strike
// layer pulls from /api/snapshot, Tempest reports distance to a strike
// but not bearing, so each strike is plotted as a distance ring centered
// on the station rather than a point.
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
    { id: 'hurricanes', label: 'Hurricanes (NOAA NHC track + cone)', endpoints: [], attribution: 'NOAA/NHC' },
    { id: 'lightning_tempest', label: 'Lightning (Tempest station)', endpoints: [], attribution: 'Tempest (local station)' },
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

  // Compact chip labels for the mobile row, keyed by catalog id. Anything
  // not listed falls back to the descriptor label with the parenthetical
  // stripped, so a brand-new provider still gets a usable chip.
  var SHORT_LABELS = {
    rainviewer: 'Precip',
    nexrad_iem: 'NEXRAD',
    nowcoast: 'NOAA Mosaic',
    geomet_ca: 'Radar CA',
    dwd_de: 'Radar DE',
    fmi_fi: 'Radar FI',
    nowcast: 'Forecast',
    warnings_us: 'Alerts',
    hurricanes: 'Hurricanes',
    lightning_tempest: 'Strikes',
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

  // NHC aggregate summary service (recon-verified): layer 6 = forecast
  // track lines, layer 7 = forecast cones. The per-storm service uses
  // per-slot layer groups, the summary service answers in one query each.
  var NHC_STORMS_URL = 'https://www.nhc.noaa.gov/CurrentStorms.json';
  var NHC_SUMMARY_BASE =
    'https://mapservices.weather.noaa.gov/tropical/rest/services/tropical/NHC_tropical_weather_summary/MapServer';
  var NHC_TRACK_URL = NHC_SUMMARY_BASE + '/6/query?where=1%3D1&outFields=*&f=geojson';
  var NHC_CONE_URL = NHC_SUMMARY_BASE + '/7/query?where=1%3D1&outFields=*&f=geojson';

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
    // (easier thumb reach), replace the in-map layer toggle with the chip
    // row, and turn off `tap` so single-tap-then-drag doesn't get eaten by
    // Leaflet's tap-handler quirk on iOS Safari. attributionControl moves
    // to bottom-left out of the way of the play button.
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

    // ---------- Feature: NHC hurricanes ----------
    //
    // Three sources merged per refresh: forecast cones (polygons), forecast
    // tracks (polylines), and CurrentStorms.json for named position markers.
    // Each fetch fails independently; a quiet basin (zero features) renders
    // nothing at all, silently. 15-minute cadence: advisories move slowly.

    var hurricanesDesc = featureById('hurricanes');
    var hurricanesGroup = hurricanesDesc ? L.layerGroup() : null;

    function refreshHurricanes() {
      if (!hurricanesGroup || !map.hasLayer(hurricanesGroup)) return;
      // Descriptor endpoints in catalog order: storm summary JSON,
      // forecast track query, forecast cone query. The recon constants
      // cover descriptors that arrive without endpoints.
      var eps = hurricanesDesc.endpoints;
      var coneP = fetch(eps[2] || NHC_CONE_URL)
        .then(function (r) { return r.json(); })
        .catch(function (e) { warnOnce('nhc-cone', 'NHC forecast cone fetch failed', e); return null; });
      var trackP = fetch(eps[1] || NHC_TRACK_URL)
        .then(function (r) { return r.json(); })
        .catch(function (e) { warnOnce('nhc-track', 'NHC forecast track fetch failed', e); return null; });
      var stormsP = fetch(eps[0] || NHC_STORMS_URL)
        .then(function (r) { return r.json(); })
        .catch(function (e) { warnOnce('nhc-storms', 'NHC CurrentStorms fetch failed', e); return null; });
      Promise.all([coneP, trackP, stormsP]).then(function (res) {
        var cone = res[0], track = res[1], storms = res[2];
        hurricanesGroup.clearLayers();
        var attribution = hurricanesDesc.attribution || 'NOAA/NHC';
        if (cone && Array.isArray(cone.features) && cone.features.length) {
          L.geoJSON(cone, {
            style: { color: '#ff6b81', weight: 1, fillColor: '#ff6b81', fillOpacity: 0.12 },
            attribution: attribution,
          }).addTo(hurricanesGroup);
        }
        if (track && Array.isArray(track.features) && track.features.length) {
          L.geoJSON(track, {
            style: { color: '#ff6b81', weight: 2 },
            attribution: attribution,
          }).addTo(hurricanesGroup);
        }
        var active = (storms && storms.activeStorms) || [];
        active.forEach(function (s) {
          var sLat = s.latitudeNumeric;
          var sLon = s.longitudeNumeric;
          if (!isFinite(sLat) || !isFinite(sLon)) return;
          var name = s.name || 'Storm';
          if (s.classification) name += ' (' + s.classification + ')';
          L.circleMarker([sLat, sLon], {
            radius: 6,
            color: '#ff4757',
            fillColor: '#ff4757',
            fillOpacity: 0.9,
            weight: 2,
            attribution: attribution,
          })
            .bindTooltip(name, { direction: 'top' })
            .addTo(hurricanesGroup);
        });
      });
    }

    // ---------- Feature: local Tempest strike rings ----------
    //
    // Tempest reports distance-to-strike but not bearing, so we draw a
    // pulsing ring at the reported radius around the station for each
    // strike from the last hour. The newest strike is highlighted.

    var lightningDesc = featureById('lightning_tempest');
    var strikeLayer = lightningDesc ? L.layerGroup() : null;
    var lastStrikeIds = new Set();

    function strikeRadiusMeters(distanceMi) {
      return Math.max(distanceMi, 0.1) * 1609.34;
    }

    function refreshStrikes() {
      if (!strikeLayer) return;
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
    // The overlay list is generated from the descriptor arrays: providers
    // first (catalog order), then features. Both the desktop control and
    // the mobile chip row are built from this one list, and persistence
    // speaks descriptor ids so display labels can be reworded freely
    // without invalidating stored prefs.

    var overlayDefs = [];
    if (radarLayer) {
      overlayDefs.push({ id: rainviewerDesc.id, label: rainviewerDesc.label, layer: radarLayer, desc: rainviewerDesc });
    }
    wmsProviders.forEach(function (p) {
      overlayDefs.push({ id: p.desc.id, label: p.desc.label, layer: p.layer, desc: p.desc });
    });
    if (nowcastLayer) {
      overlayDefs.push({ id: nowcastDesc.id, label: nowcastDesc.label, layer: nowcastLayer, desc: nowcastDesc });
    }
    if (warningsGroup) {
      overlayDefs.push({ id: warningsDesc.id, label: warningsDesc.label, layer: warningsGroup, desc: warningsDesc });
    }
    if (hurricanesGroup) {
      overlayDefs.push({ id: hurricanesDesc.id, label: hurricanesDesc.label, layer: hurricanesGroup, desc: hurricanesDesc });
    }
    if (strikeLayer) {
      overlayDefs.push({ id: lightningDesc.id, label: lightningDesc.label, layer: strikeLayer, desc: lightningDesc });
    }

    var overlays = {};
    overlayDefs.forEach(function (d) { overlays[d.label] = d.layer; });

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

    // On mobile we ditch the in-map L.control.layers entirely (even
    // collapsed it covered the map when the user tapped the toggle, which
    // defeats the purpose of an interactive map). Instead we render a
    // horizontal chip row in #radar-layer-chips below the map, same
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
        // descriptor label becomes the aria-label so screen readers still
        // get the context. Each chip toggles between .is-on / not-on,
        // mirroring the actual map state, and addLayer / removeLayer
        // drive the visibility.
        chipsContainer.innerHTML = '';
        overlayDefs.forEach(function (d) {
          var layer = d.layer;
          var btn = document.createElement('button');
          btn.type = 'button';
          btn.className = 'radar-layer-chip';
          btn.setAttribute('aria-label', d.label);
          btn.setAttribute('aria-pressed', 'false');
          btn.textContent = shortLabelFor(d.id, d.label);

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
    map.on('overlayadd overlayremove layeradd layerremove', function (e) {
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
      // Re-apply blend so reflectivity layers coming back pick up the
      // right zoom-based opacity instead of the layer's default.
      applyZoomBlend();
      saveLayerPrefs();
    });

    // Custom legend control, generated from the active descriptors so it
    // never describes a layer that isn't in the menu. Always visible,
    // bottom-left on desktop. On mobile we skip it entirely; the chip
    // row's labels are enough, and the legend's verbose prose was just
    // covering the map. Swatch colors are inline so new catalog entries
    // need no CSS changes.
    var LEGEND_SWATCH = {
      rainviewer: '#58a6ff',
      nowcast: '#58e0ff',
      warnings_us: '#ff9f1c',
      hurricanes: '#ff4757',
      lightning_tempest: '#ffe066',
    };
    function legendText(d) {
      if (d.id === 'nowcast') {
        return 'Extends the precipitation animation with RainViewer forecast frames, tagged "+Nm forecast" in the time readout.';
      }
      if (d.id === 'warnings_us') {
        return 'NWS active alert polygons. Fill color is severity: red extreme, orange severe. Tap a polygon for the headline. Refreshes every 2 min.';
      }
      if (d.id === 'hurricanes') {
        return 'NHC active storms: position markers, forecast track lines, and the forecast cone. Empty when the basins are quiet.';
      }
      if (d.id === 'lightning_tempest') {
        return 'Yellow ring = strike from your station. Tempest reports distance, not bearing, so each strike is a ring at the reported radius.';
      }
      if (d.desc && d.desc.kind === 'rainviewer') {
        return 'Animated recent frames. dBZ scale: blue light · green moderate · yellow to orange to red heavy.';
      }
      if (d.desc && d.desc.kind === 'wms') {
        var text = 'Reflectivity mosaic via WMS, coverage: ' + (d.desc.coverageLabel || 'regional') + '.';
        if (d.desc.crossfade) {
          text += ' Crossfades in past z=7 so detail stays sharp at street scale.';
        }
        if (d.desc.attribution) {
          text += ' Source: ' + d.desc.attribution + '.';
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
    var Legend = L.Control.extend({
      options: { position: 'bottomleft' },
      onAdd: function () {
        var div = L.DomUtil.create('div', 'radar-legend');
        var rows = overlayDefs.map(function (d) {
          return ''
            + '<div class="radar-legend-row">'
            +   '<span class="radar-legend-swatch" style="background:' + legendSwatchColor(d) + '"></span>'
            +   '<div class="radar-legend-text">'
            +     '<strong>' + escHtml(d.label) + '</strong>'
            +     '<span>' + escHtml(legendText(d)) + '</span>'
            +   '</div>'
            + '</div>';
        }).join('');
        div.innerHTML = ''
          + '<div class="radar-legend-head">'
          +   '<span>Legend</span>'
          +   '<button type="button" class="radar-legend-toggle" aria-label="Toggle legend">−</button>'
          + '</div>'
          + '<div class="radar-legend-body">' + rows + '</div>';

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
