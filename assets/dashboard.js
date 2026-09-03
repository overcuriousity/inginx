// The usage charts, on the dashboard and on a key's own page.
//
// The card carries `data-stats-url`, so the same file draws whatever that URL
// returns — all traffic, or one key's — and no inline script is needed, which
// is what lets the Content-Security-Policy stay at `script-src 'self'`.
(async function () {
  const card = document.querySelector('[data-stats-url]');
  if (!card || !document.getElementById('chart-req')) return;
  const s = await (await fetch(card.getAttribute('data-stats-url'))).json();

  // The chart is as wide as the region it sits in. Read from the container
  // rather than from the window: the shell is centred and padded, and the two
  // are not the same number at any width.
  const box = document.getElementById('chart-req').parentElement;
  const width = Math.max(320, box.clientWidth);
  // The palette is the stylesheet's, read once at draw time, so a theme flip
  // needs no second copy of the colours here. uPlot paints to a canvas and
  // cannot read a custom property itself.
  const css = getComputedStyle(document.body);
  const tok = (n) => css.getPropertyValue(n).trim();
  const axisStyle = {
    stroke: tok('--color-fg-muted'),
    font: '11px "JetBrains Mono", ui-monospace, monospace',
    grid: { stroke: tok('--color-border-subtle') },
    ticks: { stroke: tok('--color-border') },
  };
  // Buckets wider than an hour are days or weeks: a time-of-day label on a
  // weekly point is noise, so the tick format follows the bucket.
  const daily = s.bucket_secs >= 86400;
  const timeAxis = Object.assign({}, axisStyle, {
    values: daily
      ? [[3600 * 24 * 365, '{YYYY}', null, null, '{MMM}', null, null, null, 1],
         [3600 * 24, '{MMM} {D}', '\n{YYYY}', null, null, null, null, null, 1]]
      : null,
  });
  function mk(el, series, data) {
    new uPlot({
      width, height: 200,
      cursor: { drag: { setScale: false } },
      series: [{}, ...series],
      axes: [timeAxis, axisStyle],
    }, [s.buckets, ...data], document.getElementById(el));
  }
  // Assigned by index from the fixed categorical order, never recycled.
  mk('chart-req', [
    { label: 'requests', stroke: tok('--viz-series-1'), width: 2 },
    { label: 'errors', stroke: tok('--viz-series-6'), width: 1 },
  ], [s.requests, s.errors]);
  mk('chart-tok', [
    { label: 'prompt', stroke: tok('--viz-series-2'), width: 2 },
    { label: 'completion', stroke: tok('--viz-series-3'), width: 2 },
  ], [s.prompt_tokens, s.completion_tokens]);
})();
