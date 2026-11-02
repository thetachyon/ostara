/* OSTARA inspect.js — click-a-country panel.
 *
 * Left-click on empty map area resolves the point via
 * GET /api/country/inspect and shows the country's OSINT resource pack
 * plus the country-tagged items from the other feeds (news, conflicts,
 * nuclear) in one floating panel. An "Unrest signals" section (GET
 * /api/unrest — heuristic indicator, never a forecast) loads async
 * between OSINT and News. Marker popups and the right-click
 * dossier are untouched.
 */
(function (OST) {
  "use strict";
  const esc = OST.esc;

  function link(url, text) {
    return `<a href="${esc(url)}" target="_blank" rel="noopener">${esc(text)}</a>`;
  }

  function feedSection(title, sec, renderItem) {
    let inner;
    if (!sec.ok) {
      inner = `<div class="empty">unavailable — ${esc(sec.error || "feed error")}</div>`;
    } else if (!sec.items.length) {
      inner = `<div class="empty">no items in the current window</div>`;
    } else {
      inner = `<ul class="insp-list">${sec.items.map(renderItem).join("")}</ul>`;
    }
    return `<div class="insp-sec"><h3>${esc(title)} <span class="m">${sec.count}</span></h3>${inner}</div>`;
  }

  function renderOsint(osint, iso3, name) {
    if (!osint) {
      return `<div class="insp-sec"><h3>OSINT resources</h3>
        <div class="empty">no curated pack for this country yet — try the global resources in the Countries drawer</div></div>`;
    }
    const cats = osint.categories || {};
    const names = Object.keys(cats).sort();
    const blocks = names.map((c) => {
      const items = cats[c].slice(0, 3).map((r) =>
        `<li>${link(r.url, r.name)} <span class="acc">${esc(r.access || "")}</span></li>`
      ).join("");
      const more = cats[c].length > 3 ? `<li class="m">+${cats[c].length - 3} more in the full pack</li>` : "";
      return `<div class="insp-cat"><h4>${esc(c)}</h4><ul class="insp-list">${items}${more}</ul></div>`;
    }).join("");
    return `<div class="insp-sec"><h3>OSINT resources <span class="m">${esc(osint.coverage || "")} · ${osint.resource_count || 0}</span></h3>
      ${blocks}
      <button class="insp-full" data-iso3="${esc(iso3)}" data-name="${esc(name)}">Open full pack →</button></div>`;
  }

  function openFullPack(iso3, name) {
    OST.openPanel("cosp");
    const q = document.getElementById("co-q");
    if (q) {
      q.value = name;
      q.dispatchEvent(new Event("input", { bubbles: true }));
      // click the matching row once the list re-renders
      setTimeout(() => {
        const row = document.querySelector(`#co-list [data-iso3="${iso3}"]`);
        if (row) row.click();
      }, 60);
    }
  }

  // ---- Unrest signals (GET /api/unrest) ----
  // Heuristic indicator, never a forecast. Loads async after the main
  // inspector render; the section title says what it is and the full
  // methodology + disclaimer sit one click away in <details>.

  const UNREST_COLORS = { low: "#46a758", elevated: "#ffb224", high: "#f76b15", severe: "#e5484d" };

  function renderUnrest(u) {
    const score = Math.max(0, Math.min(100, Number(u.score) || 0));
    const color = UNREST_COLORS[u.level] || "#888";
    const comps = (u.components || []).map((c) => {
      const head = `<div class="unrest-comp-head"><strong>${esc(c.source)}</strong>
        <span class="m">weight ${Number(c.weight).toFixed(2)}</span>
        ${c.ok ? `<span class="unrest-norm">${Number(c.normalized_0_1).toFixed(3)}</span>`
               : `<span class="unrest-bad">unavailable</span>`}</div>`;
      const vals = c.ok
        ? `<div class="m">recent ${esc(String(c.raw_value))} ${esc(c.raw_unit)} · baseline ${esc(String(c.baseline_value))} ${esc(c.baseline_unit)}</div>`
        : `<div class="m unrest-bad">${esc(c.error || "feed error")}</div>`;
      return `<li>${head}<div class="m">${esc(c.signal)}</div>${vals}${c.note ? `<div class="m">note: ${esc(c.note)}</div>` : ""}</li>`;
    }).join("");
    return `<div class="insp-sec"><h3>Unrest signals <span class="unrest-chip" style="background:${color}">${esc(u.level)}</span>
      <span class="m">heuristic indicator — not a forecast</span></h3>
      <div class="unrest-bar"><div class="unrest-fill" style="width:${score.toFixed(1)}%;background:${color}"></div></div>
      <div class="m">score ${score.toFixed(1)} / 100${u.partial ? " · partial (a feed was down, weight redistributed)" : ""}</div>
      ${u.note ? `<div class="m">${esc(u.note)}</div>` : ""}
      <ul class="insp-list">${comps}</ul>
      <details class="unrest-method"><summary>Methodology</summary>
        <div class="m">${esc(u.methodology)}</div>
        <div class="unrest-disc">${esc(u.disclaimer)}</div>
      </details></div>`;
  }

  async function loadUnrest(iso2) {
    const slot = document.getElementById("insp-unrest");
    if (!slot) return;
    slot.innerHTML = `<div class="insp-sec"><h3>Unrest signals</h3><div class="empty">loading signals…</div></div>`;
    try {
      const { data } = await OST.fetchJSON(`/api/unrest?country=${encodeURIComponent(iso2)}`, { timeout: 60000 });
      slot.innerHTML = renderUnrest(data);
    } catch (e) {
      slot.innerHTML = `<div class="insp-sec"><h3>Unrest signals</h3><div class="empty">unavailable — ${esc(e.message || "fetch error")}</div></div>`;
    }
  }

  // ---- Safety risk (GET /api/risk) ----
  // Heuristic indicator, never a forecast. Same contract as unrest
  // signals: score bar, level chip, safety reading, per-family breakdown,
  // political-unrest diagnostic as a muted row, methodology + disclaimer
  // one click away in <details>.

  const RISK_COLORS = { low: "#46a758", moderate: "#ffb224", high: "#f76b15", severe: "#e5484d" };

  function renderRisk(r) {
    const score = Math.max(0, Math.min(100, Number(r.score) || 0));
    const color = RISK_COLORS[r.level] || "#888";
    const compLi = (c) => {
      const head = `<div class="unrest-comp-head"><strong>${esc(c.source)}</strong>
        <span class="m">weight ${Number(c.weight).toFixed(2)}</span>
        ${c.ok ? `<span class="unrest-norm">${Number(c.normalized_0_1).toFixed(3)}</span>`
               : `<span class="unrest-bad">unavailable</span>`}</div>`;
      const vals = c.ok
        ? `<div class="m">recent ${esc(String(c.raw_value))} ${esc(c.raw_unit)} · baseline ${esc(String(c.baseline_value))} ${esc(c.baseline_unit)}</div>`
        : `<div class="m unrest-bad">${esc(c.error || "feed error")}</div>`;
      return `<li>${head}<div class="m">${esc(c.signal)}</div>${vals}${c.note ? `<div class="m">note: ${esc(c.note)}</div>` : ""}</li>`;
    };
    const fams = (r.families || []).map((f) =>
      `<li><div class="unrest-comp-head"><strong>${esc(f.family)}</strong>
        <span class="m">weight ${Number(f.weight).toFixed(2)}</span>
        <span class="unrest-norm">${Number(f.score_0_100 || 0).toFixed(1)}</span></div>
        <ul class="insp-list">${(f.components || []).map(compLi).join("")}</ul></li>`
    ).join("");
    const pu = r.political_unrest;
    const puRow = pu ? `<li style="opacity:0.7"><div class="unrest-comp-head"><strong>political unrest</strong>
        <span class="m">diagnostic — not in the safety score · ${pu.score_0_100 == null ? "unavailable" : `${Number(pu.score_0_100).toFixed(1)} / 100 · ${esc(pu.level)}`}</span></div>
        <div class="m">broad protest/strike/riot/unrest keywords — political signal for supply-chain / operations exposure</div></li>` : "";
    return `<div class="insp-sec"><h3>Safety risk <span class="unrest-chip" style="background:${color}">${esc(r.level)}</span>
      <span class="m">danger to persons — heuristic indicator, not a forecast</span></h3>
      <div class="unrest-bar"><div class="unrest-fill" style="width:${score.toFixed(1)}%;background:${color}"></div></div>
      <div class="m">safety risk ${score.toFixed(1)} / 100 · safety: <strong>${esc(r.safety || "—")}</strong>${r.partial ? " · partial (a feed was down, weight redistributed)" : ""}${r.thin_basis ? " · thin basis — treat the level as provisional" : ""}</div>
      ${r.note ? `<div class="m">${esc(r.note)}</div>` : ""}
      <ul class="insp-list">${fams}${puRow}</ul>
      <details class="unrest-method"><summary>Methodology</summary>
        <div class="m">${esc(r.methodology)}</div>
        <div class="unrest-disc">${esc(r.disclaimer)}</div>
      </details></div>`;
  }

  async function loadRisk(iso2) {
    const slot = document.getElementById("insp-risk");
    if (!slot) return;
    slot.innerHTML = `<div class="insp-sec"><h3>Safety risk</h3><div class="empty">loading safety signals…</div></div>`;
    try {
      const { data } = await OST.fetchJSON(`/api/risk?place=${encodeURIComponent(iso2)}&kind=country`, { timeout: 120000 });
      slot.innerHTML = renderRisk(data);
    } catch (e) {
      slot.innerHTML = `<div class="insp-sec"><h3>Safety risk</h3><div class="empty">unavailable — ${esc(e.message || "fetch error")}</div></div>`;
    }
  }

  function render(data) {
    const body = document.getElementById("insp-body");
    const head = document.getElementById("insp-title");
    if (!data.country) {
      head.textContent = "International waters";
      body.innerHTML = `<div class="empty">🌊 ${esc(data.resolution_note)} — no country, no feeds, no OSINT pack here.</div>`;
      return;
    }
    const c = data.country;
    head.innerHTML = `${esc(c.name)} <span class="iso">${esc(c.iso2)} · ${esc(c.iso3)}</span>`;
    const note = data.resolution === "nearest-country"
      ? `<div class="insp-note">⚠️ ${esc(data.resolution_note)}</div>` : "";
    const osint = renderOsint(data.osint, c.iso3, c.name);
    const f = data.feeds || {};
    const news = feedSection("News", f.news || { ok: true, count: 0, items: [] }, (a) =>
      `<li>${link(a.url, a.title || a.url)}<br/><span class="m">${esc(a.domain || "")}${a.seen_at ? " · " + esc(a.seen_at.slice(0, 10)) : ""}</span></li>`);
    const conflicts = feedSection("Alerts", f.conflicts || { ok: true, count: 0, items: [] }, (e) =>
      `<li>${link(e.url, e.title)}<br/><span class="m">${esc(e.date || "")}</span></li>`);
    const nuclear = feedSection("Nuclear sites", f.nuclear || { ok: true, count: 0, items: [] }, (n) =>
      `<li>${esc(n.name)}<br/><span class="m">${esc(n.status || "")} · ${n.lat.toFixed(2)}, ${n.lon.toFixed(2)}</span></li>`);
    const flights = feedSection("Aircraft", f.flights || { ok: true, count: 0, items: [] }, (a) =>
      `<li>${esc(a.callsign || a.hex)}${a.aircraft_type ? " · " + esc(a.aircraft_type) : ""}<br/><span class="m">${a.altitude_m != null ? Math.round(a.altitude_m) + " m" : "alt n/a"}${a.speed_knots != null ? " · " + Math.round(a.speed_knots) + " kt" : ""} · ${a.latitude.toFixed(2)}, ${a.longitude.toFixed(2)}</span></li>`);
    const quakes = feedSection("Earthquakes", f.quakes || { ok: true, count: 0, items: [] }, (q) =>
      `<li>${q.magnitude != null ? "M" + q.magnitude.toFixed(1) : "M?"} — ${esc(q.place || q.id)}<br/><span class="m">${esc((q.time || "").slice(0, 10))} · ${q.latitude.toFixed(2)}, ${q.longitude.toFixed(2)}</span></li>`);
    const cameras = feedSection("Cameras", f.cameras || { ok: true, count: 0, items: [] }, (c) =>
      `<li>${link(c.page_url, c.name)}<br/><span class="m">${esc(c.category || "")}${c.location ? " · " + esc(c.location) : ""}</span></li>`);
    const whatsapp = feedSection("WhatsApp channels", f.whatsapp_channels || { ok: true, count: 0, items: [] }, (w) =>
      `<li>${link(w.invite_url, w.name)}${w.verified ? " ✓" : ""}<br/><span class="m">${esc(w.category || "")}${w.language ? " · " + esc(w.language) : ""} · directory, not live messages</span></li>`);
    const vessels = feedSection("Vessels", f.vessels || { ok: true, count: 0, items: [] }, (v) =>
      `<li>${esc(v.name || v.mmsi)}${v.vessel_type ? " · " + esc(v.vessel_type) : ""}<br/><span class="m">mmsi ${esc(String(v.mmsi))}${v.speed_knots != null ? " · " + Math.round(v.speed_knots) + " kt" : ""} · ${v.latitude.toFixed(2)}, ${v.longitude.toFixed(2)}</span></li>`);
    body.innerHTML = note + osint + `<div id="insp-unrest"></div><div id="insp-risk"></div>` + news + conflicts + nuclear + flights + quakes + cameras + whatsapp + vessels;
    const btn = body.querySelector(".insp-full");
    if (btn) btn.addEventListener("click", () => openFullPack(btn.dataset.iso3, btn.dataset.name));
    loadUnrest(c.iso2);
    loadRisk(c.iso2);
  }

  OST.inspectCountry = async function (lat, lon) {
    OST.openPanel("insp");
    document.getElementById("insp-title").textContent = "Country";
    const body = document.getElementById("insp-body");
    body.innerHTML = `<div class="empty">resolving ${lat.toFixed(2)}, ${lon.toFixed(2)}…</div>`;
    try {
      const { data } = await OST.fetchJSON(
        `/api/country/inspect?lat=${encodeURIComponent(lat)}&lon=${encodeURIComponent(lon)}`,
        { timeout: 45000 }
      );
      render(data);
    } catch (e) {
      document.getElementById("insp-title").textContent = "Country";
      OST.showFetchError(body, e, "country inspect");
    }
  };
})(window.OST);
