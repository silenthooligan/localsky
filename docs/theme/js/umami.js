// Self-hosted Umami analytics for the published docs at localsky.io/docs.
// No cookies, no PII; just URL + referrer + duration. Served first-party
// from /stats (Caddy proxies to the Umami container) so it measures the
// real audience instead of whoever forgot an ad blocker. Guard: only
// fires on the official localsky.io domain, so local previews and
// self-built copies of these docs never send traffic anywhere.
(function () {
  if (!/(^|\.)localsky\.io$/.test(location.hostname)) {
    return;
  }
  var s = document.createElement("script");
  s.async = true;
  s.defer = true;
  s.src = location.origin + "/stats/u.js";
  s.setAttribute("data-website-id", "1eba7858-b978-4e3b-9219-adfde25ae228");
  s.setAttribute("data-host-url", location.origin + "/stats");
  document.head.appendChild(s);
})();
