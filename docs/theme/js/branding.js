// Brands the mdBook chrome: replaces the plain-text menu title with
// the LocalSky logomark + LOCALSKY wordmark (matching the app and the
// homepage). The logomark src is read from the page's own favicon
// link, so mdBook's asset hashing can't break it.
(function () {
  function inject() {
    var title = document.querySelector(".menu-bar .menu-title");
    if (!title || title.querySelector(".ls-wordmark")) return;
    var icon = document.querySelector('link[rel="icon"]');
    var img = icon
      ? '<img src="' + icon.href + '" alt="" width="20" height="20">'
      : "";
    title.innerHTML =
      '<span class="ls-brand">' +
      img +
      '<span class="ls-wordmark">LOCAL<em>SKY</em></span>' +
      '<span class="ls-docs-tag">docs</span></span>';
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", inject);
  } else {
    inject();
  }
})();
