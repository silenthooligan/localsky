// Self-hosted Umami analytics for docs.localsky.io. No cookies, no PII;
// just URL + referrer + duration. Loads the tracker script async so mdBook
// page-load isn't blocked, and ignores tracking when the page is loaded
// from a localhost preview (`mdbook serve`).
(function () {
  if (
    location.hostname === "localhost" ||
    location.hostname === "127.0.0.1" ||
    location.hostname.endsWith(".local")
  ) {
    return;
  }
  var s = document.createElement("script");
  s.async = true;
  s.defer = true;
  s.src = "https://YOUR_ANALYTICS_HOST/script.js";
  s.setAttribute("data-website-id", "1eba7858-b978-4e3b-9219-adfde25ae228");
  document.head.appendChild(s);
})();
