// Highlight the irrigation toolbar chip whose section is currently the
// topmost in-viewport target. Pure progressive-enhancement: anchor links
// already work without this; the active-state highlight just gives the
// user a visible "you are here" marker as they scroll.
//
// The chip list is small (5), so we eagerly look up the targets at load
// time and observe them with one shared IntersectionObserver. rootMargin
// pulls the top boundary down so a chip activates once its section's top
// edge crosses ~25% from the top of the viewport rather than the moment
// it pokes in.
(function () {
  if (typeof IntersectionObserver === 'undefined') return;
  var chips = Array.prototype.slice.call(
    document.querySelectorAll('.irrigation-toolbar-chip[data-target]')
  );
  if (!chips.length) return;
  var byTarget = Object.create(null);
  var targets = [];
  for (var i = 0; i < chips.length; i++) {
    var id = chips[i].getAttribute('data-target');
    var el = document.getElementById(id);
    if (el) {
      byTarget[id] = chips[i];
      targets.push(el);
    }
  }

  function setActive(id) {
    for (var i = 0; i < chips.length; i++) {
      chips[i].classList.toggle('is-active', chips[i].getAttribute('data-target') === id);
    }
  }

  // Track which targets are currently intersecting; the topmost one wins.
  var visible = new Set();
  var io = new IntersectionObserver(
    function (entries) {
      for (var i = 0; i < entries.length; i++) {
        var e = entries[i];
        if (e.isIntersecting) visible.add(e.target.id);
        else visible.delete(e.target.id);
      }
      if (!visible.size) return;
      // Pick the visible target with the smallest top offset relative to viewport.
      var best = null;
      var bestTop = Infinity;
      visible.forEach(function (id) {
        var el = document.getElementById(id);
        if (!el) return;
        var top = el.getBoundingClientRect().top;
        // Bias slightly: a section that's about to leave from the top is
        // preferred over one just entering from the bottom.
        if (top < bestTop) {
          bestTop = top;
          best = id;
        }
      });
      if (best) setActive(best);
    },
    {
      // Top inset matches the toolbar's sticky offset (~3.4rem); bottom
      // inset trims the viewport so a section needs to be substantially
      // visible before it claims the chip.
      rootMargin: '-72px 0px -45% 0px',
      threshold: [0, 0.05, 0.25],
    }
  );
  targets.forEach(function (t) { io.observe(t); });

  // Make anchor clicks update the chip immediately rather than waiting
  // for the IntersectionObserver to settle after the scroll animation.
  for (var j = 0; j < chips.length; j++) {
    chips[j].addEventListener('click', function (ev) {
      var id = ev.currentTarget.getAttribute('data-target');
      if (id) setActive(id);
    });
  }
})();
