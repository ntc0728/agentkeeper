'use strict';
/**
 * 在 styles.css 上色之前就跑，好让第一帧的 <html> 上已经是正确的主题。单独成文件
 * （而不是内联）是因为 CSP 是 script-src 'self'，没有 'unsafe-inline'。
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