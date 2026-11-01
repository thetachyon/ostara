# Provenance — `countries.geo.json`

- **Source:** https://github.com/datasets/geo-countries (`data/countries.geojson`, commit ca3b715, fetched 2026-11-01)
- **Upstream data:** Natural Earth 10m Admin-0 Countries, public domain per
  https://www.naturalearthdata.com/about/terms-of-use/
- **License of this package:** Open Data Commons Public Domain Dedication and
  License (PDDL) 1.0 — no attribution required. (Repo code is MIT; the geometry
  is public domain.)
- **Vendoring transform** (script run once, not committed — reproducible from the
  source file above):
  1. `shapely.simplify(tolerance=0.12, preserve_topology=True)` per feature
     (14.6 MB → 896 KB); coordinate rounding to 3 decimals.
  2. Unique string id per feature in property `id`: the ISO 3166-1 alpha-3 code
     where Natural Earth provides one (France `FRA` and Norway `NOR` filled from
     `ISO_A3_EH`, as the upstream build does); the 20 features with `-99`
     (disputed/special areas, e.g. Kosovo, Somaliland, Northern Cyprus) get
     synthetic ids `X-<NAME-SLUG>` (e.g. `X-KOSOVO`) so every feature has a
     unique id for MapLibre `promoteId`/feature-state.
  3. Properties trimmed to `id`, `name`, `ISO3166-1-Alpha-3`, `ISO3166-1-Alpha-2`.
- **Why this dataset:** per-feature ISO codes (required for painting individual
  countries), clean 3-property schema, monthly-rebuilt upstream, public-domain
  dedication. Rejected alternatives: `johan/world.geo.json` (the Reddit-classic
  answer — UNLICENSE/public domain but ~2.5 MB aggregate with a 50-field schema
  and unmaintained since 2018); `world-atlas` TopoJSON (tiny, but needs a
  client-side TopoJSON decoder dependency).
