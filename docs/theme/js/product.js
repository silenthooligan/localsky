// Make overflowing examples and tables reachable with a keyboard.
(() => {
  const update = () => {
    document.querySelectorAll('main pre code, main table').forEach(element => {
      if (element.scrollWidth > element.clientWidth + 1) element.setAttribute('tabindex', '0');
      else element.removeAttribute('tabindex');
    });
  };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', update);
  else update();
  let pending = false;
  window.addEventListener('resize', () => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => { pending = false; update(); });
  });
})();
