/* OSTARA countries.js — paintable country-boundary layer.
 *
 * Static GeoJSON lives at web/data/countries.geo.json (see PROVENANCE.md there:
 * Natural Earth via datasets/geo-countries, public domain, simplified to <1MB).
 * Loaded once through a geojson source with promoteId:'id'; hover and painting
 * are done with MapLibre feature-state — no source reloads on paint.
 * Painted countries persist in localStorage and survive theme swaps, layer
 * toggles and reloads. Requires app.js helpers (OST.esc, OST.toast,
 * OST.MAP_LAYERS); loaded before app.js. No build step.
 */

window.OST = window.OST || {};
(function (OST) {
  "use strict";
  const esc = OST.esc || ((s) => String(s == null ? "" : s));

  const SOURCE = "countries";
  const FILL = "countries-fill";
  const LINE = "countries-line";
  const URL = "/data/countries.geo.json";
  const STORE_KEY = "ostara.countryPaint.v1";
  const HOVER_COLOR = "rgba(140,180,255,0.45)";

  const paint = new Map(); // id -> '#rrggbb'
  try {
    const raw = JSON.parse(localStorage.getItem(STORE_KEY) || "[]");
    if (Array.isArray(raw))
      for (const [id, c] of raw)
        if (typeof id === "string" && /^#[0-9a-fA-F]{6}$/.test(c)) paint.set(id, c);
  } catch (_) { /* corrupted store: start clean */ }
  function save() {
    try { localStorage.setItem(STORE_KEY, JSON.stringify([...paint])); } catch (_) {}
  }

  let paintMode = false;
  let paintColor = "#ffd166";
  let hoverId = null;

  OST.isPaintMode = () => paintMode;
  OST.togglePaintMode = function () { paintMode = !paintMode; return paintMode; };
  OST.setPaintColor = function (c) { if (/^#[0-9a-fA-F]{6}$/.test(c)) paintColor = c; };

  function loaded(map) {
    return !!map.getSource(SOURCE) && map.isSourceLoaded(SOURCE);
  }

  function applyPaint(map) {
    if (!loaded(map)) return;
    for (const [id, color] of paint)
      map.setFeatureState({ source: SOURCE, id }, { painted: true, paint: color });
  }

  function updateCount() {
    const el = document.getElementById("n-countries");
    if (el) el.textContent = paint.size > 0 ? `${paint.size} painted` : "258";
  }

  function doPaint(map, id) {
    paint.set(id, paintColor); save();
    if (loaded(map)) map.setFeatureState({ source: SOURCE, id }, { painted: true, paint: paintColor });
    updateCount();
  }

  function unpaint(map, id) {
    paint.delete(id); save();
    if (loaded(map)) map.setFeatureState({ source: SOURCE, id }, { painted: false });
    updateCount();
  }

  OST.clearCountryPaint = function (map) {
    const ids = [...paint.keys()];
    paint.clear(); save();
    if (map && loaded(map))
      for (const id of ids) map.setFeatureState({ source: SOURCE, id }, { painted: false });
    updateCount();
  };

  OST.setCountriesVisible = function (map, on) {
    for (const lid of [FILL, LINE])
      if (map.getLayer(lid)) map.setLayoutProperty(lid, "visibility", on ? "visible" : "none");
  };

  function setHover(map, id) {
    if (hoverId === id) return;
    if (loaded(map)) {
      if (hoverId) map.setFeatureState({ source: SOURCE, id: hoverId }, { hover: false });
      if (id) map.setFeatureState({ source: SOURCE, id }, { hover: true });
    }
    hoverId = id;
    map.getCanvas().style.cursor = id ? "pointer" : "";
  }

  function countryAt(map, point) {
    if (!map.getLayer(FILL) || map.getLayoutProperty(FILL, "visibility") === "none") return null;
    const feats = map.queryRenderedFeatures(point, { layers: [FILL] });
    return feats.length ? feats[0] : null;
  }

  function wire(map) {
    // Re-apply paint whenever the (re-added) source finishes loading —
    // this also covers style swaps, which drop sources and feature-state.
    map.on("sourcedata", (e) => {
      if (e.sourceId === SOURCE && e.isSourceLoaded()) { applyPaint(map); updateCount(); }
    });
    map.on("mousemove", (e) => {
      const f = countryAt(map, e.point);
      setHover(map, f ? String(f.id != null ? f.id : f.properties.id) : null);
    });
    map.on("mouseout", () => setHover(map, null));
    map.on("click", (e) => {
      const f = countryAt(map, e.point);
      if (!f) return;
      // Feed point markers win over the country underneath.
      const ids = OST.MAP_LAYERS.map((l) => l.id).filter((id) => map.getLayer(id));
      if (ids.length && map.queryRenderedFeatures(e.point, { layers: ids }).length) return;
      const id = String(f.id != null ? f.id : f.properties.id);
      const name = f.properties.name || id;
      if (paintMode) {
        const cur = paint.get(id);
        if (cur && cur.toLowerCase() === paintColor.toLowerCase()) unpaint(map, id);
        else doPaint(map, id);
        return;
      }
      const a3 = f.properties["ISO3166-1-Alpha-3"];
      const a2 = f.properties["ISO3166-1-Alpha-2"];
      const code = [a3, a2].filter((c) => c && c !== "-99").join(" · ");
      const painted = paint.get(id);
      new maplibregl.Popup({ closeButton: true })
        .setLngLat(e.lngLat)
        .setHTML(
          `<b>${esc(name)}</b><br/>` +
          `<span style="font-family:var(--mono);font-size:11px">${esc(code || id)}</span>` +
          (painted ? `<br/><span style="font-size:11px">painted <i class="dot" style="background:${esc(painted)}"></i></span>` : "")
        )
        .addTo(map);
    });
  }

  /* Add source + fill + line layers (beneath feed markers), wire interaction
   * once per map, restore visibility + paint. Idempotent; safe to call on
   * every styledata event. */
  OST.ensureCountries = function (map, visible) {
    if (!map.getSource(SOURCE)) {
      map.addSource(SOURCE, { type: "geojson", data: URL, promoteId: "id" });
      const vis = visible ? "visible" : "none";
      map.addLayer({
        id: FILL, type: "fill", source: SOURCE,
        layout: { visibility: vis },
        paint: {
          "fill-color": [
            "case",
            ["boolean", ["feature-state", "painted"], false], ["feature-state", "paint"],
            ["boolean", ["feature-state", "hover"], false], HOVER_COLOR,
            "rgba(0,0,0,0)",
          ],
          "fill-opacity": 0.6,
        },
      });
      map.addLayer({
        id: LINE, type: "line", source: SOURCE,
        layout: { visibility: vis },
        paint: { "line-color": "rgba(160,190,230,0.35)", "line-width": 0.8 },
      });
    }
    if (!map._ostCountriesWired) { wire(map); map._ostCountriesWired = true; }
    OST.setCountriesVisible(map, !!visible);
    applyPaint(map);
    updateCount();
  };
})(window.OST);
