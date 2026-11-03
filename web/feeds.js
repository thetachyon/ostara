/* OSTARA feeds.js — dedicated Feeds panel: a structured browser for all 20 feed
 * categories, distinct from the SSE intel stream and the country inspector.
 * Category navigator with live counts + health dots, per-category inputs,
 * tables/cards/kv renderers, manual + auto refresh, and "show on map"
 * cross-links that flip the layer-rail toggles (reused, not reimplemented).
 * Loaded before app.js; registers on window.OST. No build step. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc;
  function trimNum(n) {
    if (n == null || isNaN(n)) return "—";
    return parseFloat(n.toFixed(8)).toString();
  }

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
      id: "fusion", label: "Fusion", health: "fusion",
      group: "global",
      note: "proprietary fused indicators — curated weights, transforms, and labels over public feeds · heuristic, not a forecast, not trading advice",
      inputs: [
        { name: "indicator", type: "select", options: ["all", "hormuz-energy-risk", "russia-supply-risk", "china-strait-tension", "us-macro-risk", "election-unrest-risk"], value: "all" },
      ],
      url: (p) => "/api/fusion" + (p.indicator && p.indicator !== "all" ? `?indicator=${encodeURIComponent(p.indicator)}` : ""),
      render(j, el) {
        const lvlColor = (l) => l === "critical" ? "var(--bad)" : l === "elevated" ? "var(--warn)"
          : l === "watch" ? "#ffb224" : l === "calm" ? "#46a758" : "var(--dim,#9aa4b2)";
        const confChip = (c) => `<span class="chip" style="border:1px solid var(--dim,#9aa4b2);border-radius:3px;padding:0 5px;font-size:10px" title="fraction of input weight that was live">confidence: ${esc(c || "—")}</span>`;
        const cards = (j.indicators || []).map((ind) => {
          const score = ind.score;
          const color = lvlColor(ind.level);
          const bar = score != null
            ? `<div style="height:8px;background:var(--panel,#1a2233);border-radius:4px;margin:6px 0"><div style="height:8px;width:${Math.max(0, Math.min(100, score))}%;background:${color};border-radius:4px"></div></div>`
            : `<div class="m" style="margin:6px 0">score withheld — no live inputs</div>`;
          const ev = (ind.evidence || []).map((e) =>
            `<div class="feed-item" style="${e.live ? "" : "opacity:0.65"}"><div>` +
            (e.live
              ? `<span class="chip" style="color:${color};border:1px solid ${color};border-radius:3px;padding:0 5px;font-size:10px">live</span>`
              : `<span class="chip" style="color:var(--bad);border:1px solid var(--bad);border-radius:3px;padding:0 5px;font-size:10px">degraded</span>`) +
            ` <b>${esc(e.input || "")}</b> <span class="m">weight ${Number(e.weight_used != null ? e.weight_used : 0).toFixed(2)}${e.live && e.weight !== e.weight_used ? ` (of ${Number(e.weight).toFixed(2)})` : ""}</span></div>` +
            `<div>${esc(e.value || "—")}</div>` +
            `<div class="m">${esc(e.source || "")}${e.role ? " · " + esc(e.role) : ""}</div>` +
            (e.detail ? `<div class="m">${esc(e.detail)}</div>` : "") + `</div>`
          ).join("");
          return `<div class="feed-item" style="border-left:3px solid ${color};padding-left:10px;margin-bottom:12px">` +
            `<div><span class="chip" style="color:${color};border:1px solid ${color};border-radius:3px;padding:0 5px;font-size:10px">${esc(ind.level || "")}</span> ` +
            `<span class="chip" style="border:1px solid var(--dim,#9aa4b2);border-radius:3px;padding:0 5px;font-size:10px">${esc(ind.desk || "")}</span> ` +
            `<b>${esc(ind.name || "")}</b> ${confChip(ind.confidence)} ` +
            `<span class="m">${esc(ind.live_inputs || "")} inputs live</span></div>` +
            (score != null ? `<div style="font-size:18px;font-weight:bold;color:${color}">${Number(score).toFixed(1)}</div>` : "") +
            bar +
            `<div style="margin:4px 0"><i>${esc(ind.label || "")}</i></div>` +
            `<div class="m" style="margin-bottom:4px">${esc(ind.description || "")}</div>` +
            `<details style="margin:4px 0"><summary class="m" style="cursor:pointer">evidence (${(ind.evidence || []).length})</summary><div style="margin-top:4px">${ev}</div></details>` +
            `<details style="margin:4px 0"><summary class="m" style="cursor:pointer">methodology</summary><div class="m" style="margin-top:4px">${esc(ind.methodology || "")}</div></details>` +
            `</div>`;
        }).join("") || `<div class="m">no indicators</div>`;
        el.innerHTML = cards +
          `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary>` +
          `<div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>` +
          genLine(j);
        this.count = (j.indicators || []).length;
      },
    },
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
      group: "country",
      note: "AISStream live AIS (websocket) — 503 until AIS_API_KEY is set",
      inputs: [
        { name: "limit", placeholder: "limit", value: "100" },
      ],
      url: (p) => `/api/vessels?limit=${encodeURIComponent(p.limit || "100")}`,
      render(j, el) {
        const rows = (j.vessels || []).slice(0, ROW_CAP).map((v) => [
          esc(v.name || String(v.mmsi || "?")),
          esc(String(v.mmsi || "—")),
          esc(v.vessel_type || "—"),
          v.speed_knots != null ? Math.round(v.speed_knots) + " kt" : "—",
          v.cog_deg != null ? Math.round(v.cog_deg) + "°" : "—",
          `${Number(v.latitude).toFixed(2)}, ${Number(v.longitude).toFixed(2)}`,
        ]);
        el.innerHTML = table(["Name", "MMSI", "Type", "Spd", "Course", "Coords"], rows) +
          `<div class="m" style="margin-top:6px">${(j.vessels || []).length} of ${j.count != null ? j.count : "?"} shown · live AIS snapshot (best-effort sampling)</div>` +
          genLine(j);
        this.count = (j.vessels || []).length;
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
      id: "prices", label: "Prices", health: "prices",
      group: "global",
      note: "delayed quotes \u2014 Yahoo Finance (free, keyless), ~15 min delay on most venues",
      inputs: [{ name: "symbols", placeholder: "AAPL, NVDA, EURUSD=X, CL=F, ^GSPC (blank = watchlist)" }],
      url: (p) => "/api/prices" + (p.symbols && p.symbols.trim() ? `?symbols=${encodeURIComponent(p.symbols.trim())}` : ""),
      render(j, el) {
        const spark = (vals) => {
          if (!vals || vals.length < 2) return "—";
          const w = 76, h = 22;
          const min = Math.min(...vals), max = Math.max(...vals), rng = (max - min) || 1;
          const pts = vals.map((v, i) =>
            `${(i / (vals.length - 1) * w).toFixed(1)},${(h - 2 - (v - min) / rng * (h - 4)).toFixed(1)}`
          ).join(" ");
          const col = vals[vals.length - 1] >= vals[0] ? "var(--good)" : "var(--bad)";
          return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:block">` +
            `<polyline points="${pts}" fill="none" stroke="${col}" stroke-width="1.5"/></svg>`;
        };
        const fmtP = (v) => v == null ? "—" : Number(v).toLocaleString(undefined, { maximumFractionDigits: 2 });
        const rows = (j.quotes || []).slice(0, ROW_CAP).map((q) => {
          const chg = q.change_pct != null ? Number(q.change_pct).toFixed(2) + "%" : "—";
          const cls = q.change_pct > 0 ? "color:var(--good)" : q.change_pct < 0 ? "color:var(--bad)" : "";
          const range = (q.day_low != null && q.day_high != null)
            ? `${fmtP(q.day_low)} – ${fmtP(q.day_high)}` : "—";
          return [
            `<b>${esc(q.symbol)}</b><br/><span style="color:var(--dim)">${esc(q.name || "")}</span>`,
            fmtP(q.price),
            `<span style="${cls}">${chg}</span>`,
            range,
            spark(q.spark),
            esc(q.date || ""),
          ];
        });
        const missing = (j.missing || []).map((m) =>
          `<div class="m">no data: <b>${esc(m.symbol)}</b> — ${esc(m.reason || "")}</div>`
        ).join("");
        el.innerHTML = table(["Symbol", "Price", "Chg %", "Day range", "Trend", "Date"], rows) +
          missing + genLine(j);
        this.count = (j.quotes || []).length;
      },
    },
    {
      id: "calendar", label: "Econ calendar", health: "calendar",
      group: "global",
      staticNote: "curated registry — 56 events: FOMC, ECB, BoE, BoJ, US CPI, US NFP, OPEC+, G20, elections",
      note: "curated registry, not a live feed — verify against official calendars before trading",
      inputs: [
        { name: "kind", type: "select", options: ["all", "rates", "inflation", "employment", "energy", "elections", "summit"], value: "all" },
      ],
      url: (p) => "/api/calendar" + (p.kind && p.kind !== "all" ? `?kind=${encodeURIComponent(p.kind)}` : ""),
      render(j, el) {
        const chip = (d) => {
          let label, style;
          if (d === 0) { label = "Today"; style = "color:var(--bad);border-color:var(--bad)"; }
          else if (d < 7) { label = `${d}d`; style = "color:var(--warn);border-color:var(--warn)"; }
          else { label = `${Math.floor(d / 7)}w`; style = "color:var(--dim)"; }
          return `<span class="chip" style="border:1px solid;border-radius:3px;padding:0 5px;font-size:10px;white-space:nowrap;${style}">${label}</span>`;
        };
        const impBadge = (i) => i === "high"
          ? `<span class="chip" style="color:var(--bad);border:1px solid var(--bad);border-radius:3px;padding:0 5px;font-size:10px">high</span>`
          : `<span class="chip" style="color:var(--dim);border:1px solid var(--dim);border-radius:3px;padding:0 5px;font-size:10px">medium</span>`;
        const confTag = (c) => {
          const short = { "published-schedule": "published", "regular-pattern": "pattern", "announced": "announced", "scheduled": "scheduled" }[c] || c;
          return `<span class="m" title="schedule confidence: ${esc(c || "")}">${esc(short)}</span>`;
        };
        const rows = (j.events || []).slice(0, ROW_CAP).map((e) => [
          chip(e.days_until),
          `${esc(e.date || "—")}${e.time_utc ? `<br/><span class="m">${esc(e.time_utc)} UTC</span>` : ""}`,
          `<b>${esc(e.name || "—")}</b><br/><span class="m">${esc(e.note || "")}</span>`,
          esc(e.country_org || "—"),
          esc(e.kind || "—"),
          impBadge(e.importance),
          confTag(e.confidence),
        ]);
        el.innerHTML = table(["In", "Date", "Event", "Org", "Kind", "Imp.", "Sched."], rows) +
          `<div class="m" style="margin-top:6px">${(j.events || []).length} shown · ${esc(j.source || "")}</div>` + genLine(j);
        this.count = (j.events || []).length;
      },
    },
    {
      id: "earnings", label: "Earnings", health: "earnings",
      group: "global",
      note: "Nasdaq earnings calendar (keyless) — date-indexed only; no revenue estimates or per-symbol lookup from upstream",
      inputs: [{ name: "date", placeholder: "YYYY-MM-DD (blank = today)" }],
      validate: (p) => {
        const d = (p.date || "").trim();
        return d && !/^\d{4}-\d{2}-\d{2}$/.test(d) ? "date must be YYYY-MM-DD" : null;
      },
      url: (p) => "/api/earnings" + (p.date && p.date.trim() ? `?date=${encodeURIComponent(p.date.trim())}` : ""),
      render(j, el) {
        const timeBadge = (t) => {
          const map = { bmo: "BMO", amc: "AMC", dmh: "DMH" };
          const col = t === "bmo" ? "var(--good)" : t === "amc" ? "var(--warn)" : "var(--dim)";
          return `<span class="chip" style="color:${col};border:1px solid ${col};border-radius:3px;padding:0 5px;font-size:10px;font-weight:700">${esc(map[t] || t || "—")}</span>`;
        };
        const eps = (v) => v == null ? "—" : "$" + Number(v).toFixed(2);
        const cap = (v) => v == null ? "—" : "$" + (v / 1e9).toFixed(1) + "B";
        const items = j.earnings || [];
        const rows = items.slice(0, ROW_CAP).map((e) => [
          `<b>${esc(e.symbol)}</b>`,
          esc(e.name || "—"),
          timeBadge(e.time),
          eps(e.eps_estimate),
          cap(e.market_cap),
          esc(e.fiscal_quarter_ending || "—"),
          e.num_estimates != null ? e.num_estimates : "—",
        ]);
        const body = items.length === 0
          ? `<div class="empty">no earnings scheduled for ${esc(j.date || "")} — markets may be closed</div>`
          : table(["Symbol", "Company", "Time", "EPS est.", "Mkt cap", "Fiscal Q", "# ests"], rows);
        el.innerHTML = body +
          `<div class="m" style="margin-top:6px">${items.length} shown · ${esc(j.source || "")}</div>` + genLine(j);
        this.count = items.length;
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
      note: "public channels only \u00b7 keyless \u00b7 no login",
      inputs: [{ name: "channel", placeholder: "channel (blank = all 9 default news channels)" }],
      url: (p) => {
        const c = (p.channel || "").trim();
        return c ? `/api/telegram?channel=${encodeURIComponent(c)}` : `/api/telegram?channel=all`;
      },
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
      id: "bluesky", label: "Bluesky", health: "bluesky",
      group: "global",
      note: "keyless · public posts via api.bsky.app",
      inputs: [
        { name: "q", placeholder: "search query (blank = risk-keyword scan)" },
        { name: "limit", placeholder: "limit", value: "25" },
      ],
      url: (p) => {
        const q = (p.q || "").trim() || "murder OR shooting OR bombing OR stabbing OR riot OR kidnapping OR airstrike";
        return `/api/bluesky?q=${encodeURIComponent(q)}&limit=${encodeURIComponent(p.limit || "25")}`;
      },
      render(j, el) {
        const rows = (j.posts || []).slice(0, ROW_CAP).map((p) => [
          esc("@" + (p.author_handle || "?")),
          esc((p.text || "").slice(0, 120)) + ((p.text || "").length > 120 ? "…" : ""),
          p.like_count != null ? p.like_count : "—",
          p.repost_count != null ? p.repost_count : "—",
          p.reply_count != null ? p.reply_count : "—",
          esc((p.created_at || "").slice(0, 16).replace("T", " ")),
        ]);
        el.innerHTML = table(["Author", "Text", "Likes", "Reposts", "Replies", "Time"], rows) +
          `<div class="m" style="margin-top:6px">${(j.posts || []).length} posts · q=${esc(j.query || "")}</div>` + genLine(j);
        this.count = (j.posts || []).length;
      },
    },
    {
      id: "mastodon", label: "Mastodon", health: "mastodon",
      group: "global",
      note: "fosstodon.org public timeline · keyless · q filters the snapshot client-side",
      inputs: [
        { name: "q", placeholder: "filter text (optional)" },
        { name: "limit", placeholder: "limit", value: "20" },
      ],
      url: (p) => `/api/mastodon?limit=${encodeURIComponent(p.limit || "20")}` +
        (p.q ? `&q=${encodeURIComponent(p.q)}` : ""),
      render(j, el) {
        const rows = (j.posts || []).slice(0, ROW_CAP).map((p) => [
          esc("@" + (p.account || "?")),
          esc((p.text || "").slice(0, 120)) + ((p.text || "").length > 120 ? "…" : ""),
          p.reblogs != null ? p.reblogs : "—",
          p.favourites != null ? p.favourites : "—",
          esc((p.created_at || "").slice(0, 16).replace("T", " ")),
        ]);
        el.innerHTML = table(["Account", "Text", "Boosts", "Faves", "Time"], rows) +
          `<div class="m" style="margin-top:6px">${(j.posts || []).length} of ${j.fetched != null ? j.fetched : "?"} fetched${j.query ? ` · filter=${esc(j.query)}` : ""}</div>` + genLine(j);
        this.count = (j.posts || []).length;
      },
    },
    {
      id: "rss", label: "RSS News", health: "rss",
      group: "global",
      note: "curated outlets · keyless (Reuters/AP have no public RSS — excluded)",
      inputs: [
        { name: "outlet", type: "select", options: ["all", "bbc", "aljazeera", "dw", "france24"], value: "all" },
        { name: "limit", placeholder: "limit", value: "25" },
      ],
      url: (p) => `/api/rss?outlet=${encodeURIComponent(p.outlet || "all")}&limit=${encodeURIComponent(p.limit || "25")}`,
      render(j, el) {
        const items = j.items || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} item${items.length === 1 ? "" : "s"} · ${esc(j.outlet || "")}</div>` +
          items.slice(0, ROW_CAP).map((i) =>
            `<div class="feed-item"><div><a href="${esc(i.link)}" target="_blank" rel="noopener">${esc(i.title)}</a></div>` +
            `<div class="m">${esc(i.outlet || "")} · ${esc((i.published || "").slice(0, 16).replace("T", " "))}</div>` +
            (i.summary ? `<div>${esc(i.summary.slice(0, 200))}${i.summary.length > 200 ? "…" : ""}</div>` : "") + `</div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "gnews", label: "Google News", health: "gnews",
      group: "global",
      note: "keyless RSS · q is country-queryable (e.g. protest France)",
      inputs: [
        { name: "q", placeholder: "search query (blank = risk-keyword scan)" },
        { name: "limit", placeholder: "limit", value: "25" },
      ],
      validate: (p) => (p.q && p.q.trim() ? null : "enter a search query"),
      url: (p) => `/api/gnews?q=${encodeURIComponent(p.q || "")}&limit=${encodeURIComponent(p.limit || "25")}`,
      render(j, el) {
        const items = j.items || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} item${items.length === 1 ? "" : "s"} · q=${esc(j.query || "")}</div>` +
          items.slice(0, ROW_CAP).map((i) =>
            `<div class="feed-item"><div><a href="${esc(i.link)}" target="_blank" rel="noopener">${esc(i.title)}</a></div>` +
            `<div class="m">${esc((i.published || "").slice(0, 16).replace("T", " "))}</div>` +
            (i.summary ? `<div>${esc(i.summary.slice(0, 200))}${i.summary.length > 200 ? "…" : ""}</div>` : "") + `</div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "whatsapp", label: "WhatsApp", health: null,
      group: "global",
      note: "directory of public channel invite links \u2014 not live messages (WhatsApp has no public read API)",
      inputs: [
        { name: "country", placeholder: "country (name or ISO)", value: "" },
        { name: "category", placeholder: "category", value: "" },
      ],
      url: (p) => `/api/whatsapp-channels?country=${encodeURIComponent(p.country || "")}&category=${encodeURIComponent(p.category || "")}`,
      render(j, el) {
        const items = j.channels || [];
        el.innerHTML = `<div class="m" style="margin-bottom:6px">${items.length} channel${items.length === 1 ? "" : "s"}</div>` +
          items.slice(0, ROW_CAP).map((c) =>
            `<div class="feed-item"><div><a href="${esc(c.invite_url)}" target="_blank" rel="noopener">${esc(c.name)}</a>${c.verified ? " \u2713" : ""}</div>` +
            `<div class="m">${esc(c.category || "")} \u00b7 ${esc(c.country || "")}${c.language ? " \u00b7 " + esc(c.language) : ""}</div></div>`
          ).join("") + genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "threats", label: "Threat scan", health: null,
      group: "global",
      note: "keyword-flagged public mentions of political figures · direction defaults to threats against the figure · requires human review · not verified threats",
      inputs: [
        { name: "name", type: "datalist", listId: "threat-figures", placeholder: "figure name", required: true },
        { name: "severity", type: "select", options: ["all", "critical", "high", "medium"], value: "all" },
        { name: "direction", type: "select", options: ["against", "by", "unclear", "all"], value: "against" },
      ],
      validate: (p) => (p.name && p.name.trim() ? null : "enter a figure name"),
      url: (p) => `/api/threats/scan?name=${encodeURIComponent(p.name || "")}&severity=${encodeURIComponent(p.severity || "all")}&direction=${encodeURIComponent(p.direction || "against")}`,
      afterInputs(body) {
        if (body.dataset.threatListLoaded) return;
        body.dataset.threatListLoaded = "1";
        OST.fetchJSON("/api/threats/politicians", { timeout: 20000 }).then(({ data }) => {
          const dl = document.getElementById("threat-figures");
          if (dl && data.figures) dl.innerHTML = data.figures.map((f) =>
            `<option value="${esc(f.name)}">${esc(f.role || "")} · ${esc(f.country || "")}</option>`).join("");
        }).catch(() => { /* datalist stays empty; validation still works */ });
      },
      render(j, el) {
        const items = j.items || [];
        const accounts = j.accounts || [];
        const sevStyle = (s) => s === "critical"
          ? "color:var(--bad);border-color:var(--bad)"
          : s === "high" ? "color:var(--warn);border-color:var(--warn)" : "color:var(--accent,#7aa2f7);border-color:var(--accent,#7aa2f7)";
        const srcs = (j.sources || []).map((s) =>
          `${esc(s.source)}: ${s.ok ? "ok" : "unavailable" + (s.detail ? " — " + esc(s.detail) : "")}`).join(" · ");
        const fig = j.figure || {};
        const platBadge = (p) => p === "telegram"
          ? `<span class="chip" style="color:#5ac8fa;border:1px solid #5ac8fa;border-radius:3px;padding:0 5px;font-size:10px">telegram</span>`
          : `<span class="chip" style="color:#4c9aff;border:1px solid #4c9aff;border-radius:3px;padding:0 5px;font-size:10px">bluesky</span>`;
        const renderMentions = () => items.slice(0, ROW_CAP).map((it) =>
            `<div class="feed-item"><div><span class="chip" style="${sevStyle(it.severity)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(it.severity || "")}</span> ` +
            (it.direction ? `<span class="chip" style="color:var(--dim,#9aa4b2);border:1px solid var(--dim,#9aa4b2);border-radius:3px;padding:0 5px;font-size:10px">${esc(it.direction)}</span> ` : "") +
            `<a href="${esc(it.url || "#")}" target="_blank" rel="noopener">${esc(it.title || "(untitled)")}</a></div>` +
            `<div class="m">${esc(it.source || "")} · ${esc((it.published || "").slice(0, 16).replace("T", " "))}</div>` +
            (it.text_snippet ? `<div>${esc(it.text_snippet.slice(0, 260))}${it.text_snippet.length > 260 ? "…" : ""}</div>` : "") +
            (it.matched_keywords && it.matched_keywords.length ? `<div class="m">matched: ${it.matched_keywords.map(esc).join(", ")}</div>` : "") +
            `</div>`
          ).join("") || `<div class="m">no flagged mentions</div>`;
        const renderAccounts = () => accounts.map((a) => {
            const counts = [a.severities && a.severities.critical ? `<span class="chip" style="${sevStyle("critical")};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">critical ${a.severities.critical}</span>` : "",
              a.severities && a.severities.high ? `<span class="chip" style="${sevStyle("high")};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">high ${a.severities.high}</span>` : "",
              a.severities && a.severities.medium ? `<span class="chip" style="${sevStyle("medium")};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">medium ${a.severities.medium}</span>` : ""]
              .filter(Boolean).join(" ");
            const samples = (a.sample || []).map((s) =>
              `<div style="margin-top:4px"><span class="chip" style="${sevStyle(s.severity)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(s.severity || "")}</span> ` +
              (s.post_url ? `<a href="${esc(s.post_url)}" target="_blank" rel="noopener">${esc(s.title || "(untitled)")}</a>` : esc(s.title || "(untitled)")) +
              (s.published ? ` <span class="m">${esc(s.published.slice(0, 16).replace("T", " "))}</span>` : "") + `</div>`).join("");
            return `<div class="feed-item"><div>${platBadge(a.platform)} ` +
              (a.profile_url ? `<a href="${esc(a.profile_url)}" target="_blank" rel="noopener"><b>@${esc(a.handle || "")}</b></a>` : `<b>@${esc(a.handle || "")}</b>`) +
              (a.display_name ? ` <span class="m">${esc(a.display_name)}</span>` : "") + `</div>` +
              `<div class="m" style="margin-top:3px"><span class="chip" style="${sevStyle(a.max_severity)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">max ${esc(a.max_severity || "")}</span> ` +
              `${a.post_count} flagged post${a.post_count === 1 ? "" : "s"}${counts ? " · " + counts : ""}</div>` +
              (samples ? `<details style="margin-top:4px"><summary class="m" style="cursor:pointer">sample posts</summary>${samples}</details>` : "") +
              `</div>`;
          }).join("") || `<div class="m">no accounts — no social posts matched, or social sources are unavailable</div>`;
        const list = document.createElement("div");
        const draw = (mode) => {
          const mBtn = (m, label, n) =>
            `<button data-tmode="${m}" style="margin-right:6px;${mode === m ? "font-weight:bold;text-decoration:underline" : ""}">${label} (${n})</button>`;
          list.innerHTML =
            `<div class="m" style="margin-bottom:8px">` +
            mBtn("mentions", "Mentions", items.length) + mBtn("accounts", "Accounts", accounts.length) +
            ` <span title="Accounts whose public posts matched threat keywords — includes quoting, condemnation, figurative use. Not a list of threat actors.">ⓘ</span></div>` +
            (mode === "accounts" ? renderAccounts() : renderMentions()) + genLine(j);
          list.querySelectorAll("[data-tmode]").forEach((b) =>
            b.addEventListener("click", () => draw(b.dataset.tmode)));
        };
        el.innerHTML =
          `<div class="m" style="margin-bottom:4px"><b>${esc(fig.name || "")}</b>${fig.role ? " · " + esc(fig.role) : ""}${fig.country ? " · " + esc(fig.country) : ""}</div>` +
          `<div class="m" style="margin-bottom:6px">${items.length} flagged item${items.length === 1 ? "" : "s"} · ${esc(srcs)}</div>` +
          `<details style="margin-bottom:8px"><summary class="m" style="cursor:pointer">methodology + disclaimer</summary>` +
          `<div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>`;
        el.appendChild(list);
        draw("mentions");
        this.count = items.length;
      },
    },
    {
      id: "sanctions", label: "Sanctions", health: null,
      group: "global",
      note: "keyword-flagged sanctions/tariff/export-control mentions by country · not verified designations · verify against OFAC/EU lists before trading",
      inputs: [
        { name: "country", placeholder: "country (default: all 15)" },
        { name: "severity", type: "select", options: ["all", "critical", "high", "medium"], value: "all" },
      ],
      url: (p) => `/api/sanctions?country=${encodeURIComponent((p.country || "all").trim() || "all")}&severity=${encodeURIComponent(p.severity || "all")}`,
      render(j, el) {
        const items = j.items || [];
        const countries = j.countries || [];
        const sevStyle = (s) => s === "critical"
          ? "color:var(--bad);border-color:var(--bad)"
          : s === "high" ? "color:var(--warn);border-color:var(--warn)" : "color:var(--accent,#7aa2f7);border-color:var(--accent,#7aa2f7)";
        const srcs = (j.sources || []).map((s) =>
          `${esc(s.source)}: ${s.ok ? "ok" : "unavailable" + (s.detail ? " — " + esc(s.detail) : "")}`).join(" · ");
        const cards = items.slice(0, ROW_CAP).map((it) =>
            `<div class="feed-item"><div><span class="chip" style="${sevStyle(it.severity)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(it.severity || "")}</span> ` +
            `<b>${esc(it.country || "")}</b> ` +
            `<a href="${esc(it.url || "#")}" target="_blank" rel="noopener">${esc(it.title || "(untitled)")}</a></div>` +
            `<div class="m">${esc(it.source || "")} · ${esc((it.published || "").slice(0, 16).replace("T", " "))}</div>` +
            (it.text_snippet ? `<div>${esc(it.text_snippet.slice(0, 260))}${it.text_snippet.length > 260 ? "…" : ""}</div>` : "") +
            (it.matched_keywords && it.matched_keywords.length ? `<div class="m">matched: ${it.matched_keywords.map(esc).join(", ")}</div>` : "") +
            `</div>`
          ).join("") || `<div class="m">no flagged mentions</div>`;
        el.innerHTML =
          `<div class="m" style="margin-bottom:4px"><b>${esc(j.mode === "country" ? (j.country || "") : "all 15 watchlist countries")}</b></div>` +
          `<div class="m" style="margin-bottom:6px">${items.length} flagged item${items.length === 1 ? "" : "s"} · ${esc(srcs)}</div>` +
          (countries.length > 1 ? `<div class="m" style="margin-bottom:8px">` + countries.map((c) =>
            `<span class="chip" style="border:1px solid var(--dim,#9aa4b2);border-radius:3px;padding:0 5px;font-size:10px;margin:0 4px 4px 0;display:inline-block">${esc(c.country)} ${c.count}` +
            (c.critical ? ` · <b style="${sevStyle("critical")}">crit ${c.critical}</b>` : "") +
            (c.high ? ` · <b style="${sevStyle("high")}">high ${c.high}</b>` : "") + `</span>`).join("") + `</div>` : "") +
          cards +
          `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">methodology + disclaimer</summary>` +
          `<div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>` +
          genLine(j);
        this.count = items.length;
      },
    },
    {
      id: "threatposters", label: "Threat posters", health: null,
      group: "global",
      note: "accounts posting threat language directed at someone · targets extracted heuristically · keyword heuristic, no account accused of anything",
      inputs: [
        { name: "limit", placeholder: "limit", value: "20" },
      ],
      url: (p) => `/api/scan/threat-posters?limit=${encodeURIComponent(p.limit || "20")}`,
      render(j, el) {
        const srcs = (j.sources || []).map((s) =>
          `${esc(s.source)}: ${s.ok ? "ok" : "down" + (s.detail ? " — " + esc(s.detail) : "")}`).join(" · ");
        const cards = (j.posters || []).slice(0, ROW_CAP).map((t) =>
          `<div class="feed-item" style="border-left:3px solid var(--bad);padding-left:10px;margin-bottom:10px">` +
          `<div><b><a href="${esc(t.author_url || "#")}" target="_blank" rel="noopener">${esc(t.author || "?")}</a></b> ` +
          `<span class="m">${esc(t.source || "")} · ${t.threat_post_count} threat post${t.threat_post_count === 1 ? "" : "s"} · max score ${Number(t.max_score).toFixed(0)}</span></div>` +
          (t.targets && t.targets.length ? `<div>targets: <b>${t.targets.map(esc).join(", ")}</b> <span class="m">(heuristic)</span></div>` : `<div class="m">no target extracted</div>`) +
          (t.example_posts || []).slice(0, 2).map((p) =>
            `<div class="m" style="margin-top:4px">“${esc((p.text || "").slice(0, 180))}${(p.text || "").length > 180 ? "…" : ""}”` +
            (p.post_url ? ` <a href="${esc(p.post_url)}" target="_blank" rel="noopener">open</a>` : "") + `</div>`
          ).join("") +
          `</div>`
        ).join("") || `<div class="m">no threat posters found in this scan</div>`;
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px">${j.poster_count} posters · ${esc(srcs)}</div>` +
          cards +
          `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary><div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>` +
          genLine(j);
        this.count = j.poster_count;
      },
    },
    {
      id: "riskyposts", label: "Risky posts", health: null,
      group: "global",
      note: "posts ranked by risk score (highest first) from Telegram defaults & Bluesky · keyword heuristic, not verified threats · no poster is accused of anything",
      inputs: [
        { name: "limit", placeholder: "limit", value: "10" },
        { name: "min_score", placeholder: "min score 0-100 (blank = show all, ranked)", value: "" },
      ],
      url: (p) => `/api/scan/risky?limit=${encodeURIComponent(p.limit || "10")}&min_score=${encodeURIComponent((p.min_score || "").trim() === "" ? "0" : p.min_score)}`,
      render(j, el) {
        const lvlColor = (l) => l === "severe" ? "var(--bad)" : l === "high" ? "var(--warn)"
          : l === "moderate" ? "#ffb224" : "var(--dim,#9aa4b2)";
        const srcs = (j.sources || []).map((s) =>
          `${esc(s.source)}: ${s.ok ? "ok" : "down" + (s.detail ? " — " + esc(s.detail) : "")}`).join(" · ");
        const postCards = (j.posts || []).slice(0, ROW_CAP).map((p) =>
          `<div class="feed-item" style="border-left:3px solid ${lvlColor(p.risk.level)};padding-left:10px;margin-bottom:10px">` +
          `<div><span class="chip" style="color:${lvlColor(p.risk.level)};border:1px solid ${lvlColor(p.risk.level)};border-radius:3px;padding:0 5px;font-size:10px">${esc(p.risk.level || "")} ${Number(p.risk.score).toFixed(0)}</span> ` +
          `<b>${esc(p.author || "")}</b> <span class="m">${esc(p.source || "")} · ${esc((p.published || "").slice(0, 16).replace("T", " "))}</span></div>` +
          `<div>${esc(p.text || "")}</div>` +
          (p.risk.matched_keywords && p.risk.matched_keywords.length ? `<div class="m">matched: ${p.risk.matched_keywords.map(esc).join(", ")}</div>` : "") +
          (p.post_url ? `<div class="m"><a href="${esc(p.post_url)}" target="_blank" rel="noopener">open post</a></div>` : "") +
          `</div>`
        ).join("") || `<div class="m">no posts scored above the threshold</div>`;
        const posterRows = (j.posters || []).slice(0, ROW_CAP).map((t) => [
          `<a href="${esc(t.author_url || "#")}" target="_blank" rel="noopener">${esc(t.author || "?")}</a>`,
          esc(t.source || ""),
          t.risky_post_count + " / " + t.post_count,
          Number(t.max_score).toFixed(0),
          Number(t.avg_score).toFixed(0),
          esc((t.top_keywords || []).join(", ")),
        ]);
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px">${j.post_count} risky posts · ${j.poster_count} posters · ${esc(srcs)}</div>` +
          `<h4 style="margin:10px 0 6px">Top posts</h4>` + postCards +
          `<h4 style="margin:14px 0 6px">Top posters</h4>` +
          (posterRows.length ? table(["Poster", "Source", "Risky/Total", "Max", "Avg", "Top keywords"], posterRows) : `<div class="m">no posters above threshold</div>`) +
          `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary><div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>` +
          genLine(j);
        this.count = j.post_count;
      },
    },
    {
      id: "instability", label: "Instability index", health: null,
      group: "country-specific",
      note: "per-country instability score + civil unrest probability · structural baselines (food, trust, economy) + live media signal · heuristic, not a forecast",
      inputs: [
        { name: "country", placeholder: "country name or ISO code", value: "" },
      ],
      url: (p) => `/api/instability?country=${encodeURIComponent((p.country || "").trim())}`,
      validate: (p) => (p.country || "").trim() === "" ? "enter a country" : null,
      render(j, el) {
        const discl = `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary><div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>`;
        const lvlColor = (l) => l === "severe" ? "var(--bad)" : l === "high" ? "#f5a623" : l === "moderate" ? "#e3b341" : "#46a758";
        const bar = (score) => {
          const c = lvlColor(score < 25 ? "low" : score < 50 ? "moderate" : score < 75 ? "high" : "severe");
          return `<div style="background:#1a2332;border-radius:3px;height:8px;margin:4px 0"><div style="background:${c};height:8px;border-radius:3px;width:${Math.min(100, Math.max(0, score))}%"></div></div>`;
        };
        const comps = (j.components || []).map((c) =>
          c.score < 0 ? "" :
          `<div class="feed-item"><div><b>${esc(c.name.replace(/_/g, " "))}</b> <span style="color:${lvlColor(c.score < 25 ? "low" : c.score < 50 ? "moderate" : c.score < 75 ? "high" : "severe")}">${c.score.toFixed(1)}</span> <span class="m">weight ${(c.weight * 100).toFixed(0)}%</span></div>${bar(c.score)}<div class="m">${esc(c.detail || "")}</div></div>`
        ).join("");
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px"><b>${esc(j.country || "")}</b> <span class="m">(${esc(j.iso || "")})</span> <span class="m">structural confidence: ${esc(j.structural_confidence || "—")}</span></div>` +
          `<div class="feed-item"><div><b>Instability score</b> <span style="color:${lvlColor(j.instability_level)};font-size:18px"><b>${(j.instability_score || 0).toFixed(1)}</b></span> <span class="chip" style="color:${lvlColor(j.instability_level)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(j.instability_level || "")}</span></div>${bar(j.instability_score || 0)}</div>` +
          `<div class="feed-item"><div><b>Civil unrest probability</b> <span style="color:${lvlColor(j.unrest_level)};font-size:18px"><b>${(j.unrest_probability || 0).toFixed(1)}%</b></span> <span class="chip" style="color:${lvlColor(j.unrest_level)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(j.unrest_level || "")}</span></div>${bar(j.unrest_probability || 0)}<div class="m">${esc(j.live_signal_detail || "")}</div></div>` +
          `<div class="m" style="margin:8px 0 4px"><b>Components</b></div>` + comps + discl + genLine(j);
        this.count = (j.components || []).length;
      }
    },
    {
      id: "crypto", label: "Crypto transactions", health: null,
      group: "global",
      note: "recent on-chain transactions across BTC, ETH, LTC, DOGE · keyless BlockCypher feed · values as reported on-chain",
      inputs: [
        { name: "currency", type: "select", options: ["all", "btc", "eth", "ltc", "doge"], value: "all" },
        { name: "limit", placeholder: "limit", value: "10" },
        { name: "min_risk", placeholder: "min risk 0-100 (blank = all)", value: "" },
      ],
      url: (p) => {
        const mr = (p.min_risk || "").trim();
        return `/api/crypto/transactions?currency=${encodeURIComponent(p.currency || "all")}&limit=${encodeURIComponent(p.limit || "10")}${mr === "" ? "" : "&min_risk=" + encodeURIComponent(mr)}`;
      },
      render(j, el) {
        const discl = `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary><div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>`;
        if (!j.source_ok) {
          el.innerHTML = `<div class="m">source unavailable: ${esc(j.source_detail || "")}</div>` + discl;
          this.count = 0;
          return;
        }
        const riskColor = (l) => l === "severe" ? "var(--bad)" : l === "high" ? "#f5a623" : l === "moderate" ? "#e3b341" : "#46a758";
        const card = (t) => {
          const rs = t.risk_score != null ? t.risk_score : 0, rl = t.risk_level || "low";
          const badge = `<span class="chip" style="color:${riskColor(rl)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px;margin-left:6px">risk ${rs} ${esc(rl)}</span>`;
          const reasons = (t.risk_reasons || []).length
            ? `<div class="m" style="color:${riskColor(rl)}">${esc(t.risk_reasons.join(" · "))}</div>`
            : "";
          const partyLine = (p, valSuffix) => {
            const lbl = p.label ? ` <b style="color:var(--accent)">${esc(p.label)}</b>` : "";
            const chg = p.likely_change ? ` <span class="m" style="font-style:italic">(likely change)</span>` : "";
            return `<div class="m" title="${esc(p.address_full || "")}">${mono(esc(p.address || ""))}${lbl}${valSuffix || ""}${chg}</div>`;
          };
          const sendersHtml = (t.senders || []).length
            ? `<div class="m" style="margin-top:4px"><b>From</b> <span class="m">(sender)</span></div>` +
              (t.senders || []).slice(0, 4).map((p) => partyLine(p)).join("")
            : "";
          const recipientsHtml = (t.recipients || []).length
            ? `<div class="m" style="margin-top:4px"><b>To</b> <span class="m">(recipient)</span></div>` +
              (t.recipients || []).slice(0, 6).map((p) =>
                partyLine(p, p.value != null ? ` <span class="m">— ${esc(trimNum(p.value))} ${esc(t.ticker)}</span>` : "")
              ).join("")
            : "";
          return `<div class="feed-item"${rs >= 50 ? ` style="border-left:2px solid ${riskColor(rl)};padding-left:8px"` : ""}><div><b>${esc(t.ticker)}</b> · ${esc(trimNum(t.value))} <span class="m">${esc(t.currency)}</span>${badge}</div>` +
          `<div class="m">fee: ${esc(trimNum(t.fee))} · confirmations: ${t.confirmations} · ${esc((t.received_at || "").slice(0, 19).replace("T", " "))}</div>` +
          reasons + sendersHtml + recipientsHtml +
          `<div class="m">${mono((t.hash || "").slice(0, 20))}…</div>` +
          `</div>`;
        };
        const txs = (j.transactions || []).slice(0, ROW_CAP);
        const byRecent = (a, b) => (b.received_at || "").localeCompare(a.received_at || "");
        const elevated = txs.filter((t) => (t.risk_score || 0) >= 50).sort(byRecent);
        const elevatedHtml = elevated.length
          ? `<div class="m" style="margin:8px 0 4px"><b style="color:var(--bad)">⚠ Elevated risk — most recent</b> <span class="chip" style="border:1px solid var(--bad);border-radius:3px;padding:0 5px;font-size:10px;color:var(--bad)">${elevated.length}</span></div>` +
            elevated.map(card).join("")
          : `<div class="m" style="margin:8px 0 4px"><b style="color:#46a758">✓ No elevated-risk transactions</b> <span class="m">in this batch</span></div>`;
        const allHtml = `<div class="m" style="margin:10px 0 4px"><b>All transactions</b> <span class="m">(${txs.length})</span></div>` +
          (txs.map(card).join("") || `<div class="m">no transactions</div>`);
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px"><b>${esc(j.currency || "")}</b> · ${j.count} transaction${j.count === 1 ? "" : "s"}</div>` +
          elevatedHtml + allHtml + discl + genLine(j);
        this.count = j.count;
      }
    },
    {
      id: "darkweb", label: "Dark web", health: null,
      group: "global",
      note: "onion search (Ahmia) + ransomware leak-site victim claims · operator claims, not verified breaches · no Tor, never touches stolen data",
      inputs: [
        { name: "mode", type: "select", options: ["overview", "victims", "onion search"], value: "overview" },
        { name: "q", placeholder: "query (victim name or onion search)" },
        { name: "country", placeholder: "ISO2 (victims mode)" },
      ],
      url: (p) => {
        const mode = p.mode || "overview";
        if (mode === "onion search") {
          const q = (p.q || "").trim();
          return q ? `/api/darkweb/search?q=${encodeURIComponent(q)}` : `/api/darkweb`;
        }
        if (mode === "victims") {
          const q = (p.q || "").trim(), c = (p.country || "").trim();
          if (q) return `/api/darkweb/ransomware/victims?q=${encodeURIComponent(q)}`;
          if (c) return `/api/darkweb/ransomware/victims?country=${encodeURIComponent(c)}`;
          return `/api/darkweb/ransomware/victims`;
        }
        return `/api/darkweb`;
      },
      render(j, el) {
        const discl = `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">disclaimer</summary><div class="m" style="margin-top:4px">${esc(j.disclaimer || "")}</div></details>`;
        // Overview shape
        if (j.sources && !j.claims && !j.results) {
          const srcs = j.sources.map((s) =>
            `<div class="feed-item"><div><span class="chip" style="color:${s.ok ? "#46a758" : "var(--bad)"};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${s.ok ? "live" : "down"}</span> <b>${esc(s.name)}</b></div><div class="m">${esc(s.detail || "")}</div></div>`).join("");
          el.innerHTML =
            `<div class="m" style="margin-bottom:6px">ransomware groups tracked: <b>${j.ransomware_groups != null ? j.ransomware_groups : "—"}</b> · victim claims indexed: <b>${j.ransomware_victims_total != null ? j.ransomware_victims_total : "—"}</b></div>` +
            `<div class="m" style="margin-bottom:6px">${esc(j.note || "")}</div>` + srcs + discl + genLine(j);
          this.count = j.sources.length;
          return;
        }
        // Victim claims shape
        if (j.claims) {
          const cards = j.claims.slice(0, ROW_CAP).map((c) =>
            `<div class="feed-item"><div><b>${esc(c.victim || "(unnamed)")}</b></div>` +
            `<div class="m">group: ${esc(c.group || "—")} · country: ${esc(c.country || "—")} · discovered: ${esc((c.discovered || "").slice(0, 10))}</div>` +
            (c.description ? `<div>${esc(c.description)}</div>` : "") +
            (c.claim_url ? `<div class="m">${mono(c.claim_url)}</div>` : "") + `</div>`
          ).join("") || `<div class="m">no victim claims${j.source_ok ? "" : " — source unavailable: " + esc(j.source_detail || "")}</div>`;
          el.innerHTML =
            `<div class="m" style="margin-bottom:6px"><b>${esc(j.mode || "")}</b> · ${j.count} claim${j.count === 1 ? "" : "s"}</div>` +
            cards + discl + genLine(j);
          this.count = j.claims.length;
          return;
        }
        // Onion search shape
        const rows = (j.results || []).slice(0, ROW_CAP).map((r) =>
          `<div class="feed-item"><div><b>${esc(r.title || "(untitled)")}</b></div><div class="m">${mono(r.url || "")}</div></div>`
        ).join("") || `<div class="m">no onion results${j.source_ok ? "" : " — source unavailable: " + esc(j.source_detail || "")}</div>`;
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px">onion search: <b>${esc(j.query || "")}</b> · ${j.count} result${j.count === 1 ? "" : "s"}</div>` +
          rows + discl + genLine(j);
        this.count = (j.results || []).length;
      },
    },
    {
      id: "storms", label: "Storms", health: "storms", map: "storms",
      group: "global",
      note: "NOAA NHC active tropical cyclones — position, category, wind/pressure",
      url: () => "/api/storms",
      render(j, el) {
        const active = j.active || [];
        const catStyle = (c) => /^Cat[345]/.test(c || "")
          ? "color:var(--bad);border-color:var(--bad)"
          : /^Cat[12]/.test(c || "") ? "color:var(--warn);border-color:var(--warn)"
          : (c === "TS" ? "color:var(--accent,#7aa2f7);border-color:var(--accent,#7aa2f7)" : "color:var(--dim,#9aa4b2);border-color:var(--dim,#9aa4b2)");
        if (j.none_active) {
          el.innerHTML = `<div class="m">no active tropical cyclones (NHC, as of ${esc((j.generated_at || "").slice(0, 16).replace("T", " "))} UTC)</div>` + genLine(j);
          this.count = 0;
          return;
        }
        el.innerHTML =
          `<div class="m" style="margin-bottom:6px">${active.length} active storm${active.length === 1 ? "" : "s"}</div>` +
          active.map((s) =>
            `<div class="feed-item"><div><span class="chip" style="${catStyle(s.category)};border:1px solid;border-radius:3px;padding:0 5px;font-size:10px">${esc(s.category || "")}</span> ` +
            `<b>${esc(s.name || "unnamed")}</b> <span class="m">${esc(s.basin || "")} · ${esc(s.atcf_id || "")}</span></div>` +
            `<div>wind ${s.wind_kt != null ? s.wind_kt + " kt" : "—"} · pressure ${s.pressure_mb != null ? s.pressure_mb + " mb" : "—"} · moving ${esc(s.movement || "—")}</div>` +
            `<div class="m">advisory ${esc(s.advisory_time || "—")}</div>` +
            (s.headline ? `<div style="margin-top:4px">${esc(s.headline)}</div>` : "") +
            `</div>`
          ).join("") + genLine(j);
        this.count = active.length;
      },
    },
    {
      id: "risk", label: "Risk & safety", health: null,
      group: "global",
      note: "heuristic SAFETY score per country or city — danger to persons (violent crime, terror/conflict, violent unrest) · not a forecast · not travel advice",
      timeout: 120000,
      inputs: [
        { name: "place", placeholder: "country or city", required: true },
        { name: "kind", type: "select", options: ["auto", "country", "city"], value: "auto" },
        { name: "country", placeholder: "country hint (city only, optional)" },
      ],
      validate: (p) => (p.place && p.place.trim() ? null : "enter a place name"),
      url: (p) => `/api/risk?place=${encodeURIComponent(p.place || "")}&kind=${encodeURIComponent(p.kind || "auto")}` +
        (p.country && p.country.trim() ? `&country=${encodeURIComponent(p.country.trim())}` : ""),
      render(j, el) {
        const scoreNull = j.score == null;
        const score = scoreNull ? 0 : Math.max(0, Math.min(100, Number(j.score)));
        const colors = { low: "#46a758", moderate: "#ffb224", high: "#f76b15", severe: "#e5484d", unknown: "#888" };
        const color = colors[j.level] || "#888";
        const place = j.place || {};
        const peerLine = (j.percentile != null && j.rank != null && j.peer_count != null)
          ? `<div class="m" style="margin-bottom:6px">hotter than <strong>#${j.rank} of ${j.peer_count}</strong> watchlist places this week ` +
            `<span class="m">(percentile ${Number(j.percentile).toFixed(0)} — baseline-free peer rank, robust to feed capping)</span></div>`
          : (j.peer_note ? `<div class="m" style="margin-bottom:6px">peer rank unavailable: ${esc(j.peer_note)}</div>` : "");
        const compLi = (c) => {
          const diag = [];
          if (c.baseline_source && c.baseline_source !== "unknown") diag.push(`baseline: ${esc(c.baseline_source)}`);
          if (c.saturated) diag.push("saturated");
          if (c.cap_fill_hours != null) diag.push(`cap filled in ${Number(c.cap_fill_hours).toFixed(1)}h`);
          if (c.cap_fill_intensity != null) diag.push(`intensity ${Number(c.cap_fill_intensity).toFixed(0)}`);
          return `<div class="feed-item"><div><strong>${esc(c.source)}</strong>
            <span class="m">weight ${Number(c.weight).toFixed(2)}</span>
            ${c.ok ? `<span class="chip" style="color:${color};border:1px solid ${color};border-radius:3px;padding:0 5px;font-size:10px">${Number(c.normalized_0_1).toFixed(3)}</span>`
                   : `<span class="chip" style="color:var(--bad);border:1px solid var(--bad);border-radius:3px;padding:0 5px;font-size:10px">unavailable</span>`}</div>
            <div class="m">${esc(c.signal)}</div>
            ${c.ok ? `<div class="m">recent ${esc(String(c.raw_value))} ${esc(c.raw_unit)} · baseline ${esc(String(c.baseline_value))} ${esc(c.baseline_unit)}</div>`
                   : `<div class="m" style="color:var(--bad)">${esc(c.error || "feed error")}</div>`}
            ${diag.length ? `<div class="m">${diag.join(" · ")}</div>` : ""}
            ${c.note ? `<div class="m">note: ${esc(c.note)}</div>` : ""}</div>`;
        };
        const fams = (j.families || []).map((f) =>
          `<div class="feed-item"><div><strong>${esc(f.family)}</strong>
            <span class="m">weight ${Number(f.weight).toFixed(2)} · family score ${Number(f.score_0_100 || 0).toFixed(1)}${f.partial ? " · partial" : ""}${f.thin_basis ? ` · <span class="chip" style="color:var(--warn,#b7791f);border:1px solid var(--warn,#b7791f);border-radius:3px;padding:0 5px;font-size:10px">thin basis</span>` : ""}</span></div>
            ${(f.components || []).map(compLi).join("")}</div>`
        ).join("");
        const thinChip = j.thin_basis
          ? ` <span class="chip" style="color:var(--warn,#b7791f);border:1px solid var(--warn,#b7791f);border-radius:3px;padding:1px 8px;font-size:12px">thin basis</span>` : "";
        const confLine = j.confidence ? `<span class="m"> · confidence: <strong>${esc(j.confidence)}</strong></span>` : "";
        el.innerHTML =
          `<div class="m" style="margin-bottom:4px"><b>${esc(place.name || "")}</b>${place.kind ? " · " + esc(place.kind) : ""}${place.country_hint ? " · " + esc(place.country_hint) : ""}</div>` +
          `<div style="margin-bottom:6px"><span class="chip" style="background:${color};color:#fff;border-radius:3px;padding:1px 8px;font-size:12px">${esc(j.level || "")}</span>${thinChip} ` +
          (scoreNull
            ? `<span class="m">risk score unavailable — no usable signal (all feeds down or baselines saturated)</span>`
            : `<span class="m">risk ${score.toFixed(1)} / 100 · safety: <strong>${esc(j.safety || "—")}</strong>${j.partial ? " · partial" : ""}${confLine}</span>`) +
          `</div>` +
          (scoreNull ? "" : `<div class="unrest-bar" style="margin-bottom:8px"><div class="unrest-fill" style="width:${score.toFixed(1)}%;background:${color}"></div></div>`) +
          peerLine +
          (j.note ? `<div class="m" style="margin-bottom:6px">${esc(j.note)}</div>` : "") +
          (j.score_basis ? `<div class="m" style="margin-bottom:6px">score basis: ${esc(j.score_basis)}</div>` : "") +
          fams +
          (j.political_unrest ? (() => {
            const pu = j.political_unrest;
            const puScore = pu.score_0_100 == null ? null : Number(pu.score_0_100);
            const puColor = colors[pu.level] || "#888";
            return `<div class="feed-item" style="opacity:0.75"><div><strong>political unrest</strong>
              <span class="m">diagnostic — not in the safety score</span>
              ${puScore == null
                ? `<span class="m">unavailable</span>`
                : `<span class="chip" style="color:${puColor};border:1px solid ${puColor};border-radius:3px;padding:0 5px;font-size:10px">${esc(pu.level)}</span>
                   <span class="m">${puScore.toFixed(1)} / 100${pu.thin_basis ? " · thin basis" : ""}${pu.partial ? " · partial" : ""}</span>`}</div>
              <div class="m">broad protest/strike/riot/unrest keywords — political signal for supply-chain / operations exposure, not a safety signal</div></div>`;
          })() : "") +
          `<details style="margin:8px 0"><summary class="m" style="cursor:pointer">methodology + disclaimer</summary>` +
          `<div class="m" style="margin-top:4px">${esc(j.methodology || "")}</div>` +
          `<div class="unrest-disc">${esc(j.disclaimer || "")}</div></details>`;
        this.count = (j.families || []).length;
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
      id: "chokepoints", label: "Chokepoints", health: "chokepoints", map: "chokepoints",
      group: "global",
      staticNote: "embedded registry — 10 entries",
      note: "dated snapshot (2026-11-02) — verify before operational use; reference data, not a live feed",
      inputs: [
        { name: "category", type: "select", options: ["all", "chokepoint", "congestion_port"], value: "all" },
      ],
      url: (p) => "/api/chokepoints" + (p.category && p.category !== "all" ? `?category=${encodeURIComponent(p.category)}` : ""),
      render(j, el) {
        const catChip = (c) => c === "chokepoint"
          ? `<span class="chip" style="color:var(--bad);border:1px solid var(--bad);border-radius:3px;padding:0 5px;font-size:10px">chokepoint</span>`
          : `<span class="chip" style="color:var(--warn);border:1px solid var(--warn);border-radius:3px;padding:0 5px;font-size:10px">congested port</span>`;
        el.innerHTML = (j.entries || []).slice(0, ROW_CAP).map((c) =>
          `<div class="feed-item"><div>${catChip(c.category)} <b>${esc(c.name || "—")}</b></div>` +
          `<div>${esc(c.status || "")}</div>` +
          `<div class="m">status as of ${esc(c.status_date || "—")}</div></div>`
        ).join("") +
        `<div class="m" style="margin-top:6px">${(j.entries || []).length} shown · ${esc(j.caveat || "")}</div>` + genLine(j);
        this.count = (j.entries || []).length;
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
      OST.FEED_CATS.filter((c) => (c.group || "global") === g.id)
        .sort((a, b) => (a.label || "").localeCompare(b.label || ""))
        .map((c) =>
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
      if (i.type === "datalist") {
        const prev = lastParams[cat.id] && lastParams[cat.id][i.name] != null ? lastParams[cat.id][i.name] : (i.value || "");
        return `<input data-fin="${i.name}" list="${esc(i.listId)}" placeholder="${esc(i.placeholder || i.name)}" value="${esc(prev)}" spellcheck="false" autocomplete="off" /><datalist id="${esc(i.listId)}"></datalist>`;
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
    if (cat.afterInputs) cat.afterInputs(body);
    body.querySelectorAll("input").forEach((inp) =>
      inp.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); }));
    // Select dropdowns (e.g. dark-web mode) reload immediately on change —
    // no need to tap Load.
    body.querySelectorAll("select").forEach((sel) =>
      sel.addEventListener("change", go));
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
      const { data } = await OST.fetchJSON(cat.url(params), { timeout: cat.timeout || 30000 });
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
