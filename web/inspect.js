/* OSTARA inspect.js — click-a-country panel.
 *
 * Left-click on empty map area resolves the point via
 * GET /api/country/inspect and shows the country's OSINT resource pack
 * plus the country-tagged items from the other feeds (news, conflicts,
 * nuclear) in one floating panel. Marker popups and the right-click
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
    body.innerHTML = note + osint + news + conflicts + nuclear + flights + quakes + cameras;
    const btn = body.querySelector(".insp-full");
    if (btn) btn.addEventListener("click", () => openFullPack(btn.dataset.iso3, btn.dataset.name));
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
