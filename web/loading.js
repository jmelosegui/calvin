// Shared loading indicator for every calvin page.
// calvinLoading.fetch() is a drop-in for fetch() that also drives a progress bar along the
// top of the page and a small status chip, so slow queries over a big database never look frozen.
(() => {
  // Fast requests finish before anything is shown, so the page does not flicker.
  const SHOW_AFTER_MS = 200;
  const NULL_BODY = new Set([101, 103, 204, 205, 304]);
  const pending = new Set();
  let batch = [];
  let batchStart = 0;
  let shown = false;
  let showTimer = 0;
  let hideTimer = 0;
  let ticker = 0;
  let ui = null;

  const fmtBytes = (n) => (n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MB" : n >= 1024 ? Math.round(n / 1024) + " KB" : n + " B");

  function mount() {
    if (ui) return ui;
    const bar = document.createElement("div");
    bar.className = "loadbar";
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", "Loading data");
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", "100");
    bar.innerHTML = "<i></i>";
    const chip = document.createElement("div");
    chip.className = "loadchip";
    chip.setAttribute("aria-hidden", "true");
    chip.innerHTML = '<span class="spinner"></span><span></span>';
    document.body.append(bar, chip);
    ui = { bar, fill: bar.firstChild, chip, text: chip.lastChild };
    return ui;
  }

  // While the server is still querying, a request creeps toward halfway; once the
  // response streams in, the bytes received fill the other half.
  function fraction(t, now) {
    if (t.done) return 1;
    if (t.total) return 0.5 + 0.5 * Math.min(1, t.loaded / t.total);
    return 0.5 * (1 - Math.exp(-(now - t.start) / 4000));
  }

  function render() {
    if (!shown || !batch.length) return;
    const { bar, fill, text } = mount();
    const now = performance.now();
    const pct = Math.round((batch.reduce((sum, t) => sum + fraction(t, now), 0) / batch.length) * 100);
    const done = batch.filter((t) => t.done).length;
    const active = batch.find((t) => !t.done);
    const secs = Math.floor((now - batchStart) / 1000);
    const parts = [active ? `Loading ${active.label}…` : "Done"];
    if (batch.length > 1) parts.push(`${done} of ${batch.length}`);
    if (active?.total) parts.push(`${fmtBytes(active.loaded)} of ${fmtBytes(active.total)}`);
    else if (active?.loaded) parts.push(fmtBytes(active.loaded));
    if (secs >= 2) parts.push(`${secs}s`);
    fill.style.width = `${Math.max(pct, 3)}%`;
    bar.setAttribute("aria-valuenow", String(pct));
    text.textContent = parts.join(" · ");
  }

  function show() {
    showTimer = 0;
    if (!pending.size) return;
    const { bar, fill, chip } = mount();
    // Start the bar from empty rather than animating back from the last run.
    fill.style.transition = "none";
    fill.style.width = "0";
    void fill.offsetWidth;
    fill.style.transition = "";
    shown = true;
    bar.classList.add("on");
    chip.classList.add("on");
    render();
  }

  function begin(label) {
    clearTimeout(hideTimer);
    hideTimer = 0;
    if (!pending.size) {
      batch = [];
      batchStart = performance.now();
    }
    const t = { label, start: performance.now(), loaded: 0, total: 0, done: false };
    pending.add(t);
    batch.push(t);
    if (!shown && !showTimer) showTimer = setTimeout(show, SHOW_AFTER_MS);
    if (!ticker) ticker = setInterval(render, 200);
    render();
    return t;
  }

  function end(t) {
    t.done = true;
    pending.delete(t);
    render();
    if (pending.size) return;
    clearInterval(ticker);
    ticker = 0;
    clearTimeout(showTimer);
    showTimer = 0;
    if (!shown) return;
    hideTimer = setTimeout(() => {
      shown = false;
      ui.bar.classList.remove("on");
      ui.chip.classList.remove("on");
    }, 350);
  }

  function labelFor(input) {
    const url = new URL(input instanceof Request ? input.url : String(input), location.href);
    return url.pathname.replace(/^\/api\//, "").replace(/\//g, " ") || "data";
  }

  async function trackedFetch(input, init, label) {
    const t = begin(label || labelFor(input));
    try {
      const r = await fetch(input, init);
      if (!r.body || NULL_BODY.has(r.status)) return r;
      t.total = Number(r.headers.get("content-length")) || 0;
      const reader = r.body.getReader();
      const chunks = [];
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value);
        t.loaded += value.byteLength;
      }
      // Hand back an ordinary Response so callers keep using r.ok, r.json(), r.blob() and headers.
      return new Response(new Blob(chunks), { status: r.status, statusText: r.statusText, headers: r.headers });
    } finally {
      end(t);
    }
  }

  window.calvinLoading = { fetch: trackedFetch };
})();
