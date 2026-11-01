/* OSTARA feeds.js — dedicated Feeds panel: a structured browser for all 16 feed
 * categories, distinct from the SSE intel stream and the country inspector.
 * Category navigator with live counts + health dots, per-category inputs,
 * tables/cards/kv renderers, manual + auto refresh, and "show on map"
 * cross-links that flip the layer-rail toggles (reused, not reimplemented).
 * Loaded before app.js; registers on window.OST. No build step. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;

  const REFRESH_MS = 5 * 60 * 1000;
  const HEALTH_MS = 60 * 1000;
  const ROW_CAP = 100;

  /* ---------------- small builders ---------------- */

  function table(headers, rows) {
    return `<table class="ftable"><thead><tr>${headers.map((h) => `<th>${esc(h)}</th>`).join("")}</tr></thead>` +
      `<tbody>${rows.map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("")}</tbody></table>`;
  }

  function kv(pairs) {
    return `<table class="kv">` + pairs.map(([k, v]) =>
      `<tr><td>${esc(k)}</td><td>${v}</td></tr>`).join("") + `</table>`;
  }

  function genLine(j) {
    return j.generated_at ? `<div class="m" style="margin:6px 0">generated ${esc(j.generated_at)}</div>` : "";
  }

  function mono(s) { return `<span style="font-family:var(--mono);font-size:11px">${esc(s)}</span>`; }

  /* ---------------- category definitions ---------------- */
  // health: key in /api/health/feeds (null = static/local, shown as a blue dot)
  // map: id in OST.MAP_LAYERS for the "show on map" cross-link
  // inputs: rendered as a form; {required:true} blocks auto-load until Load is pressed

  OST.FEED_CATS = [
    {
      id: "flights", label: "Flights", health: "flights", map: "flights",
      group: "country",
      inputs: [
        { name: "scope", type: "select", options: ["mil", "all"], value: "mil" },
        { name: "limit", placeholder: "limit", value: "100" },
      ],
      url: (p) => `/api/flights?scope=${encodeURIComponent(p.scope || "mil")}&limit=${encodeURIComponent(p.limit || "100")}`,
      render(j, el) {
        const rows = (j.flights || []).slice(0, ROW_CAP).map((f) => [
          esc(f.callsign || f.registration || f.hex || "?") +
            (f.emergency ? ` <b style="color:var(--bad)">EMG ${esc(f.emergency)}</b>` : ""),
          esc(f.aircraft_type || "—"),
          f.altitude_m != null ? Math.round(f.altitude_m) + " m" : (f.on_ground ? "gnd" : "—"),
          f.speed_knots != null ? Math.round(f.speed_knots) + " kt" : "—",
          f.track_deg != null ? Math.round(f.track_deg) + "°" : "—",
        ]);
        el.innerHTML = table(["Callsign", "Type", "Alt", "Spd", "Track"], rows) +
          `<div class="m" style="margin-top:6px">${(j.flights || []).length} of ${j.count != null ? j.count : "?"} shown · scope ${esc(j.scope || "")} · upstream total ${j.total_upstream != null ? j.total_upstream : "?"}</div>` +
          genLine(j);
        this.count = (j.flights || []).length;
      },
    },
    {
      id: "vessels", label: "Vessels", health: "vessels", map: "vessels",
      group: "global",
      note: "no AIS REST adapter — 503 by design until AIS_API_KEY is wired",
      url: () => "/api/vessels",
      render(j, el) {
        el.innerHTML = `<div class="empty">no vessels — no AIS adapter configured</div>` + genLine(j);
        this.count = 0;
      },
    },
    {
      id: "fires", label: "Fires", health: "fires", map: "fires",
      group: "global",
      note: "NASA FIRMS — 503 until FIRMS_API_KEY is set",
      url: () => "/api/fires",
      render(j, el) {
        const rows = (j.fires || []).slice(0, ROW_CAP).map((f) => [
          Math.round(f.brightness || 0) + " K",
          esc(f.acq_date || "—"),
          esc(f.satellite || "—"),
          `${Number(f.lat).toFixed(2)}, ${Number(f.lon).toFixed(2)}`,
        ]);
        el.innerHTML = table(["Brightness", "Acquired", "Satellite", "Coords"], rows) +
          `<div class="m" style="margin-top:6px">${(j.fires || []).length} shown</div>` + genLine(j);
        this.count = (j.fires || []).length;
      },
    },
    {
      id: "quakes", label: "Quakes", health: "quakes", map: "quakes",
      group: "country",
      url: () => "/api/quakes",
      render(j, el) {
        const rows = (j.quakes || []).slice(0, ROW_CAP).map((q) => [
          `<b>M${q.magnitude != null ? Number(q.magnitude).toFixed(1) : "?"}</b>`,
          esc(q.place || "—"),
          esc((q.time || "").replace("T", " ").slice(0, 19)),
          q.depth_km != null ? Number(q.depth_km).toFixed(1) + " km" : "—",
          q.tsunami ? "🌊" : "",
        ]);
        el.innerHTML = table(["Mag", "Place", "Time (UTC)", "Depth", ""], rows) +
          `<div class="m" style="margin-top:6px">${(j.quakes || []).length} shown · M4.5+ past day</div>` + genLine(j);
        this.count = (j.quakes || []).length;
      },
    },
    {
      id: "weather", label: "Weather", health: "weather",
      group: "global",
      inputs: [
        { name: "lat", placeholder: "latitude", value: "40.71", required: true },
        { name: "lon", placeholder: "longitude", value: "-74.00", required: true },
      ],
      validate: (p) => {
        const la = Number(p.lat), lo = Number(p.lon);
        if (!isFinite(la) || la < -90 || la > 90) return "latitude must be in [-90, 90]";
        if (!isFinite(lo) || lo < -180 || lo > 180) return "longitude must be in [-180, 180]";
        return null;
      },
      url: (p) => `/api/weather?lat=${encodeURIComponent(p.lat)}&lon=${encodeURIComponent(p.lon)}`,
      render(j, el) {
        el.innerHTML = kv([
          ["temperature", j.temperature_c != null ? Number(j.temperature_c).toFixed(1) + " °C" : "—"],
          ["wind", j.wind_speed_kmh != null ? Number(j.wind_speed_kmh).toFixed(1) + " km/h" : "—"],
          ["elevation", j.elevation_m != null ? Math.round(j.elevation_m) + " m" : "—"],
          ["observed", esc(j.time || "—")],
          ["coords", `${j.latitude}, ${j.longitude}`],
        ]) + genLine(j);
        this.count = 1;
      },
    },
    {
      id: "satellites", label: "Satellites", health: "satellites",
      group: "global",
      inputs: [{ name: "limit", placeholder: "limit", value: "50" }],
      url: (p) => `/api/satellites?limit=${encodeURIComponent(p.limit || "50")}`,
      render(j, el) {
        const items = j.satellites || [];
        el.innerHTML =
          (j.note ? `<div class="m" style="margin-bottom:6px">${esc(j.note)}</div>` : "") +
          `<div class="m" style="margin-bottom:6px">${items.length} object${items.length === 1 ? "" : "s"}</div>` +
          items.slice(0, ROW_CAP).map((s) =>
            `<div class="feed-item"><div class="t">${esc(s.name || "")} <span class="m">NORAD ${s.norad_id}</span></div>` +
            `<div class="m" style="font-size:10px">${esc((s.line1 || "").slice(0, 69))}<br/>${esc((s.line2 || "").slice(0, 69))}</div></div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "n2yo", label: "Overhead (N2YO)", health: "n2yo",
      group: "global",
      inputs: [
        { name: "lat", placeholder: "latitude (default 0)" },
        { name: "lon", placeholder: "longitude (default 0)" },
      ],
      url: (p) => `/api/satellites/catalog?lat=${encodeURIComponent(p.lat || "0")}&lon=${encodeURIComponent(p.lon || "0")}`,
      render(j, el) {
        const rows = (j.satellites || []).slice(0, ROW_CAP).map((s) => [
          esc(s.name || "—"),
          String(s.norad_id != null ? s.norad_id : "—"),
          s.latitude != null ? Number(s.latitude).toFixed(2) : "—",
          s.longitude != null ? Number(s.longitude).toFixed(2) : "—",
          s.altitude_km != null ? Number(s.altitude_km).toFixed(0) + " km" : "—",
        ]);
        el.innerHTML = table(["Name", "NORAD", "Lat", "Lon", "Alt"], rows) +
          `<div class="m" style="margin-top:6px">${(j.satellites || []).length} shown · source ${esc(j.source || "")}</div>` + genLine(j);
        this.count = (j.satellites || []).length;
      },
    },
    {
      id: "news", label: "News", health: "news",
      group: "country",
      inputs: [{ name: "q", placeholder: "search news… (required)", required: true }],
      validate: (p) => (p.q && p.q.trim() ? null : "enter a search query"),
      url: (p) => `/api/news?q=${encodeURIComponent(p.q || "")}&max_records=25`,
      render(j, el) {
        const items = j.articles || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} article${items.length === 1 ? "" : "s"}</div>` +
          items.map((a) =>
            `<div class="feed-item"><div class="t"><a href="${esc(a.url)}" target="_blank" rel="noopener">${esc(a.title || a.url)}</a></div>` +
            `<div class="m">${esc(a.domain || "")} · ${esc(a.source_country || "")} · ${esc(a.language || "")} · ${esc(a.seen_at || "")}</div></div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "cameras", label: "Cameras", health: "cameras", map: "cameras",
      group: "country",
      staticNote: "embedded registry — 48 curated public cameras",
      inputs: [
        { name: "category", type: "select", options: ["all", "traffic", "weather", "park"], value: "all" },
        { name: "limit", placeholder: "limit", value: "50" },
      ],
      url: (p) => `/api/cameras?limit=${encodeURIComponent(p.limit || "50")}` +
        (p.category && p.category !== "all" ? `&category=${encodeURIComponent(p.category)}` : ""),
      render(j, el) {
        const items = j.cameras || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} camera${items.length === 1 ? "" : "s"}</div>` +
          items.slice(0, ROW_CAP).map((c) =>
            `<div class="feed-item"><div class="t">${esc(c.name || "camera")}</div>` +
            `<div class="m">${esc(c.location || "")} · ${esc(c.category || "")} · ${Number(c.lat).toFixed(2)}, ${Number(c.lon).toFixed(2)}</div>` +
            `<div class="m">` +
            (c.image_url ? `<a href="${esc(c.image_url)}" target="_blank" rel="noopener">snapshot</a>` : "") +
            (c.image_url && c.page_url ? " · " : "") +
            (c.page_url ? `<a href="${esc(c.page_url)}" target="_blank" rel="noopener">source</a>` : "") +
            `</div></div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "markets", label: "Markets (CoinGecko)", health: "markets",
      group: "global",
      note: "global crypto data \u2014 no country attribution (listed under Global for this reason)",
      url: () => "/api/markets",
      render(j, el) {
        const rows = (j.markets || []).slice(0, ROW_CAP).map((m) => {
          const chg = m.change_24h_pct != null ? Number(m.change_24h_pct).toFixed(2) + "%" : "—";
          const cls = m.change_24h_pct > 0 ? "color:var(--good)" : m.change_24h_pct < 0 ? "color:var(--bad)" : "";
          const cap = m.market_cap_usd != null ? "$" + (m.market_cap_usd / 1e9).toFixed(1) + "B" : "—";
          return [
            `<b>${esc(m.symbol)}</b><br/><span style="color:var(--dim)">${esc(m.name || "")}</span>`,
            "$" + Number(m.price_usd).toLocaleString(),
            `<span style="${cls}">${chg}</span>`,
            cap,
          ];
        });
        el.innerHTML = table(["Asset", "Price", "24h", "Mkt cap"], rows) + genLine(j);
        this.count = (j.markets || []).length;
      },
    },
    {
      id: "conflicts", label: "Conflicts", health: "conflicts",
      group: "country",
      url: () => "/api/conflicts",
      render(j, el) {
        const items = j.events || j.conflicts || [];
        el.innerHTML =
          (j.note ? `<div class="m" style="margin-bottom:6px">${esc(j.note)}</div>` : "") +
          `<div class="m" style="margin-bottom:6px">${items.length} event${items.length === 1 ? "" : "s"}</div>` +
          items.slice(0, ROW_CAP).map((c) =>
            `<div class="feed-item"><div class="t">${esc(c.title || "")}</div>` +
            `<div class="m">${esc(c.country || "")} · ${esc(c.date || "")}${c.url ? ` · <a href="${esc(c.url)}" target="_blank" rel="noopener">source</a>` : ""}</div></div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "cyber", label: "Cyber IOCs", health: "cyber",
      group: "global",
      note: "URLhaus + ThreatFox merged — 503 until THREATFOX_API_KEY is set",
      url: () => "/api/cyber/threats",
      render(j, el) {
        const rows = (j.indicators || []).slice(0, ROW_CAP).map((t) => [
          esc(t.kind || "—"),
          mono(t.value || "—"),
          esc(t.threat_type || "—"),
          esc((t.first_seen || "").replace("T", " ").slice(0, 19)),
        ]);
        el.innerHTML = table(["Kind", "Indicator", "Threat", "First seen"], rows) +
          `<div class="m" style="margin-top:6px">${(j.indicators || []).length} shown</div>` + genLine(j);
        this.count = (j.indicators || []).length;
      },
    },
    {
      id: "threatfox", label: "ThreatFox", health: "threatfox",
      group: "global",
      note: "abuse.ch — 503 until THREATFOX_API_KEY is set",
      inputs: [
        { name: "malware", placeholder: "malware family (optional)" },
        { name: "days", placeholder: "days (1–7)", value: "1" },
      ],
      url: (p) => `/api/threatfox?days=${encodeURIComponent(p.days || "1")}&limit=100` +
        (p.malware ? `&malware=${encodeURIComponent(p.malware)}` : ""),
      render(j, el) {
        const rows = (j.iocs || []).slice(0, ROW_CAP).map((t) => [
          mono(t.ioc || "—"),
          esc(t.ioc_type || "—"),
          esc(t.malware_printable || t.malware || "—"),
          esc(t.threat_type || "—"),
          t.confidence_level != null ? t.confidence_level : "?",
        ]);
        el.innerHTML = table(["IOC", "Type", "Malware", "Threat", "Conf"], rows) +
          `<div class="m" style="margin-top:6px">${(j.iocs || []).length} shown · ${j.days != null ? j.days : "?"} day(s)</div>` + genLine(j);
        this.count = (j.iocs || []).length;
      },
    },
    {
      id: "telegram", label: "Telegram", health: "telegram",
      group: "global",
      note: "503 until TELEGRAM_API_ID + TELEGRAM_API_HASH are set",
      inputs: [{ name: "channel", placeholder: "channel name (no @)", required: true }],
      validate: (p) => (p.channel && p.channel.trim() ? null : "enter a channel name"),
      url: (p) => `/api/telegram?channel=${encodeURIComponent(p.channel || "")}`,
      render(j, el) {
        const items = j.messages || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} message${items.length === 1 ? "" : "s"}</div>` +
          items.slice(0, ROW_CAP).map((m) =>
            `<div class="feed-item"><div class="m">@${esc(m.channel || "")} · ${esc(m.date || "")} · 👁 ${m.views != null ? m.views : "?"}</div>` +
            `<div>${esc((m.text || "").slice(0, 280))}${(m.text || "").length > 280 ? "…" : ""}</div>` +
            (m.url ? `<div class="m"><a href="${esc(m.url)}" target="_blank" rel="noopener">open</a></div>` : "") + `</div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "helio", label: "Space Wx", health: "helio",
      group: "global",
      url: () => "/api/space-weather",
      render(j, el) {
        el.innerHTML = kv([
          ["X-ray flux (GOES)", j.xray_flux_latest != null ? Number(j.xray_flux_latest).toExponential(2) + " W/m²" : "—"],
          ["Kp index", j.kp_latest != null ? j.kp_latest : "—"],
          ["generated", esc(j.generated_at || "—")],
          ["note", esc(j.note || "—")],
        ]);
        this.count = 1;
      },
    },
    {
      id: "nuclear", label: "Nuclear", health: "nuclear", map: "nuclear",
      group: "country",
      staticNote: "embedded registry — 189 stations",
      inputs: [{ name: "country", placeholder: "country filter (optional)" }],
      url: (p) => "/api/nuclear" + (p.country ? `?country=${encodeURIComponent(p.country)}` : ""),
      render(j, el) {
        const rows = (j.facilities || []).slice(0, ROW_CAP).map((n) => [
          esc(n.name || "—"),
          esc(n.country || "—"),
          esc(n.status || "—"),
          esc(n.reactor_type || "—"),
        ]);
        el.innerHTML = table(["Facility", "Country", "Status", "Reactor"], rows) +
          `<div class="m" style="margin-top:6px">${(j.facilities || []).length} shown</div>` + genLine(j);
        this.count = (j.facilities || []).length;
      },
    },
  ];

  /* ---------------- state ---------------- */

  let curCat = "flights";
  let health = {};       // health key -> {status, detail}
  let lastParams = {};   // cat id -> params used on last load
  let healthTimer = null, refreshTimer = null;

  function catById(id) { return OST.FEED_CATS.find((c) => c.id === id); }

  function healthDot(cat) {
    if (cat.health == null) return `<i class="hdot static" title="static registry — always available"></i>`;
    const h = health[cat.health];
    if (!h) return `<i class="hdot" title="health unknown"></i>`;
    const cls = h.status === "ok" ? "ok" : h.status === "degraded" ? "degraded" : h.status === "down" ? "down" : "";
    return `<i class="hdot ${cls}" title="${esc(cat.health)}: ${esc(h.status)}${h.detail ? " — " + h.detail : ""}"></i>`;
  }

  /* ---------------- navigator ---------------- */

  const FEED_GROUPS = [
    { id: "country", title: "Country-specific", hint: "resolvable per country in the click-a-country inspector" },
    { id: "global", title: "Global", hint: "no country attribution" },
  ];

  function buildNav() {
    const nav = document.getElementById("feed-nav");
    nav.innerHTML = FEED_GROUPS.map((g) =>
      `<div class="feed-nav-group"><div class="feed-nav-head" title="${esc(g.hint)}">${esc(g.title)}</div>` +
      OST.FEED_CATS.filter((c) => (c.group || "global") === g.id).map((c) =>
        `<button class="co-row" data-fcat="${c.id}">` +
        `<span class="co-name">${healthDot(c)} ${esc(c.label)}</span>` +
        `<span class="co-meta"><span class="n" id="fc-n-${c.id}">…</span></span></button>`
      ).join("") + `</div>`
    ).join("");
    nav.onclick = (e) => {
      const b = e.target.closest("[data-fcat]");
      if (b) OST.openFeedCat(b.dataset.fcat);
    };
    // "show on map" cross-links reuse the layer-rail checkboxes
    document.getElementById("feeddrawer").addEventListener("change", (e) => {
      const id = e.target.dataset && e.target.dataset.maplink;
      if (!id) return;
      const rail = document.querySelector(`#map-layer-list input[data-layer="${id}"]`);
      if (rail) {
        rail.checked = e.target.checked;
        rail.dispatchEvent(new Event("change", { bubbles: true }));
      }
    });
  }

  function paintNav() {
    for (const c of OST.FEED_CATS) {
      const btn = document.querySelector(`#feed-nav [data-fcat="${c.id}"]`);
      if (!btn) continue;
      btn.classList.toggle("on", c.id === curCat);
      const name = btn.querySelector(".co-name");
      if (name) name.innerHTML = `${healthDot(c)} ${esc(c.label)}`;
      const n = document.getElementById(`fc-n-${c.id}`);
      if (n && c.count != null) n.textContent = Number(c.count).toLocaleString();
    }
  }

  /* ---------------- category view ---------------- */

  function mapToggleHTML(cat) {
    if (!cat.map) return "";
    const rail = document.querySelector(`#map-layer-list input[data-layer="${cat.map}"]`);
    const on = rail ? rail.checked : false;
    return `<label class="maplink" title="toggle the ${esc(cat.label)} map layer">` +
      `<input type="checkbox" data-maplink="${cat.map}" ${on ? "checked" : ""}/> show on map</label>`;
  }

  function renderCatHead(cat) {
    const h = cat.health ? health[cat.health] : null;
    const head = document.getElementById("feed-cathead");
    head.innerHTML =
      `<div class="row"><h3>${healthDot(cat)} ${esc(cat.label)}</h3>` +
      `<span class="spacer"></span>${mapToggleHTML(cat)}` +
      `<button id="feed-refresh" title="refresh now">⟳</button></div>` +
      (cat.note ? `<div class="m">${esc(cat.note)}</div>` : "") +
      (cat.staticNote ? `<div class="m">${esc(cat.staticNote)}</div>` : "") +
      (h && h.status !== "ok" && h.detail ? `<div class="m" style="color:var(--warn)">health: ${esc(h.status)} — ${esc(h.detail)}</div>` : "");
    document.getElementById("feed-refresh").onclick = () => loadCat(cat, true);
  }

  function renderInputs(cat) {
    const body = document.getElementById("feed-body");
    const inputs = (cat.inputs || []).map((i) => {
      if (i.type === "select") {
        const opts = i.options.map((o) =>
          `<option value="${esc(o)}" ${(lastParams[cat.id] && lastParams[cat.id][i.name] === o) || (!lastParams[cat.id] && i.value === o) ? "selected" : ""}>${esc(o)}</option>`).join("");
        return `<select data-fin="${i.name}">${opts}</select>`;
      }
      const prev = lastParams[cat.id] && lastParams[cat.id][i.name] != null ? lastParams[cat.id][i.name] : (i.value || "");
      return `<input data-fin="${i.name}" placeholder="${esc(i.placeholder || i.name)}" value="${esc(prev)}" spellcheck="false" />`;
    }).join("");
    body.innerHTML =
      (inputs ? `<div class="feed-tools">${inputs}<button id="feed-go">Load</button></div>` : "") +
      `<div id="feed-out"><div class="m">loading…</div></div>`;
    const go = () => loadCat(cat, true);
    const btn = document.getElementById("feed-go");
    if (btn) btn.onclick = go;
    body.querySelectorAll("input").forEach((inp) =>
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); }));
  }

  async function loadCat(cat, manual) {
    const out = document.getElementById("feed-out");
    const params = {};
    document.querySelectorAll("#feed-body [data-fin]").forEach((el) => { params[el.dataset.fin] = el.value.trim(); });
    if (cat.validate) {
      const err = cat.validate(params);
      if (err) { out.innerHTML = `<div class="errbox">${esc(err)}</div>`; return; }
    }
    lastParams[cat.id] = params;
    out.innerHTML = `<div class="m">loading…</div>`;
    try {
      const { data } = await OST.fetchJSON(cat.url(params), { timeout: 30000 });
      cat.render.call(cat, data, out);
    } catch (e) {
      cat.count = null;
      OST.showFetchError(out, e, cat.label);
    }
    paintNav();
  }

  OST.openFeedCat = function (id) {
    const cat = catById(id) || catById("flights");
    curCat = cat.id;
    if (!document.getElementById("feed-nav").dataset.built) {
      buildNav();
      document.getElementById("feed-nav").dataset.built = "1";
    }
    paintNav();
    renderCatHead(cat);
    renderInputs(cat);
    const needsInput = (cat.inputs || []).some((i) => i.required);
    if (!needsInput) loadCat(cat, false);
    else {
      const out = document.getElementById("feed-out");
      if (out) out.innerHTML = `<div class="empty">set parameters and press Load</div>`;
    }
    refreshHealth();
  };

  /* ---------------- health ---------------- */

  async function refreshHealth() {
    try {
      const { data } = await OST.fetchJSON("/api/health/feeds", { timeout: 20000 });
      health = data.feeds || {};
    } catch (_) { /* keep last known */ }
    paintNav();
    const cat = catById(curCat);
    if (cat && !document.getElementById("feeddrawer").classList.contains("hidden")) renderCatHead(cat);
  }

  /* ---------------- init ---------------- */

  OST.initFeeds = function () {
    if (!document.getElementById("feed-nav")) return;
    refreshTimer = setInterval(() => {
      if (document.getElementById("feeddrawer").classList.contains("hidden")) return;
      const cat = catById(curCat);
      if (!cat) return;
      const needsInput = (cat.inputs || []).some((i) => i.required);
      if (!needsInput || lastParams[cat.id]) {
        renderInputs(cat); // restore last params into the form
        loadCat(cat, false);
      }
    }, REFRESH_MS);
    healthTimer = setInterval(() => {
      if (!document.getElementById("feeddrawer").classList.contains("hidden")) refreshHealth();
    }, HEALTH_MS);
  };
})(window.OST);
