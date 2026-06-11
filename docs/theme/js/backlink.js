// Adds a "localsky.io" home link to the mdBook menu bar so readers can
// always get back to the product homepage (the book title links to the
// book root, not the site).
(function () {
  function inject() {
    var right = document.querySelector(".menu-bar .right-buttons");
    if (!right || document.getElementById("ls-home-link")) return;
    var a = document.createElement("a");
    a.id = "ls-home-link";
    a.href = "https://localsky.io/";
    a.title = "Back to localsky.io";
    a.innerHTML = "localsky.io ↗";
    right.insertBefore(a, right.firstChild);
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", inject);
  } else {
    inject();
  }
})();
