// Location pin picker: a Leaflet map on OpenStreetMap tiles that the setup
// wizard's Location step and Settings -> Location open on demand (the
// "Pick on map" button in components::geo_search). Same contract as
// radar.js: Rust renders <div data-geo-picker data-lat data-lon>, this file
// mounts Leaflet on it when it appears and tears it down when it goes, and
// the only thing it tells Rust is "the operator put the pin here", as a
// bubbling `geo-pick` CustomEvent with { lat, lon } in detail. Rust owns the
// coordinates; the data-lat/data-lon attributes it writes are what moves
// the pin when a search hit, the device position or a typed value lands.
//
// Loaded once at app boot (app.rs), deferred after leaflet.js, like radar.js
// and for the same reason: a per-route script tag re-ran the IIFE on every
// visit and stacked observers.
//
// Tiles are fetched only while a map is open, from tile.openstreetmap.org,
// the same base layer the radar uses, with the attribution OSM asks for.
(function () {
  'use strict';

  var TILE_URL = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
  var TILE_ATTRIBUTION =
    '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors';
  // A street and its neighbors once a pin exists; the whole world before.
  var PLACED_ZOOM = 16;
  var WORLD_VIEW = { center: [20, 0], zoom: 2 };

  var mounted = new Map(); // element -> { map, marker, observer }

  function coordinate(value) {
    var n = parseFloat(value);
    return isFinite(n) ? n : null;
  }

  // The host's current coordinates, or null when unset. 0,0 is the blank
  // draft's null island, never a yard.
  function hostPosition(el) {
    var lat = coordinate(el.dataset.lat);
    var lon = coordinate(el.dataset.lon);
    if (lat === null || lon === null || (lat === 0 && lon === 0)) return null;
    return { lat: lat, lon: lon };
  }

  // Five decimals is about a meter; more is pointer jitter.
  function round5(n) {
    return Math.round(n * 1e5) / 1e5;
  }

  // A CSS dot, themed by the stylesheet, so no marker image is fetched.
  function pinIcon() {
    return L.divIcon({
      className: 'geo-pin',
      html: '<span class="geo-pin__dot"></span>',
      iconSize: [28, 28],
      iconAnchor: [14, 14],
    });
  }

  function mount(el) {
    if (mounted.has(el) || typeof L === 'undefined') return;
    var reduced =
      window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    var map = L.map(el, {
      zoomControl: true,
      attributionControl: true,
      worldCopyJump: true,
      fadeAnimation: !reduced,
      zoomAnimation: !reduced,
      markerZoomAnimation: !reduced,
    });
    map.attributionControl.setPrefix(false);
    L.tileLayer(TILE_URL, { maxZoom: 19, attribution: TILE_ATTRIBUTION }).addTo(map);
    var state = { map: map, marker: null, observer: null };
    mounted.set(el, state);

    function place(lat, lon) {
      if (state.marker) {
        state.marker.setLatLng([lat, lon]);
        return;
      }
      state.marker = L.marker([lat, lon], {
        draggable: true,
        keyboard: true,
        icon: pinIcon(),
        title: 'Your location. Drag to adjust.',
        alt: 'Location pin',
      }).addTo(map);
      state.marker.on('dragend', function () {
        var p = state.marker.getLatLng();
        announce(p.lat, p.lng);
      });
    }

    // The operator moved the pin: place it and tell Rust.
    function announce(lat, lon) {
      lat = round5(lat);
      lon = round5(lon);
      place(lat, lon);
      el.dispatchEvent(
        new CustomEvent('geo-pick', { bubbles: true, detail: { lat: lat, lon: lon } })
      );
    }

    var start = hostPosition(el);
    if (start) {
      map.setView([start.lat, start.lon], PLACED_ZOOM);
      place(start.lat, start.lon);
    } else {
      map.setView(WORLD_VIEW.center, WORLD_VIEW.zoom);
    }

    map.on('click', function (e) {
      announce(e.latlng.lat, e.latlng.lng);
    });

    // Follow the host: a search hit, the device position or a typed value
    // moves the pin and recenters, without echoing a geo-pick back.
    state.observer = new MutationObserver(function () {
      var next = hostPosition(el);
      if (!next) return;
      if (state.marker) {
        var p = state.marker.getLatLng();
        if (Math.abs(p.lat - next.lat) < 5e-6 && Math.abs(p.lng - next.lon) < 5e-6) return;
      }
      place(next.lat, next.lon);
      map.setView([next.lat, next.lon], Math.max(map.getZoom(), PLACED_ZOOM), {
        animate: !reduced,
      });
    });
    state.observer.observe(el, { attributes: true, attributeFilter: ['data-lat', 'data-lon'] });

    el.dataset.geoPickerReady = 'yes';
    // Leaflet sizes itself at construction; the container was inserted a
    // moment ago, so measure again once layout has settled.
    setTimeout(function () {
      map.invalidateSize();
    }, 0);
  }

  function unmount(el) {
    var state = mounted.get(el);
    if (!state) return;
    if (state.observer) state.observer.disconnect();
    try {
      state.map.remove();
    } catch (e) {
      /* already detached */
    }
    mounted.delete(el);
  }

  function pickers(root) {
    if (!(root instanceof Element)) return [];
    var list = root.matches('[data-geo-picker]') ? [root] : [];
    return list.concat(Array.prototype.slice.call(root.querySelectorAll('[data-geo-picker]')));
  }

  function start() {
    // leaflet.js is deferred ahead of this file, so L is normally here
    // already; a slow cache is the only reason to wait.
    if (typeof L === 'undefined') {
      setTimeout(start, 50);
      return;
    }
    pickers(document.body).forEach(mount);
    new MutationObserver(function (mutations) {
      mutations.forEach(function (m) {
        m.addedNodes.forEach(function (n) {
          pickers(n).forEach(mount);
        });
        m.removedNodes.forEach(function (n) {
          pickers(n).forEach(unmount);
        });
      });
    }).observe(document.body, { childList: true, subtree: true });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
