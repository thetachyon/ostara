/* OSTARA layers.js — map-layer + data-feed definitions.
 * Every URL below is a real backend endpoint (see README API reference).
 * Loaded before app.js; registers on window.OST. */

window.OST = window.OST || {};
(function (OST) {
  "use strict";

  const esc = (s) =>
    String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  /* ---------------- map layers (geolocated) ---------------- */
  OST.MAP_LAYERS = [
    {
      id: "flights", label: "Aircraft", color: "#ff9f43",
      url: "/api/flights?scope=all&limit=250",
      toFeatures: (j) => (j.flights || [])
        .filter((f) => f.latitude != null && f.longitude != null)
        .map((f) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [f.longitude, f.latitude] },
          properties: {
            title: esc(f.callsign || f.registration || f.hex || "unknown"),
            desc:
              `hex ${esc(f.hex || "?")} · ` +
              `alt ${f.altitude_m != null ? Math.round(f.altitude_m) + " m" : "gnd"} · ` +
              `${f.speed_knots != null ? Math.round(f.speed_knots) + " kt · " : ""}` +
              `${f.aircraft_type ? esc(f.aircraft_type) + " · " : ""}` +
              (f.emergency ? `<b>EMERGENCY ${esc(f.emergency)}</b>` : `seen ${Math.round(f.seen_pos_s || 0)}s ago`),
          },
        })),
    },
    {
      id: "vessels", label: "Vessels", color: "#7c9aff",
      url: "/api/vessels",
      note: "AISStream live AIS (websocket) — 503 until AIS_API_KEY is set",
      toFeatures: (j) => (j.vessels || [])
        .filter((v) => v.latitude != null && v.longitude != null)
        .map((v) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [v.longitude, v.latitude] },
          properties: {
            title: esc(v.name || v.mmsi || "vessel"),
            desc: `mmsi ${esc(v.mmsi || "?")} · ${esc(v.vessel_type || "")} · ${v.speed_knots != null ? Math.round(v.speed_knots) + " kt" : ""}`,
          },
        })),
    },
    {
      id: "fires", label: "Wildfires", color: "#ff3d00",
      url: "/api/fires",
      note: "NASA FIRMS — 503 until FIRMS_API_KEY is set",
      toFeatures: (j) => (j.fires || [])
        .filter((f) => f.lat != null && f.lon != null)
        .map((f) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [f.lon, f.lat] },
          properties: {
            title: `Fire — ${Math.round(f.brightness || 0)} K`,
            desc: `${esc(f.acq_date || "")} · ${esc(f.satellite || "")}`,
          },
        })),
    },
    {
      id: "quakes", label: "Earthquakes", color: "#ff5252",
      url: "/api/quakes",
      sized: true,
      toFeatures: (j) => (j.quakes || [])
        .filter((q) => q.latitude != null && q.longitude != null)
        .map((q) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [q.longitude, q.latitude] },
          properties: {
            title: `M${q.magnitude != null ? Number(q.magnitude).toFixed(1) : "?"} — ${esc(q.place || "unknown")}`,
            desc: `${esc(q.time || "")} · depth ${q.depth_km != null ? q.depth_km + " km" : "—"}` +
              (q.url ? ` · <a href="${esc(q.url)}" target="_blank" rel="noopener">usgs</a>` : ""),
            mag: q.magnitude || 4,
          },
        })),
    },
    {
      id: "nuclear", label: "Nuclear facilities", color: "#ffd600",
      url: "/api/nuclear",
      toFeatures: (j) => (j.facilities || j.items || j.nuclear || [])
        .filter((n) => n.lat != null && n.lon != null)
        .map((n) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [n.lon, n.lat] },
          properties: {
            title: esc(n.name || "facility"),
            desc: `${esc(n.country || "")} · ${esc(n.status || "")}${n.reactor_type ? " · " + esc(n.reactor_type) : ""}`,
          },
        })),
    },
    {
      id: "chokepoints", label: "Chokepoints", color: "#ff5252",
      url: "/api/chokepoints?category=chokepoint",
      toFeatures: (j) => (j.entries || [])
        .filter((c) => c.latitude != null && c.longitude != null)
        .map((c) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [c.longitude, c.latitude] },
          properties: {
            title: esc(c.name || "chokepoint"),
            desc: `${esc(c.status || "")}<br/><span class="m">status as of ${esc(c.status_date || "—")} — dated snapshot, not live</span>`,
          },
        })),
    },
    {
      id: "chokepoint_ports", label: "Congested ports", color: "#f5a524",
      url: "/api/chokepoints?category=congestion_port",
      toFeatures: (j) => (j.entries || [])
        .filter((c) => c.latitude != null && c.longitude != null)
        .map((c) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [c.longitude, c.latitude] },
          properties: {
            title: esc(c.name || "port"),
            desc: `${esc(c.status || "")}<br/><span class="m">status as of ${esc(c.status_date || "—")} — dated snapshot, not live</span>`,
          },
        })),
    },
    {
      id: "riskmap", label: "Risk map", color: "#e5484d",
      url: "/api/risk-map",
      scoreStyled: true,
      defaultOff: true,
      note: "weekly risk scores (heuristic media signals, not a forecast) — dated snapshot",
      toFeatures: (j) => (j.places || [])
        .filter((p) => p.lat != null && p.lon != null && p.score != null)
        .map((p) => {
          const s = Number(p.score);
          const lvl = esc(p.level || "");
          const delta = p.delta == null ? "" :
            ` <span class="m">(${p.delta > 0 ? "+" : ""}${Number(p.delta).toFixed(1)} vs last week)</span>`;
          const peer = (p.percentile != null && p.rank != null && p.peer_count != null) ?
            `<br/><span class="m">hotter than <b>#${p.rank} of ${p.peer_count}</b> watchlist places this week <span class="m">(percentile ${Number(p.percentile).toFixed(0)} — baseline-free peer rank)</span></span>` : "";
          const fams = (p.families || [])
            .map((f) => `${esc(f.family)} ${Number(f.score_0_100).toFixed(0)}${f.thin_basis ? " (thin)" : ""}`)
            .join(" · ");
          const thinLine = p.thin_basis
            ? `<br/><span class="chip" style="color:#b7791f;border:1px solid #b7791f;border-radius:3px;padding:0 5px;font-size:10px">thin basis — score rests on &lt;50% of intended signal, level provisional</span>` : "";
          return {
            type: "Feature",
            geometry: { type: "Point", coordinates: [p.lon, p.lat] },
            properties: {
              title: `${esc(p.name || "")} <span class="m">(${esc(p.kind || "")})</span>`,
              desc: `risk <b>${s.toFixed(1)} / 100</b> · ${lvl} · safety: <b>${esc(p.safety || "—")}</b>${delta}` +
                thinLine +
                peer +
                (fams ? `<br/><span class="m">${fams}</span>` : "") +
                (p.partial ? `<br/><span class="m">partial — a feed was down, weights redistributed</span>` : "") +
                `<br/><span class="m">snapshot ${esc(j.generated_at || "—")} — weekly, not live</span>`,
              score: s,
              kind: p.kind || "",
            },
          };
        }),
    },
    {
      id: "storms", label: "Storms", color: "#e5484d",
      url: "/api/storms",
      scoreStyled: true,
      defaultOff: true,
      note: "NOAA NHC active tropical cyclones — marker size/color scale with Saffir-Simpson category",
      toFeatures: (j) => {
        // Reuse the score-driven paint: category → pseudo-score.
        const catScore = { TD: 15, PT: 25, TS: 35, HU: 60, Cat1: 55, Cat2: 65, Cat3: 80, Cat4: 90, Cat5: 100 };
        return (j.active || [])
          .filter((s) => s.lat != null && s.lon != null)
          .map((s) => ({
            type: "Feature",
            geometry: { type: "Point", coordinates: [s.lon, s.lat] },
            properties: {
              title: `${esc(s.category || "")} ${esc(s.name || "unnamed")} <span class="m">(${esc(s.atcf_id || "")})</span>`,
              desc: `wind <b>${s.wind_kt != null ? s.wind_kt + " kt" : "—"}</b> · pressure ${s.pressure_mb != null ? s.pressure_mb + " mb" : "—"} · moving ${esc(s.movement || "—")}` +
                `<br/><span class="m">advisory ${esc(s.advisory_time || "—")} · ${esc(s.basin || "")}</span>` +
                (s.headline ? `<br/>${esc(s.headline)}` : ""),
              score: catScore[s.category] != null ? catScore[s.category] : 30,
            },
          }));
      },
    },
    {
      id: "cameras", label: "Public cameras", color: "#40c4ff",
      url: "/api/cameras?limit=200",
      toFeatures: (j) => (j.cameras || [])
        .filter((c) => c.lat != null && c.lon != null)
        .map((c) => ({
          type: "Feature",
          geometry: { type: "Point", coordinates: [c.lon, c.lat] },
          properties: {
            title: esc(c.name || "camera"),
            desc: `${esc(c.location || "")} · ${esc(c.category || "")}` +
              (c.image_url ? `<br/><a href="${esc(c.image_url)}" target="_blank" rel="noopener">snapshot</a>` : "") +
              (c.page_url ? ` · <a href="${esc(c.page_url)}" target="_blank" rel="noopener">source</a>` : ""),
          },
        })),
    },
  ];

  /* ---------------- list feeds (non-geolocated) ---------------- */
  // Each: id/label, url(params)->string, items(json)->array, itemHTML(item)->html.
  // Optional: inputs=[{name, placeholder}] rendered as a small form.
  OST.LIST_FEEDS = [
    {
      id: "news", label: "News",
      inputs: [{ name: "q", placeholder: "search news… (required)" }],
      url: (p) => `/api/news?q=${encodeURIComponent(p.q || "")}&max_records=25`,
      validate: (p) => (p.q && p.q.trim() ? null : "enter a search query"),
      items: (j) => j.articles || [],
      itemHTML: (a) =>
        `<div class="feed-item"><div class="t"><a href="${esc(a.url)}" target="_blank" rel="noopener">${esc(a.title || a.url)}</a></div>` +
        `<div class="m">${esc(a.domain || "")} · ${esc(a.source_country || "")} · ${esc(a.language || "")} · ${esc(a.seen_at || "")}</div></div>`,
    },
    {
      id: "markets", label: "Markets",
      url: () => "/api/markets",
      items: (j) => j.markets || [],
      itemHTML: (m) => {
        const chg = m.change_24h_pct != null ? Number(m.change_24h_pct).toFixed(2) + "%" : "—";
        const cls = m.change_24h_pct > 0 ? "color:var(--good)" : m.change_24h_pct < 0 ? "color:var(--bad)" : "";
        return `<div class="feed-item"><div class="t">${esc(m.symbol)} <span class="m">$${Number(m.price_usd).toLocaleString()}</span></div>` +
          `<div class="m">${esc(m.name || "")} · <span style="${cls}">${chg}</span> 24h</div></div>`;
      },
    },
    {
      id: "conflicts", label: "Conflicts",
      url: () => "/api/conflicts",
      items: (j) => j.events || j.conflicts || [],
      itemHTML: (c) =>
        `<div class="feed-item"><div class="t">${esc(c.title || "")}</div>` +
        `<div class="m">${esc(c.country || "")} · ${esc(c.date || "")}${c.url ? ` · <a href="${esc(c.url)}" target="_blank" rel="noopener">source</a>` : ""}</div></div>`,
    },
    {
      id: "cyber", label: "Cyber IOCs",
      url: () => "/api/cyber/threats",
      items: (j) => j.indicators || [],
      itemHTML: (t) =>
        `<div class="feed-item"><div class="t" style="font-family:var(--mono);font-size:11px">${esc(t.value || "")}</div>` +
        `<div class="m">${esc(t.kind || "")} · ${esc(t.threat_type || "")} · ${esc(t.first_seen || "")}</div></div>`,
    },
    {
      id: "threatfox", label: "ThreatFox",
      inputs: [
        { name: "malware", placeholder: "malware family (optional)" },
        { name: "days", placeholder: "days (1–7)", value: "1" },
      ],
      url: (p) => `/api/threatfox?days=${encodeURIComponent(p.days || "1")}&limit=100${p.malware ? "&malware=" + encodeURIComponent(p.malware) : ""}`,
      items: (j) => j.iocs || [],
      itemHTML: (t) =>
        `<div class="feed-item"><div class="t" style="font-family:var(--mono);font-size:11px">${esc(t.ioc || "")}</div>` +
        `<div class="m">${esc(t.ioc_type || "")} · ${esc(t.malware_printable || t.malware || "")} · ${esc(t.threat_type || "")} · conf ${t.confidence_level != null ? t.confidence_level : "?"}</div></div>`,
    },
    {
      id: "telegram", label: "Telegram",
      inputs: [{ name: "channel", placeholder: "channel name (no @)" }],
      url: (p) => `/api/telegram?channel=${encodeURIComponent(p.channel || "")}`,
      validate: (p) => (p.channel && p.channel.trim() ? null : "enter a channel name"),
      items: (j) => j.messages || [],
      itemHTML: (m) =>
        `<div class="feed-item"><div class="m">@${esc(m.channel || "")} · ${esc(m.date || "")} · 👁 ${m.views != null ? m.views : "?"}</div>` +
        `<div>${esc((m.text || "").slice(0, 280))}${(m.text || "").length > 280 ? "…" : ""}</div>` +
        (m.url ? `<div class="m"><a href="${esc(m.url)}" target="_blank" rel="noopener">open</a></div>` : "") + `</div>`,
    },
    {
      id: "spacewx", label: "Space Wx",
      url: () => "/api/space-weather",
      custom: (j, el) => {
        el.innerHTML =
          `<table class="kv">` +
          `<tr><td>X-ray flux (GOES)</td><td>${j.xray_flux_latest != null ? Number(j.xray_flux_latest).toExponential(2) + " W/m²" : "—"}</td></tr>` +
          `<tr><td>Kp index</td><td>${j.kp_latest != null ? j.kp_latest : "—"}</td></tr>` +
          `<tr><td>generated</td><td>${esc(j.generated_at || "")}</td></tr>` +
          (j.note ? `<tr><td>note</td><td>${esc(j.note)}</td></tr>` : "") + `</table>`;
      },
    },
    {
      id: "satellites", label: "Satellites",
      url: () => "/api/satellites?limit=50",
      items: (j) => j.satellites || [],
      note: (j) => j.note,
      itemHTML: (s) =>
        `<div class="feed-item"><div class="t">${esc(s.name || "")} <span class="m">NORAD ${s.norad_id}</span></div>` +
        `<div class="m" style="font-size:10px">${esc((s.line1 || "").slice(0, 69))}<br/>${esc((s.line2 || "").slice(0, 69))}</div></div>`,
    },
  ];

  OST.esc = esc;
})(window.OST);
