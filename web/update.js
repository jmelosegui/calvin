// Shared "new version" banner for every calvin page.
// It sits right under the page header and reads the background process's cached update check
// from /api/status, so showing it never goes online. Pages that already poll /api/status can
// hand the result to calvinUpdate.render() instead of waiting for the next refresh.
(() => {
  const DISMISSED_KEY = "calvin-dismissed-update";
  // The server re-checks GitHub at most once a day; this only picks up a check made while the page is open.
  const REFRESH_MS = 30 * 60 * 1000;
  const INSTALL_CMD = navigator.platform.startsWith("Win")
    ? "irm https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.ps1 | iex"
    : "curl -fsSL https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.sh | sh";
  let box = null;

  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

  function mount() {
    if (box) return box;
    box = document.getElementById("update");
    if (!box) {
      box = document.createElement("div");
      box.className = "update";
      box.id = "update";
      box.setAttribute("role", "status");
      const header = document.querySelector("header.top");
      if (header) header.after(box);
      else document.body.prepend(box);
    }
    return box;
  }

  function render(u) {
    const el = mount();
    if (!u || localStorage.getItem(DISMISSED_KEY) === u.latest) {
      el.classList.remove("show");
      return;
    }
    if (el.dataset.latest !== u.latest) {
      el.dataset.latest = u.latest;
      el.innerHTML = `<span><b>calvin ${esc(u.latest)} is available</b> (you have ${esc(u.current)}). Update with <code>${esc(INSTALL_CMD)}</code> · <a href="${esc(u.url)}" target="_blank" rel="noopener">what's new</a></span><button title="Dismiss until the next version" aria-label="Dismiss">×</button>`;
      el.querySelector("button").onclick = () => {
        localStorage.setItem(DISMISSED_KEY, u.latest);
        el.classList.remove("show");
      };
    }
    el.classList.add("show");
  }

  async function refresh() {
    try {
      const r = await fetch("/api/status");
      if (r.ok) render((await r.json()).update);
    } catch {
      // Not worth bothering anyone about; the next refresh will try again.
    }
  }

  function start() {
    refresh();
    setInterval(refresh, REFRESH_MS);
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start);
  else start();

  window.calvinUpdate = { render, refresh };
})();
