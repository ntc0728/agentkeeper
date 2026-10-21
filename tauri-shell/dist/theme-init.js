'use strict';
/**
 * Runs before styles.css paints so the correct theme is on <html> for the
 * very first frame. Kept separate (rather than inline) because the CSP is
 * script-src 'self' with no 'unsafe-inline'.
 */
(function () {
  var pref = localStorage.getItem('ak.theme') || 'system';
  var dark = pref === 'dark'
    || (pref === 'system' && window.matchMedia
      && window.matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
  if (window.matchMedia) {
    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', function (e) {
      if ((localStorage.getItem('ak.theme') || 'system') === 'system') {
        document.documentElement.setAttribute('data-theme', e.matches ? 'dark' : 'light');
      }
    });
  }
})();