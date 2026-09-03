// The theme toggle, and nothing else.
//
// The pre-paint script in the <head> is what applies a stored choice before
// anything is drawn; this only has to keep the button honest. Three states are
// possible — light, dark, and "whatever the system says" — and the button
// names the destination rather than the state: a button reading "Dark" while
// the page is dark reads as a label rather than as something to press.
(function () {
  var btn = document.querySelector('[data-theme-toggle]');
  if (!btn) return;
  var label = btn.querySelector('[data-theme-label]');

  function current() {
    var set = document.documentElement.getAttribute('data-theme');
    if (set) return set;
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  function paint() {
    label.textContent = current() === 'dark' ? 'Light' : 'Dark';
  }

  btn.addEventListener('click', function () {
    var next = current() === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', next);
    try { localStorage.setItem('inginx.theme', next); } catch (e) {}
    paint();
  });
  paint();
})();

// A whole-row link, delegated. A table row cannot be wrapped in an <a>, and an
// inline `onclick` is exactly what `script-src 'self'` forbids — so the row
// carries `data-href` and one listener on the document turns a click into a
// navigation. A click that lands on a real control inside the row (a link, a
// button, a form field) is left alone, so the row's own actions still work.
(function () {
  document.addEventListener('click', function (e) {
    var row = e.target.closest('[data-href]');
    if (!row) return;
    if (e.target.closest('a, button, input, select, textarea, form')) return;
    window.location.href = row.getAttribute('data-href');
  });
})();

// `confirm()` before a destructive POST, without an inline handler. The form
// carries `data-confirm="…question…"`; this asks it on submit and cancels the
// submit if the answer is no. Replaces the `onclick="return confirm(...)"`
// that the CSP now blocks.
(function () {
  document.addEventListener('submit', function (e) {
    var form = e.target.closest('form[data-confirm]');
    if (!form) return;
    if (!window.confirm(form.getAttribute('data-confirm'))) e.preventDefault();
  });
})();

// The model picker on the API-keys form: filter, live count, and the
// "All models" interlock. The interlock's *visual* half is a CSS `:has()` rule
// so the form is still honest with this file missing; what is here is the
// convenience — picking a model turns "All models" off, and turning it back on
// clears the picks, so the form never submits a contradiction.
(function () {
  var picker = document.querySelector('[data-model-picker]');
  if (!picker) return;
  var all = document.querySelector('[data-all-models]');
  var filter = picker.querySelector('[data-model-filter]');
  var count = picker.querySelector('[data-model-count]');
  var clear = picker.querySelector('[data-model-clear]');
  var boxes = Array.prototype.slice.call(picker.querySelectorAll('input[name="model_ids"]'));

  function paint() {
    var n = boxes.filter(function (b) { return b.checked; }).length;
    count.textContent = n === 1 ? '1 selected' : n + ' selected';
  }

  picker.addEventListener('change', function (e) {
    if (e.target.name !== 'model_ids') return;
    if (e.target.checked && all) all.checked = false;
    paint();
  });

  if (all) {
    all.addEventListener('change', function () {
      if (!all.checked) return;
      boxes.forEach(function (b) { b.checked = false; });
      paint();
    });
  }

  if (clear) {
    clear.addEventListener('click', function () {
      boxes.forEach(function (b) { b.checked = false; });
      paint();
    });
  }

  if (filter) {
    filter.addEventListener('input', function () {
      var q = filter.value.trim().toLowerCase();
      boxes.forEach(function (b) {
        var row = b.parentElement;
        var name = (row.getAttribute('data-model-name') || '').toLowerCase();
        // A checked model stays visible: hiding it would hide a pick that is
        // still going to be submitted.
        row.hidden = q !== '' && !b.checked && name.indexOf(q) === -1;
      });
    });
  }

  paint();
})();
