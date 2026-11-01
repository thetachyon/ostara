/* OSTARA country.js — Country OSINT directory drawer.
 * Every entry below is a real backend route (/api/country-osint*).
 * The directory is curated link data (names + URLs only, no prose copied
 * from the source repos); see data/country_osint.json provenance notes.
 * Coverage badges are honest: full / partial / links. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;

  const CAT_LABELS = {
    search: "Search", people: "People & phone", business: "Business & corporate",
    government: "Government & courts", news: "News & media", social: "Social",
    finance: "Finance & sanctions", collections: "Link collections",
  };
  const CAT_ORDER = ["search", "people", "business", "government", "news", "social", "finance", "collections"];

  let countries = [];
  let loaded = false;
  let filter = "all";

  function covBadge(cov) {
    const cls = cov === "full" ? "ok" : cov === "partial" ? "warn" : "mut";
    return `<span class="badge ${cls}">${esc(cov)}</span>`;
  }

  function accBadge(acc) {
    if (!acc || acc === "unknown") return "";
    const cls = acc === "free" ? "ok" : acc === "paid" ? "bad" : "warn";
    return `<span class="badge ${cls}">${esc(acc)}</span>`;
  }

  function renderCountryList(q) {
    const box = document.getElementById("co-list");
    const query = (q || "").trim().toLowerCase();
    const rows = countries.filter((c) => {
      if (filter !== "all" && c.coverage !== filter) return false;
      if (!query) return true;
      return c.name.toLowerCase().includes(query) ||
        c.iso2.toLowerCase() === query || c.iso3.toLowerCase() === query;
    });
    if (!rows.length) {
      box.innerHTML = `<div class="empty">no countries match — try a name or ISO code</div>`;
      return;
    }
    box.innerHTML = rows.map((c) =>
      `<button class="co-row" data-iso3="${esc(c.iso3)}">
         <span class="co-name">${esc(c.name)}</span>
         <span class="co-meta">${esc(c.iso2)} · ${c.resource_count} resources ${covBadge(c.coverage)}</span>
       </button>`
    ).join("");
  }

  function renderPack(c) {
    const body = document.getElementById("co-body");
    let html = `<div class="co-head">
      <h3>${esc(c.name)} <span class="m">${esc(c.iso2)} / ${esc(c.iso3)}</span></h3>
      <div>${covBadge(c.coverage)} <span class="m">${c.resource_count} resources · curated links, open in new tab</span></div>
    </div>`;
    for (const cat of CAT_ORDER) {
      const rs = c.categories[cat];
      if (!rs || !rs.length) continue;
      html += `<div class="co-cat"><h4>${CAT_LABELS[cat]} <span class="m">${rs.length}</span></h4>`;
      for (const r of rs) {
        html += `<div class="co-res">
          <a href="${esc(r.url)}" target="_blank" rel="noopener">${esc(r.name)}</a>
          ${accBadge(r.access)}
          ${r.note ? `<div class="m">${esc(r.note)}</div>` : ""}
        </div>`;
      }
      html += `</div>`;
    }
    body.innerHTML = html;
  }

  async function loadPack(iso3) {
    const body = document.getElementById("co-body");
    body.innerHTML = `<div class="empty">loading pack…</div>`;
    try {
      const { data } = await OST.fetchJSON(`/api/country-osint?country=${encodeURIComponent(iso3)}`, { timeout: 15000 });
      renderPack(data);
    } catch (e) { OST.showFetchError(body, e, "country pack"); }
  }

  async function loadGlobal() {
    const body = document.getElementById("co-body");
    body.innerHTML = `<div class="empty">loading global resources…</div>`;
    try {
      const { data } = await OST.fetchJSON("/api/country-osint/global", { timeout: 15000 });
      let html = `<div class="co-head"><h3>Global resources</h3>
        <div><span class="m">${data.count} country-agnostic resources — sanctions, corporate, leaks</span></div></div>
        <div class="co-cat">`;
      for (const r of data.resources) {
        html += `<div class="co-res">
          <a href="${esc(r.url)}" target="_blank" rel="noopener">${esc(r.name)}</a>
          ${accBadge(r.access)}
          ${r.note ? `<div class="m">${esc(r.note)}</div>` : ""}
        </div>`;
      }
      body.innerHTML = html + `</div>`;
    } catch (e) { OST.showFetchError(body, e, "global resources"); }
  }

  OST.initCountry = function () {
    const panel = document.getElementById("cosp");
    const list = document.getElementById("co-list");
    const search = document.getElementById("co-q");
    const chips = document.getElementById("co-chips");

    chips.addEventListener("click", (e) => {
      const f = e.target.dataset && e.target.dataset.f;
      if (!f) return;
      filter = f;
      chips.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.dataset.f === f));
      renderCountryList(search.value);
    });
    search.addEventListener("input", () => renderCountryList(search.value));
    list.addEventListener("click", (e) => {
      const row = e.target.closest("[data-iso3]");
      if (!row) return;
      list.querySelectorAll(".co-row").forEach((r) => r.classList.toggle("on", r === row));
      loadPack(row.dataset.iso3);
    });
    document.getElementById("co-global").addEventListener("click", loadGlobal);

    OST.fetchJSON("/api/country-osint/countries", { timeout: 15000 })
      .then(({ data }) => {
        countries = data.sort((a, b) => a.name.localeCompare(b.name));
        loaded = true;
        renderCountryList("");
        document.getElementById("co-count").textContent = `${countries.length} countries`;
      })
      .catch((e) => {
        list.innerHTML = "";
        OST.showFetchError(list, e, "country list");
      });
  };
})(window.OST);
