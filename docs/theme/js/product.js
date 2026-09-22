// Make overflowing examples and tables reachable with a keyboard.
(() => {
  const update = () => {
    document.querySelectorAll('main pre code, main .table-wrapper').forEach(element => {
      if (element.scrollWidth > element.clientWidth + 1) element.setAttribute('tabindex', '0');
      else element.removeAttribute('tabindex');
    });
  };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', update);
  else update();
  // Keep mdBook's chapter shortcuts from taking over a focused scroll area.
  // Leave the browser's default arrow-key scrolling intact.
  document.addEventListener('keydown', event => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
    if (event.target.matches('main .table-wrapper[tabindex="0"], main pre code[tabindex="0"]')) {
      event.stopPropagation();
    }
  }, true);
  let pending = false;
  window.addEventListener('resize', () => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => { pending = false; update(); });
  });
})();
