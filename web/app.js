/* OSTARA web UI — fetches the ostara API, plots toggleable GeoJSON layers
 * on a MapLibre globe, and streams the intel digest via SSE. v1 fidelity:
 * functional, fast, clean. No build step. */

const REFRESH_MS = 5 * 60 * 1000;

const LAYERS = [
  {
    id: "flights", label: "Aircraft", url: "/api/flights?scope=all&limit=250",
    color: "#ff9f43",
    toFeatures: (j) => (j.flights || [])
      .filter((f) => f.latitude != null && f.longitude != null)
      .map((f) => ({
        type: "Feature",
        geometry: { type: "Point", coordinates: [f.longitude, f.latitude] },
        properties: {
          title: f.callsign || f.registration || f.hex,
          desc: `hex ${f.hex} · alt ${f.altitude_m != null ? Math.round(f.altitude_m) + " m" : "—"} · ${f.speed_knots != null ? Math.round(f.speed_knots) + " kt" : ""}${f.emergency ? " · <b>EMERGENCY " + f.emergency + "</b>" : ""}`,
        },
      })),
  },
  {
    id: "quakes", label: "Earthquakes", url: "/api/quakes",
    color: "#ff5252",
    toFeatures: (j) => (j.quakes || [])
      .filter((q) => q.latitude != null && q.longitude != null)
      .map((q) => ({
        type: "Feature",
        geometry: { type: "Point", coordinates: [q.longitude, q.latitude] },
        properties: {
          title: `M${q.magnitude != null ? q.magnitude.toFixed(1) : "?"} — ${q.place || "unknown"}`,
          desc: `${q.time || ""} · depth ${q.depth_km != null ? q.depth_km + " km" : "—"}`,
          mag: q.magnitude || 4,
        },
      })),
  },
  {
    id: "fires", label: "Wildfires", url: "/api/fires",
    color: "#ff3d00",
    toFeatures: (j) => (j.fires || [])
      .map((f) => ({
        type: "Feature",
        geometry: { type: "Point", coordinates: [f.lon, f.lat] },
        properties: {
          title: `Fire — ${Math.round(f.brightness)} K`,
          desc: `${f.acq_date || ""} · ${f.satellite || ""}`,
        },
      })),
  },
  {
    id: "nuclear", label: "Nuclear facilities", url: "/api/nuclear",
    color: "#ffd600",
    toFeatures: (j) => (j.facilities || j.items || [])
      .map((n) => ({
        type: "Feature",
        geometry: { type: "Point", coordinates: [n.lon, n.lat] },
        properties: {
          title: n.name,
          desc: `${n.country} · ${n.status || ""}`,
        },
      })),
  },
  {
    id: "cameras", label: "Public cameras", url: "/api/cameras",
    color: "#40c4ff",
    toFeatures: (j) => (j.cameras || [])
      .map((c) => ({
        type: "Feature",
        geometry: { type: "Point", coordinates: [c.lon, c.lat] },
        properties: {
          title: c.name,
          desc: `${c.location || ""}${c.page_url ? ` · <a href="${c.page_url}" target="_blank" rel="noopener">open</a>` : ""}`,
        },
      })),
  },
];

const map = new maplibregl.Map({
  container: "map",
  style: "https://demotiles.maplibre.org/style.json",
  center: [10, 25],
  zoom: 1.6,
  projection: "globe",
});
map.addControl(new maplibregl.NavigationControl(), "top-right");

const layerList = document.getElementById("layer-list");
const state = {}; // id -> {features, count, available}

for (const L of LAYERS) {
  state[L.id] = { count: null, available: true };
  const row = document.createElement("label");
  row.className = "layer";
  row.id = `row-${L.id}`;
  row.innerHTML = `<input type="checkbox" checked data-layer="${L.id}" />
    <i class="dot" style="background:${L.color}"></i>${L.label}<span class="n" id="n-${L.id}">…</span>`;
  layerList.appendChild(row);
}
layerList.addEventListener("change", (e) => {
  const id = e.target.dataset.layer;
  if (!id || !map.getLayer(id)) return;
  map.setLayoutProperty(id, "visibility", e.target.checked ? "visible" : "none");
});

map.on("click", (e) => {
  const feats = map.queryRenderedFeatures(e.point, {
    layers: LAYERS.map((l) => l.id).filter((id) => map.getLayer(id)),
  });
  if (!feats.length) return;
  const p = feats[0].properties;
  new maplibregl.Popup()
    .setLngLat(feats[0].geometry.coordinates)
    .setHTML(`<b>${p.title || ""}</b><br/>${p.desc || ""}`)
    .addTo(map);
});

async function loadLayer(L) {
  const nEl = document.getElementById(`n-${L.id}`);
  const row = document.getElementById(`row-${L.id}`);
  try {
    const r = await fetch(L.url);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const features = L.toFeatures(await r.json());
    state[L.id] = { count: features.length, available: true };
    nEl.textContent = features.length.toLocaleString();
    row.classList.remove("unavailable");
    const src = map.getSource(L.id);
    if (src) src.setData({ type: "FeatureCollection", features });
    else {
      map.addSource(L.id, { type: "geojson", data: { type: "FeatureCollection", features } });
      const paint = L.id === "quakes"
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
    }
  } catch (err) {
    state[L.id] = { count: null, available: false };
    nEl.textContent = "n/a";
    row.classList.add("unavailable");
    row.title = `Unavailable: ${err.message} (feed may need an API key)`;
  }
}

async function refreshAll() {
  await Promise.all(LAYERS.map(loadLayer));
}
map.on("load", () => { refreshAll(); setInterval(refreshAll, REFRESH_MS); });

// --- intel ticker (SSE) ---
const ticker = document.getElementById("ticker");
const clock = document.getElementById("clock");
setInterval(() => { clock.textContent = new Date().toUTCString(); }, 1000);

try {
  const es = new EventSource("/api/intel/stream");
  es.addEventListener("digest", (e) => {
    try {
      const d = JSON.parse(e.data);
      const bits = [];
      if (d.counts?.quakes != null) bits.push(`<b>${d.counts.quakes}</b> quakes`);
      if (d.counts?.flights != null) bits.push(`<b>${d.counts.flights}</b> mil aircraft`);
      for (const h of (d.highlights || []).slice(0, 3)) bits.push(`<b>⚠</b> ${h}`);
      ticker.innerHTML = bits.length ? bits.join(" · ") : "no highlights";
    } catch { /* keep last good ticker */ }
  });
  es.onerror = () => { ticker.textContent = "intel stream reconnecting…"; };
} catch {
  ticker.textContent = "intel stream unavailable";
}
