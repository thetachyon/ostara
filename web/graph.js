/* OSTARA graph.js — entity-graph tree view + region dossier panel.
 * Endpoints (all real): GET /api/graph?entity=, GET /api/graph/expand?entity=&depth=,
 * GET /api/dossier?lat=&lon=&radius_km=. Tree view is v1 by design. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;

  /* ---------------- entity graph ---------------- */

  async function searchGraph(entity) {
    const body = document.getElementById("graph-body");
    body.innerHTML = `<div class="m">searching…</div>`;
    try {
      const { data } = await OST.fetchJSON(`/api/graph?entity=${encodeURIComponent(entity)}`, { timeout: 30000 });
      renderDetail(body, data, 0);
    } catch (e) { OST.showFetchError(body, e, "graph"); }
  }

  function nodeRow(n, relation, feed, depth) {
    const div = document.createElement("div");
    div.className = "gnode";
    div.innerHTML =
      `<div class="grow" title="click to expand">` +
      `<span class="kind">${esc(n.kind || "?")}</span>` +
      `<span><b>${esc(n.entity)}</b></span>` +
      (relation ? `<span class="rel">${esc(relation)}</span>` : "") +
      (feed ? `<span class="feed">${esc(feed)}</span>` : "") +
      `</div><div class="kids hidden"></div>`;
    const kids = div.querySelector(".kids");
    let loaded = false;
    div.querySelector(".grow").addEventListener("click", async () => {
      if (kids.classList.contains("hidden")) {
        kids.classList.remove("hidden");
        if (!loaded) {
          loaded = true;
          kids.innerHTML = `<div class="m">expanding…</div>`;
          try {
            const { data } = await OST.fetchJSON(
              `/api/graph/expand?entity=${encodeURIComponent(n.entity)}&depth=1`, { timeout: 30000 });
            kids.innerHTML = "";
            renderExpandInto(kids, data, depth + 1);
          } catch (e) {
            kids.innerHTML = `<div class="errbox">${esc(OST.errText(e))}</div>`;
            loaded = false;
          }
        }
      } else {
        kids.classList.add("hidden");
      }
    });
    return div;
  }

  function renderDetail(body, d, depth) {
    body.innerHTML = "";
    const head = document.createElement("div");
    head.className = "gnode";
    head.innerHTML = `<div class="grow"><span class="kind">${esc(d.kind || "?")}</span><span><b>${esc(d.entity)}</b></span>` +
      `<span class="feed">${(d.neighbors || []).length} neighbours</span></div>`;
    body.appendChild(head);
    const wrap = document.createElement("div");
    for (const n of (d.neighbors || []))
      wrap.appendChild(nodeRow({ entity: n.entity, kind: n.kind }, n.relation, n.source_feed, depth));
    if (!wrap.children.length) wrap.innerHTML = `<div class="empty">no neighbours found</div>`;
    body.appendChild(wrap);
  }

  function renderExpandInto(el, d, depth) {
    // group edges by their far endpoint relative to the queried entity
    const seen = new Set();
    for (const e of (d.edges || [])) {
      const other = e.from.entity === d.entity ? e.to : e.from;
      const key = other.kind + "|" + other.entity;
      if (seen.has(key)) continue;
      seen.add(key);
      el.appendChild(nodeRow(other, e.relation, e.source_feed, depth));
    }
    const orphans = (d.nodes || []).filter((n) => n.entity !== d.entity && !seen.has(n.kind + "|" + n.entity));
    for (const n of orphans.slice(0, 20)) el.appendChild(nodeRow(n, null, null, depth));
    if (!el.children.length) el.innerHTML = `<div class="empty">no further neighbours</div>`;
  }

  OST.initGraph = function () {
    const q = document.getElementById("graph-q");
    const go = () => { const v = q.value.trim(); if (v) searchGraph(v); };
    document.getElementById("graph-go").onclick = go;
    q.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
  };

  /* ---------------- region dossier ---------------- */

  OST.showDossier = async function (lat, lon) {
    const card = document.getElementById("dossier");
    const body = document.getElementById("dossier-body");
    card.classList.remove("hidden");
    body.innerHTML = `<div class="m">compiling dossier for ${lat.toFixed(3)}, ${lon.toFixed(3)}…</div>`;
    try {
      const { data } = await OST.fetchJSON(
        `/api/dossier?lat=${lat.toFixed(4)}&lon=${lon.toFixed(4)}&radius_km=100`, { timeout: 45000 });
      renderDossier(body, data);
    } catch (e) { OST.showFetchError(body, e, "dossier"); }
  };

  function sectionHTML(title, sec, itemFn) {
    if (!sec) return "";
    const items = sec.items || [];
    const count = sec.count != null ? sec.count : items.length;
    let h = `<div class="dsec"><h4>${esc(title)} · ${count}</h4>`;
    if (sec.ok === false) h += `<div class="m">${esc(sec.detail || sec.error || "unavailable")}</div>`;
    else if (!items.length) h += `<div class="m">none in radius</div>`;
    else h += items.slice(0, 8).map(itemFn).join("") + (items.length > 8 ? `<div class="m">… ${items.length - 8} more</div>` : "");
    return h + `</div>`;
  }

  function renderDossier(body, d) {
    const s = d.sections || {};
    body.innerHTML =
      `<div class="m" style="margin-bottom:8px">${d.lat.toFixed(3)}, ${d.lon.toFixed(3)} · radius ${d.radius_km} km · ${esc(d.generated_at || "")}</div>` +
      sectionHTML("Aircraft", s.flights, (f) => `<div class="di"><b>${esc(f.callsign || f.hex || "?")}</b> · alt ${f.altitude_m != null ? Math.round(f.altitude_m) + "m" : "gnd"}</div>`) +
      sectionHTML("Earthquakes", s.quakes, (q) => `<div class="di"><b>M${q.magnitude != null ? Number(q.magnitude).toFixed(1) : "?"}</b> · ${esc(q.place || "")}</div>`) +
      sectionHTML("Wildfires", s.fires, (f) => `<div class="di"><b>${Math.round(f.brightness || 0)} K</b> · ${esc(f.acq_date || "")} · ${esc(f.satellite || "")}</div>`) +
      sectionHTML("Nuclear", s.nuclear, (n) => `<div class="di"><b>${esc(n.name || "")}</b> · ${esc(n.country || "")} · ${esc(n.status || "")}</div>`) +
      sectionHTML("Cameras", s.cameras, (c) => `<div class="di"><b>${esc(c.name || "")}</b> · ${esc(c.location || "")}</div>`) +
      sectionHTML("Conflicts", s.conflicts, (c) => `<div class="di"><b>${esc(c.title || "").slice(0, 80)}</b> · ${esc(c.date || "")}</div>`);
  }
})(window.OST);
