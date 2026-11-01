/* OSTARA app.js — orchestrator: map, HUD, layers, intel stream, health,
 * keyboard, share links, drawers. Requires layers.js / recon.js / graph.js
 * loaded first (they register on window.OST). No build step. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;

  const REFRESH_MS = 5 * 60 * 1000;
  const HEALTH_MS = 60 * 1000;
  const HOME = { center: [10, 25], zoom: 1.6 };
  const STYLES = {
    dark: "https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json",
    light: "https://basemaps.cartocdn.com/gl/positron-gl-style/style.json",
  };

  /* ---------------- fetch helpers ---------------- */

  OST.fetchJSON = async function (url, opts) {
    opts = opts || {};
    const timeout = opts.timeout || 15000;
    const fetchOpts = Object.assign({}, opts);
    delete fetchOpts.timeout;
    const ctrl = new AbortController();
    let timedOut = false;
    const t = setTimeout(() => { timedOut = true; ctrl.abort(); }, timeout);
    const t0 = performance.now();
    try {
      const r = await fetch(url, Object.assign({}, fetchOpts, { signal: ctrl.signal }));
      const ms = Math.round(performance.now() - t0);
      let data = null;
      try { data = await r.json(); } catch (_) { /* non-JSON body */ }
      if (!r.ok) {
        const err = new Error("HTTP " + r.status);
        err.status = r.status; err.body = data; err.ms = ms;
        throw err;
      }
      return { data: data, ms: ms };
    } catch (e) {
      if (e && e.name === "AbortError" && timedOut) {
        const te = new Error("request timed out"); te.timedOut = true; throw te;
      }
      throw e;
    } finally { clearTimeout(t); }
  };

  OST.errText = function (e) {
    if (!e) return "unknown error";
    if (e.timedOut) return "request timed out after limit";
    if (e.status) {
      const b = e.body || {};
      const detail = b.detail || b.error || "";
      return "HTTP " + e.status + (detail ? " — " + detail : "");
    }
    return e.message || String(e);
  };

  OST.showFetchError = function (el, e, label) {
    el.innerHTML = `<div class="errbox">${esc(label)} failed: ${esc(OST.errText(e))}</div>`;
  };

  OST.toast = function (msg, kind) {
    const box = document.getElementById("toasts");
    const d = document.createElement("div");
    d.className = "toast" + (kind ? " " + kind : "");
    d.textContent = msg;
    box.appendChild(d);
    setTimeout(() => { d.style.opacity = "0"; setTimeout(() => d.remove(), 400); }, 4200);
    while (box.children.length > 4) box.firstChild.remove();
  };

  /* ---------------- share hash ---------------- */

  function readHash() {
    const h = location.hash.replace(/^#/, "");
    const out = {};
    for (const part of h.split("&")) {
      const [k, v] = part.split("=");
      if (k && v != null) out[k] = decodeURIComponent(v);
    }
    return out;
  }
  function writeHash() {
    if (!map) return;
    const c = map.getCenter();
    const on = OST.MAP_LAYERS.filter((l) => layerState[l.id] && layerState[l.id].visible).map((l) => l.id);
    if (layerState.countries && layerState.countries.visible) on.push("countries");
    location.hash = `#c=${c.lat.toFixed(3)},${c.lng.toFixed(3)},${map.getZoom().toFixed(2)}&l=${on.join(",")}`;
  }

  /* ---------------- map ---------------- */

  let map = null;
  const layerState = {}; // id -> {visible, count, available, timer}
  let styleSwitching = false;
  let currentTheme = "dark";

  function initMap() {
    const h = readHash();
    let center = HOME.center, zoom = HOME.zoom;
    if (h.c) {
      const p = h.c.split(",").map(Number);
      if (p.length === 3 && p.every(isFinite)) { center = [p[1], p[0]]; zoom = p[2]; }
    }
    const theme = document.documentElement.dataset.theme || "dark";
    currentTheme = theme;
    map = new maplibregl.Map({
      container: "map",
      style: STYLES[theme] || STYLES.dark,
      center, zoom,
      projection: "globe",
      attributionControl: { compact: true },
    });
    map.addControl(new maplibregl.NavigationControl(), "top-right");

    map.on("load", () => { ensureLayers(); refreshAll(); });
    // re-add our layers after any style swap (theme change), then reload data
    map.on("styledata", () => {
      if (styleSwitching) { styleSwitching = false; ensureLayers(); refreshAll(); }
    });

    map.on("click", (e) => {
      const ids = OST.MAP_LAYERS.map((l) => l.id).filter((id) => map.getLayer(id));
      if (!ids.length) return;
      const feats = map.queryRenderedFeatures(e.point, { layers: ids });
      if (!feats.length) return;
      const p = feats[0].properties;
      new maplibregl.Popup({ closeButton: true })
        .setLngLat(feats[0].geometry.coordinates)
        .setHTML(`<b>${p.title || ""}</b><br/>${p.desc || ""}`)
        .addTo(map);
    });

    // right-click → region dossier (single contextmenu; dbl-right-click is finicky)
    map.on("contextmenu", (e) => {
      e.preventDefault();
      OST.showDossier(e.lngLat.lat, e.lngLat.lng);
    });

    map.on("moveend", writeHash);
  }

  function ensureLayers() {
    // Countries first so feed markers render above the polygons.
    if (OST.ensureCountries) OST.ensureCountries(map, !!(layerState.countries && layerState.countries.visible));
    for (const L of OST.MAP_LAYERS) {
      if (map.getSource(L.id)) continue;
      map.addSource(L.id, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      const paint = L.sized
        ? {
            "circle-radius": ["interpolate", ["linear"], ["get", "mag"], 4, 3, 6, 8, 8, 14],
            "circle-color": L.color, "circle-opacity": 0.75,
            "circle-stroke-width": 1, "circle-stroke-color": "#0b0e13",
          }
        : {
            "circle-radius": 4, "circle-color": L.color, "circle-opacity": 0.85,
            "circle-stroke-width": 1, "circle-stroke-color": "#0b0e13",
          };
      map.addLayer({ id: L.id, type: "circle", source: L.id, paint });
      const st = layerState[L.id];
      if (st && !st.visible) map.setLayoutProperty(L.id, "visibility", "none");
    }
  }

  /* ---------------- layer panel ---------------- */

  function buildLayerPanel() {
    const h = readHash();
    const onIds = h.l ? h.l.split(",") : [...OST.MAP_LAYERS.map((l) => l.id), "countries"];
    const list = document.getElementById("map-layer-list");
    list.innerHTML = "";
    for (const L of OST.MAP_LAYERS) {
      layerState[L.id] = { visible: onIds.includes(L.id), count: null, available: true, wasDown: false };
      const row = document.createElement("label");
      row.className = "layer"; row.id = `row-${L.id}`;
      row.title = L.note || L.label;
      row.innerHTML =
        `<input type="checkbox" data-layer="${L.id}" ${layerState[L.id].visible ? "checked" : ""} />` +
        `<i class="dot" style="background:${L.color}"></i><span>${esc(L.label)}</span>` +
        `<span class="n" id="n-${L.id}">…</span>` +
        `<button class="ref" data-refresh="${L.id}" title="refresh now" style="background:none;border:none;color:var(--dim)">⟳</button>`;
      list.appendChild(row);
    }
    // Paintable countries layer (static GeoJSON, web/data/countries.geo.json).
    layerState.countries = { visible: onIds.includes("countries"), count: null };
    const crow = document.createElement("div");
    crow.className = "layercol"; crow.id = "row-countries";
    crow.innerHTML =
      `<label class="layer" title="Paintable country boundaries — hover to highlight, click for info">` +
      `<input type="checkbox" data-layer="countries" ${layerState.countries.visible ? "checked" : ""} />` +
      `<i class="dot" style="background:#8ecae6"></i><span>Countries</span>` +
      `<span class="n" id="n-countries">…</span></label>` +
      `<div class="paintbar">` +
      `<input type="color" id="paint-color" value="#ffd166" title="paint color" />` +
      `<button id="paint-mode" class="pbtn" title="paint mode: click a country to paint it; click again with the same color to erase">🖌</button>` +
      `<button id="paint-clear" class="pbtn" title="clear all painted countries">✕</button>` +
      `</div>`;
    list.appendChild(crow);
    document.getElementById("paint-mode").addEventListener("click", (ev) => {
      ev.preventDefault();
      const on = OST.togglePaintMode();
      ev.currentTarget.classList.toggle("on", on);
      OST.toast(on
        ? "paint mode on — click a country to paint it (same color again erases)"
        : "paint mode off", "");
    });
    document.getElementById("paint-color").addEventListener("input", (ev) => OST.setPaintColor(ev.target.value));
    document.getElementById("paint-clear").addEventListener("click", (ev) => {
      ev.preventDefault();
      OST.clearCountryPaint(map);
      OST.toast("painting cleared", "");
    });
    list.addEventListener("change", (e) => {
      const id = e.target.dataset && e.target.dataset.layer;
      if (!id) return;
      if (id === "countries") {
        layerState.countries.visible = e.target.checked;
        OST.setCountriesVisible(map, e.target.checked);
        writeHash();
        return;
      }
      if (!map.getLayer(id)) return;
      layerState[id].visible = e.target.checked;
      map.setLayoutProperty(id, "visibility", e.target.checked ? "visible" : "none");
      writeHash();
    });
    list.addEventListener("click", (e) => {
      const btn = e.target.closest("[data-refresh]");
      if (!btn) return;
      e.preventDefault();
      const L = OST.MAP_LAYERS.find((x) => x.id === btn.dataset.refresh);
      if (L) loadLayer(L, true);
    });

    const fl = document.getElementById("feed-list");
    fl.innerHTML = "";
    for (const F of OST.LIST_FEEDS) {
      const row = document.createElement("div");
      row.className = "layer"; row.tabIndex = 0;
      row.innerHTML = `<i class="dot" style="background:var(--accent)"></i><span>${esc(F.label)}</span><span class="n">→</span>`;
      const open = () => { openPanel("feeddrawer"); OST.openFeedTab(F.id); };
      row.addEventListener("click", open);
      row.addEventListener("keydown", (e) => { if (e.key === "Enter") open(); });
      fl.appendChild(row);
    }
  }

  async function loadLayer(L, manual) {
    const nEl = document.getElementById(`n-${L.id}`);
    const row = document.getElementById(`row-${L.id}`);
    const st = layerState[L.id];
    try {
      const { data } = await OST.fetchJSON(L.url, { timeout: 30000 });
      const features = L.toFeatures(data);
      st.count = features.length; st.available = true;
      if (nEl) nEl.textContent = features.length.toLocaleString();
      if (row) { row.classList.remove("unavailable"); row.title = L.note || L.label; }
      if (st.wasDown) { OST.toast(`${L.label} feed is back`, ""); st.wasDown = false; }
      const src = map.getSource(L.id);
      if (src) src.setData({ type: "FeatureCollection", features });
      updateCounts();
    } catch (err) {
      st.count = null; st.available = false;
      if (nEl) nEl.textContent = "n/a";
      if (row) { row.classList.add("unavailable"); row.title = `Unavailable: ${OST.errText(err)}`; }
      if (!st.wasDown || manual) {
        OST.toast(`${L.label}: ${OST.errText(err)}`, "warn");
        st.wasDown = true;
      }
    }
  }

  function refreshAll() {
    return Promise.all(OST.MAP_LAYERS.map((L) => loadLayer(L, false)));
  }

  function updateCounts() {
    const total = OST.MAP_LAYERS.reduce((a, L) => a + ((layerState[L.id] && layerState[L.id].count) || 0), 0);
    const onCount = OST.MAP_LAYERS.filter((l) => layerState[l.id].visible).length +
      (layerState.countries && layerState.countries.visible ? 1 : 0);
    document.getElementById("layer-counts").textContent =
      `${total.toLocaleString()} plotted · ${onCount}/${OST.MAP_LAYERS.length + 1} layers`;
  }

  /* ---------------- feed drawer ---------------- */

  let feedTab = null;
  OST.openFeedTab = function (id) {
    feedTab = id || OST.LIST_FEEDS[0].id;
    const tabs = document.getElementById("feed-tabs");
    tabs.innerHTML = OST.LIST_FEEDS.map((f) =>
      `<button data-ftab="${f.id}" class="${f.id === feedTab ? "on" : ""}">${esc(f.label)}</button>`).join("");
    renderFeedBody();
  };

  function renderFeedBody() {
    const F = OST.LIST_FEEDS.find((f) => f.id === feedTab);
    const body = document.getElementById("feed-body");
    if (!F) { body.innerHTML = `<div class="empty">unknown feed</div>`; return; }
    const inputs = (F.inputs || []).map((i) =>
      `<input data-fin="${i.name}" placeholder="${esc(i.placeholder)}" value="${esc(i.value || "")}" spellcheck="false" />`).join("");
    body.innerHTML =
      `<div class="feed-tools">${inputs}<button id="feed-go">Load</button></div><div id="feed-out"><div class="m">press Load</div></div>`;
    const go = () => loadFeed(F);
    document.getElementById("feed-go").onclick = go;
    body.querySelectorAll("input").forEach((inp) =>
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); }));
    if (!F.inputs) go(); // auto-load feeds that need no input
  }

  async function loadFeed(F) {
    const out = document.getElementById("feed-out");
    const params = {};
    document.querySelectorAll("#feed-body [data-fin]").forEach((i) => { params[i.dataset.fin] = i.value.trim(); });
    if (F.validate) {
      const err = F.validate(params);
      if (err) { out.innerHTML = `<div class="errbox">${esc(err)}</div>`; return; }
    }
    out.innerHTML = `<div class="m">loading…</div>`;
    try {
      const { data } = await OST.fetchJSON(F.url(params), { timeout: 30000 });
      if (F.custom) { F.custom(data, out); return; }
      const items = F.items(data);
      let html = "";
      if (F.note) { const n = F.note(data); if (n) html += `<div class="m" style="margin-bottom:6px">${esc(n)}</div>`; }
      html += `<div class="m" style="margin-bottom:6px">${items.length} result${items.length === 1 ? "" : "s"}` +
        (data.generated_at ? ` · ${esc(data.generated_at)}` : "") + `</div>`;
      html += items.length ? items.slice(0, 100).map(F.itemHTML).join("") : `<div class="empty">no results</div>`;
      out.innerHTML = html;
    } catch (e) { OST.showFetchError(out, e, F.label); }
  }

  /* ---------------- intel stream ---------------- */

  function initIntel() {
    const ticker = document.getElementById("ticker");
    const list = document.getElementById("intel-list");
    const clock = document.getElementById("clock");
    const tickClock = () => { clock.textContent = new Date().toISOString().replace("T", " ").slice(0, 19) + "Z"; };
    tickClock(); setInterval(tickClock, 1000);

    let es;
    try { es = new EventSource("/api/intel/stream"); }
    catch (_) { ticker.textContent = "intel stream unavailable"; return; }

    es.addEventListener("digest", (e) => {
      try {
        const d = JSON.parse(e.data);
        const bits = [];
        if (d.counts && d.counts.quakes != null) bits.push(`<b>${d.counts.quakes}</b> quakes`);
        if (d.counts && d.counts.flights != null) bits.push(`<b>${d.counts.flights}</b> mil aircraft`);
        for (const hl of (d.highlights || []).slice(0, 3)) bits.push(`<b>⚠</b> ${esc(hl)}`);
        ticker.innerHTML = bits.length ? bits.join(" · ") : "no highlights";

        const item = document.createElement("div");
        item.className = "intel-item";
        const dt = d.generated_at ? new Date(d.generated_at) : new Date();
        item.innerHTML = `<time>${isNaN(dt) ? esc(d.generated_at || "") : dt.toISOString().slice(11, 19) + "Z"}</time>` +
          (d.highlights || []).map((hl) => `<div class="hl">⚠ ${esc(hl)}</div>`).join("") +
          `<div class="ct">${d.counts && d.counts.quakes != null ? d.counts.quakes + " quakes" : ""}` +
          `${d.counts && d.counts.flights != null ? " · " + d.counts.flights + " mil aircraft" : ""}</div>`;
        const empty = list.querySelector(".empty");
        if (empty) empty.remove();
        list.prepend(item);
        while (list.children.length > 50) list.lastChild.remove();
      } catch (_) { /* keep last good state */ }
    });
    es.onerror = () => { ticker.textContent = "intel stream reconnecting…"; };
  }

  /* ---------------- health / status bar ---------------- */

  async function pollHealth() {
    try {
      const { data, ms } = await OST.fetchJSON("/api/health/feeds", { timeout: 20000 });
      document.getElementById("api-lat").textContent = `api ${ms}ms`;
      const dots = document.getElementById("health-dots");
      dots.innerHTML = "";
      const feeds = data.feeds || {};
      for (const [name, f] of Object.entries(feeds)) {
        const s = document.createElement("span");
        s.className = "hdot " + (f.status === "ok" ? "ok" : f.status === "degraded" ? "degraded" : f.status === "down" ? "down" : "");
        s.title = `${name}: ${f.status}${f.detail ? " — " + f.detail : ""}`;
        dots.appendChild(s);
      }
      // reflect feed health on map-layer rows
      for (const L of OST.MAP_LAYERS) {
        const f = feeds[L.id];
        const row = document.getElementById(`row-${L.id}`);
        if (!row || !f) continue;
        if (f.status !== "ok") {
          row.classList.add("unavailable");
          row.title = `${L.label}: ${f.status} — ${f.detail || ""}`;
        }
      }
    } catch (e) {
      document.getElementById("api-lat").textContent = "api unreachable";
    }
  }

  /* ---------------- panels ---------------- */

  const RIGHT_PANELS = ["intel", "feeddrawer", "recon", "graphp"];
  let rightOpen = "intel";

  function openPanel(id) {
    const el = document.getElementById(id);
    if (!el) return;
    if (RIGHT_PANELS.includes(id)) {
      for (const p of RIGHT_PANELS)
        document.getElementById(p).classList.toggle("hidden", p !== id);
      rightOpen = id;
    } else {
      el.classList.remove("hidden");
    }
  }
  OST.openPanel = openPanel;

  function closeAll() {
    for (const p of ["layers", ...RIGHT_PANELS, "dossier", "help"])
      document.getElementById(p).classList.add("hidden");
    if (window.maplibregl && map) {
      // close any open popups
      const popups = document.getElementsByClassName("maplibregl-popup");
      while (popups.length) popups[0].remove();
    }
  }

  /* ---------------- keyboard ---------------- */

  function initKeys() {
    document.addEventListener("keydown", (e) => {
      const t = e.target;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT" || t.isContentEditable)) return;
      const k = e.key;
      if (k === "Escape") { closeAll(); return; }
      if (k === "?") { openPanel("help"); return; }
      switch (k.toLowerCase()) {
        case "f":
          if (document.fullscreenElement) document.exitFullscreen();
          else document.documentElement.requestFullscreen().catch(() => {});
          break;
        case "l": {
          const el = document.getElementById("layers");
          el.classList.toggle("hidden");
          break;
        }
        case "i": openPanel("intel"); break;
        case "r": if (map) map.flyTo({ center: HOME.center, zoom: HOME.zoom }); break;
        case "s": {
          e.preventDefault();
          writeHash();
          const url = location.href;
          if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(url).then(
              () => OST.toast("share link copied"),
              () => OST.toast("copy failed — URL is in the address bar", "warn"));
          } else {
            prompt("Copy share link:", url);
          }
          break;
        }
      }
    });
  }

  /* ---------------- HUD buttons / misc ---------------- */

  function initChrome() {
    document.querySelectorAll("[data-act]").forEach((b) => {
      b.addEventListener("click", () => {
        const act = b.dataset.act;
        if (act === "layers") document.getElementById("layers").classList.toggle("hidden");
        else if (act === "intel") openPanel("intel");
        else if (act === "feeds") { openPanel("feeddrawer"); OST.openFeedTab(feedTab); }
        else if (act === "recon") openPanel("recon");
        else if (act === "graph") openPanel("graphp");
        else if (act === "help") openPanel("help");
        else if (act === "refresh-all") refreshAll().then(() => OST.toast("layers refreshed"));
        else if (act === "share") {
          writeHash();
          const url = location.href;
          if (navigator.clipboard && navigator.clipboard.writeText)
            navigator.clipboard.writeText(url).then(() => OST.toast("share link copied"));
          else OST.toast("copy unavailable — URL is in the address bar", "warn");
        }
      });
    });
    document.querySelectorAll("[data-close]").forEach((b) => {
      b.addEventListener("click", () => document.getElementById(b.dataset.close).classList.add("hidden"));
    });
    document.querySelectorAll("[data-theme-set]").forEach((b) => {
      b.addEventListener("click", () => {
        const theme = b.dataset.themeSet;
        document.documentElement.dataset.theme = theme;
        try { localStorage.setItem("ostara-theme", theme); } catch (_) {}
        if (map && theme !== currentTheme) {
          currentTheme = theme;
          styleSwitching = true;
          map.setStyle(STYLES[theme] || STYLES.dark);
        }
      });
    });
    document.getElementById("help").addEventListener("click", (e) => {
      if (e.target.id === "help") e.target.classList.add("hidden");
    });
    // restore saved theme
    try {
      const saved = localStorage.getItem("ostara-theme");
      if (saved === "light" || saved === "dark") document.documentElement.dataset.theme = saved;
    } catch (_) {}
  }

  /* ---------------- boot ---------------- */

  function init() {
    if (typeof maplibregl === "undefined") {
      document.getElementById("ticker").textContent = "map library failed to load (CDN unreachable)";
      return;
    }
    initChrome();
    buildLayerPanel();
    initMap();
    initIntel();
    OST.initRecon();
    OST.initGraph();
    OST.openFeedTab(OST.LIST_FEEDS[0].id);
    initKeys();
    // default: layers + intel visible
    document.getElementById("feeddrawer").classList.add("hidden");
    document.getElementById("recon").classList.add("hidden");
    document.getElementById("graphp").classList.add("hidden");
    document.getElementById("dossier").classList.add("hidden");
    document.getElementById("help").classList.add("hidden");
    setInterval(refreshAll, REFRESH_MS);
    pollHealth();
    setInterval(pollHealth, HEALTH_MS);
  }

  if (document.readyState === "loading")
    document.addEventListener("DOMContentLoaded", init);
  else init();
})(window.OST);
