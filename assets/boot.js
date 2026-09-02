// Applies a stored theme choice before the first paint.
//
// Loaded as a blocking, same-origin script in the <head>, ahead of the
// stylesheet — it must run before anything is drawn or the wrong theme shows
// for a frame, brightly on a phone. It was an inline <script> until the CSP
// tightened to `script-src 'self'`; a served file satisfies that with no
// nonce. Wrapped, because a browser with storage disabled must still render.
try {
  var t = localStorage.getItem('inginx.theme');
  if (t) document.documentElement.setAttribute('data-theme', t);
} catch (e) {}
