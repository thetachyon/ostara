# ostara

A high-performance Rust reimplementation of the **API core** of
[OSIRIS](https://osirisai.live) (osirisai.live) — the open-source
intelligence platform that aggregates public OSINT feeds behind a
plain-HTTP JSON API.

**What this is:** the feed-aggregation layer — fetch from keyless public
upstreams, normalize to consistent JSON, serve through a fast cache —
rebuilt in Rust (axum + tokio) for low latency and high throughput.

**What this is not:** a full OSIRIS replica. The WebGL globe UI, the RECON
scanner toolkit (DNS/WHOIS/port scanning), the Telegram OSINT layer, and the
OI assist/forecast engine are out of scope for v0.1. This crate is the
backend the map would run on.

No performance claim is made against OSIRIS's live instance — nothing here
was benchmarked against it. The numbers in this README are local
measurements of this codebase's own hot paths.

## Quickstart

Requires Rust stable (built with 1.99.1):

```sh
cargo run --release
# listening on 0.0.0.0:3000
```

```sh
curl 'http://localhost:3000/api/quakes' | head -c 400
curl 'http://localhost:3000/api/stats'
```

Configuration:

| Variable      | Default | Meaning                                    |
|---------------|---------|--------------------------------------------|
| `OSIRIS_PORT` | `3000`  | Host port to listen on (mirrors OSIRIS)    |
| `RUST_LOG`    | `info`  | Log filter, e.g. `RUST_LOG=ostara=debug`   |

No API keys, no database, no credentials. Every upstream used here is a
keyless public feed — the same "no keys required" philosophy as OSIRIS.

## API reference

All routes are `GET` and return JSON. Timestamps are ISO 8601 UTC.
Upstream failure is normal: a failed feed returns `502` with
`{"error": ..., "detail": ...}` and never takes down the other feeds.
Responses carry an `X-Cache: HIT | MISS` header.

### `GET /healthz`

```sh
curl http://localhost:3000/healthz
```
```json
{"status":"ok","service":"ostara","version":"0.1.0"}
```

### `GET /api/flights` — military/civil aviation (adsb.lol)

| Param   | Default | Constraints              |
|---------|---------|--------------------------|
| `scope` | `mil`   | `mil` or `all`           |
| `limit` | `250`   | clamped to 1–1000        |

```sh
curl 'http://localhost:3000/api/flights?scope=mil&limit=5'
```
```json
{
  "scope": "mil",
  "count": 5,
  "total_upstream": 312,
  "generated_at": "2026-11-01T16:20:00.000Z",
  "flights": [
    {
      "hex": "ae1450",
      "callsign": "TREK136",
      "registration": "11-9001",
      "aircraft_type": "C17",
      "latitude": 38.95, "longitude": -77.45,
      "altitude_m": null, "on_ground": true,
      "speed_knots": 0.0, "track_deg": 90.1,
      "emergency": null, "seen_pos_s": 12.0
    }
  ]
}
```
Altitudes are converted from feet to meters; `altitude_m` is `null` when
the aircraft reports on-ground.

### `GET /api/quakes` — earthquakes, M4.5+ past day (USGS)

```sh
curl http://localhost:3000/api/quakes
```
```json
{
  "count": 15,
  "generated_at": "2026-11-01T16:20:00.000Z",
  "quakes": [
    {
      "id": "us7000tupq",
      "magnitude": 4.8, "magnitude_type": "mb",
      "place": "90 km WNW of Sola, Vanuatu",
      "time": "2026-11-01T12:08:30.561Z",
      "updated": "2026-11-01T12:30:00.000Z",
      "url": "https://earthquake.usgs.gov/earthquakes/eventpage/us7000tupq",
      "latitude": -13.8667, "longitude": 165.6199, "depth_km": 10.0,
      "tsunami": false, "status": "reviewed"
    }
  ]
}
```

### `GET /api/weather` — current conditions (Open-Meteo)

| Param | Required | Constraints              |
|-------|----------|--------------------------|
| `lat` | yes      | −90…90                   |
| `lon` | yes      | −180…180                 |

```sh
curl 'http://localhost:3000/api/weather?lat=40.71&lon=-74.00'
```
```json
{
  "latitude": 40.71, "longitude": -74.0, "elevation_m": 10.0,
  "time": "2026-11-01T16:00:00Z",
  "temperature_c": 14.2, "wind_speed_kmh": 18.5,
  "generated_at": "2026-11-01T16:20:00.000Z"
}
```

### `GET /api/satellites` — orbital element sets (CelesTrak TLE)

| Param   | Default | Constraints       |
|---------|---------|-------------------|
| `limit` | `100`   | clamped to 1–1000 |

```sh
curl 'http://localhost:3000/api/satellites?limit=2'
```
```json
{
  "count": 2,
  "generated_at": "2026-11-01T16:20:00.000Z",
  "note": "TLE element sets only; SGP4 propagation is out of scope for v0.1",
  "satellites": [
    {
      "name": "ISS (ZARYA)",
      "norad_id": 25544,
      "line1": "1 25544U 98067A   ...",
      "line2": "2 25544  51.6416 ..."
    }
  ]
}
```

### `GET /api/news` — news search (GDELT DOC 2.1)

| Param         | Default | Constraints              |
|---------------|---------|--------------------------|
| `q`           | —       | required, ≤200 chars     |
| `max_records` | `25`    | clamped to 1–250         |

```sh
curl 'http://localhost:3000/api/news?q=earthquake&max_records=5'
```
```json
{
  "query": "earthquake",
  "count": 5,
  "generated_at": "2026-11-01T16:20:00.000Z",
  "articles": [
    {
      "url": "https://www.cnnturk.com/...",
      "title": "...",
      "domain": "cnnturk.com",
      "language": "Turkish",
      "source_country": "Turkey",
      "seen_at": "2026-09-27T07:00:00Z"
    }
  ]
}
```

### `GET /api/cameras` — public cameras (embedded registry)

Serves the curated camera registry from `data/cameras.json` (embedded at
compile time). No upstream fetch at request time: `image_url` values are
live snapshots served by the camera operators themselves — we link, never
proxy or store frames. Optional filters:

```sh
curl 'http://localhost:3000/api/cameras?category=traffic&limit=5'
curl 'http://localhost:3000/api/cameras?bbox=-125,32,-114,42'   # minlon,minlat,maxlon,maxlat
```
```json
{
  "count": 2,
  "generated_at": "2026-11-01T17:10:00.000Z",
  "cameras": [
    {
      "id": "caltrans-d4-1",
      "name": "TV102 -- I-580 : West of SR-24",
      "lat": 37.82539,
      "lon": -122.27291,
      "category": "traffic",
      "image_url": "https://cwwp2.dot.ca.gov/data/d4/cctv/image/...jpg",
      "page_url": "https://cwwp2.dot.ca.gov/",
      "location": "Oakland, CA"
    }
  ]
}
```

`bbox` must be `minlon,minlat,maxlon,maxlat` (400 otherwise); `category`
matches case-insensitively; `limit` defaults to 50, clamped at 200.

### `GET /api/stats` — aggregate counters (cheap polling endpoint)

Fans out to every feed concurrently and reports per-feed counts. Degrades
per-feed instead of failing: a dead upstream shows `"ok": false` with its
error while the rest still report.

```sh
curl http://localhost:3000/api/stats
```
```json
{
  "generated_at": "2026-11-01T16:20:00.000Z",
  "degraded": false,
  "feeds": {
    "flights":     {"ok": true, "count": 312},
    "quakes":      {"ok": true, "count": 15},
    "satellites":  {"ok": true, "count": 16},
    "news":        {"ok": true, "count": 10},
    "weather":     {"ok": true, "count": null}
  }
}
```

### `GET /api/health/feeds` — live upstream health probes

Probes each of the fifteen input sources live (4s per-probe budget, all
concurrent) and reports per-feed status. Never fails because one feed is
down — each feed reports its own status. Keyed feeds whose key is unset
report `degraded` with an explicit "disabled" detail (nothing is broken).
A UI status page renders directly from this shape.

- `ok`: upstream returned 2xx with a parseable body within 3s.
- `degraded`: upstream reachable but slow, returned non-2xx, or returned a
  body we could not parse.
- `down`: connection/DNS failure or probe timeout.

```sh
curl http://localhost:3000/api/health/feeds
```
```json
{
  "generated_at": "2026-11-01T17:00:00.000Z",
  "feeds": {
    "flights":    {"status": "ok", "latency_ms": 812, "detail": "312 aircraft (scope=mil, limit=1)"},
    "quakes":     {"status": "ok", "latency_ms": 214, "detail": "15 quakes (M4.5+ day feed)"},
    "weather":    {"status": "ok", "latency_ms": 188, "detail": "probe point 0,0 ok (temp_c=Some(21.4))"},
    "satellites": {"status": "ok", "latency_ms": 640, "detail": "1 objects (stations group, limit=1)"},
    "news":       {"status": "down", "latency_ms": 4001, "detail": "probe timed out after 4s"}
  }
}
```

### Keyed feeds — graceful degradation

The scope change that dropped the "no keys" philosophy: API keys are
configured via env vars (see `.env.example`; loaded with `dotenvy`).
Every keyed feed degrades gracefully when its key is unset — it falls back
to a keyless alternative or returns 503 with `{"error","detail"}`. It never
panics and never breaks other feeds. The unset-key behavior of every keyed
feed is proven by a test (`tests/phase1_keyed.rs`).

| Endpoint | Upstream | Key | Unset behavior |
|----------|----------|-----|----------------|
| `GET /api/fires` | NASA FIRMS (VIIRS active fires) | `FIRMS_API_KEY` | 503 |
| `GET /api/flights` | OpenSky (preferred) / adsb.lol | `OPENSKY_CLIENT_ID` + `OPENSKY_CLIENT_SECRET` | adsb.lol fallback |
| `GET /api/satellites/catalog?lat=&lon=` | N2YO `above` API | `N2YO_API_KEY` | CelesTrak fallback |
| `GET /api/vessels` | AISStream (websocket, live snapshot) | `AIS_API_KEY` (free signup) | 503 when unset; `?bbox=` / `?limit=` |
| `GET /api/cyber/threats` | URLhaus + ThreatFox | `THREATFOX_API_KEY` (both abuse.ch APIs 401 keyless) | 503 when unset |
| `GET /api/threatfox` | ThreatFox `get_iocs` | `THREATFOX_API_KEY` (no keyless tier — HTTP 401) | 503 when unset |
| `GET /api/telegram?channel=` | `t.me/s` public previews | — | keyless; 400 bad/allowlist-blocked channel, 503 upstream failure |
| `GET /api/bluesky?q=&limit=` | Bluesky public AppView search | — | keyless; 400 missing q, 503 upstream failure |
| `GET /api/mastodon?q=&limit=` | fosstodon.org public timeline | — | keyless; q filters the snapshot client-side (no keyless server search exists) |
| `GET /api/rss?outlet=&limit=` | BBC / Al Jazeera / DW / France24 RSS | — | keyless; outlet ∈ all,bbc,aljazeera,dw,france24 (Reuters/AP have no public RSS) |
| `GET /api/gnews?q=&limit=` | Google News RSS | — | keyless; 400 missing q, 503 upstream failure; q is country-queryable |

### Deliberately excluded sources

- **X/Twitter** — API is paid-only; no keyless tier. Not integrated.
- **Reddit** — public JSON endpoints are bot-walled for unauthenticated
  server access (verified 2026-11-01: `www.reddit.com` serves a challenge
  page, `old.reddit.com` returns empty). Not integrated.
- **Reuters / AP RSS** — Reuters RSS endpoints return 401, AP serves 403 /
  offers no public RSS (verified 2026-11-01). Excluded from the RSS outlet list.
| `GET /api/markets` | CoinGecko | — | keyless |
| `GET /api/nuclear?country=` | embedded registry (189 stations) | — | static |
| `GET /api/conflicts` | GDACS RSS | — | keyless; `note` field states scope |
| `GET /api/space-weather` | NOAA SWPC | — | keyless |

### `GET /api/threatfox?days=&limit=&malware=` — ThreatFox malware IOCs

Dedicated ThreatFox feed (abuse.ch threat-intel platform). Requires
`THREATFOX_API_KEY` — a free Auth-Key from https://auth.abuse.ch sent as
the `Auth-Key` header. Verified live 2026-11-01: the Community API rejects
keyless requests with HTTP 401 `{"error": "Unauthorized"}`; there is no
keyless tier. Unset key → 503 with `{"error","detail"}` by design.

| Param     | Default | Constraints                          |
|-----------|---------|--------------------------------------|
| `days`    | `1`     | clamped 1–7 (upstream window)        |
| `limit`   | `100`   | clamped 1–1000                       |
| `malware` | —       | optional case-insensitive filter on malware family |

Each IOC: `ioc`, `ioc_type` (`ip:port`/`domain`/`url`/`md5`/`sha256`),
`threat_type`, `malware`, `malware_printable`, `confidence_level`,
`first_seen`, `last_seen`, `reporter`, `reference`. TTL 15 min.

```sh
curl 'http://localhost:3000/api/threatfox?days=1&malware=emotet'
```

The merged `/api/cyber/threats` feed serves URLhaus IOCs keyless and adds
the ThreatFox half automatically when the key is set; its IOCs also flow
into the entity graph's domain/ip/cve extraction.

### `GET /api/recon/*` — passive reconnaissance toolkit

Five passive tools are always on; the TCP port scanner is gated.

| Endpoint | Tool |
|----------|------|
| `GET /api/recon/dns?domain=` | DNS A/AAAA/MX/TXT/NS (explicit resolvers, no system stub) |
| `GET /api/recon/whois?domain=` | WHOIS with IANA referral following |
| `GET /api/recon/ct?domain=` | Certificate transparency (crt.sh) |
| `GET /api/recon/ip?ip=` | IP/ASN enrichment (ip-api.com → ipinfo.io) |
| `GET /api/recon/cve?id=CVE-…` | CVE lookup (NVD API v2) |
| `POST /api/recon/scan` | TCP connect scan — **gated by `RECON_ENABLED=true`** (default false → 503). Responsible use: only scan targets you own or are authorized to test; concurrency/timeout/port caps enforced. |

### `GET /api/dossier?lat=&lon=&radius_km=` — region dossier

One composite JSON aggregating every geo-capable feed (flights, quakes,
fires, nuclear, cameras) within `radius_km` of the point (haversine;
default radius 100 km, clamped 1–5000). Conflicts are included as
non-geocoded context. Per-feed failures degrade only that section; the
dossier never 502s. Cached 300 s on rounded coordinates.

### `GET /api/graph?entity=` and `GET /api/graph/expand` — entity graph

Basic-parity entity graph, rebuilt per request from feed snapshots (no
persistence — full fusion is the future differentiator). Entity kinds:
aircraft, callsign, norad_id, name, facility, country, domain, ip, cve,
threat_type. Co-occurrence edges within a single feed item only.
Unknown entity → 404 with `{"error","detail"}`.

### `GET /api/intel/stream` — SSE intel digest

Server-sent events: a `hello` event, then a `digest` event every 15 s with
feed counts plus auto-extracted highlights (M6+ quakes, emergency-squawk
aircraft). Never errors — a failing feed contributes nothing.

### `GET /api/country-osint` — country OSINT directory

Curated per-country OSINT resource packs (`data/country_osint.json`,
embedded at compile time). `?country=<name|iso2|iso3>` returns one pack —
resources grouped by category (search, people, business, government, news,
social, finance, collections) with an honest coverage badge (`full` /
`partial` / `links`). Missing param → 400, unknown country → 404 pointing
at the list. `/api/country-osint/countries` lists all 66 covered countries;
`/api/country-osint/global` returns country-agnostic resources (OCCRP
Aleph, OpenSanctions, OpenCorporates, ICIJ Offshore Leaks, …). Link data
only — no prose copied from the source collections; provenance is recorded
in the JSON's `meta.provenance`.

### `GET /api/country/inspect` — click-a-country

`?lat=&lon=` resolves a map click to a country and returns its OSINT pack
plus the country-tagged feed items in one response. Resolution uses a
compact embedded table (`data/country_bboxes.json`, built by
`scripts/gen_country_bboxes.py` from the public-domain datasets/geo-countries
110m GeoJSON — one bbox per polygon, since a single full-geometry bbox is
broken-wide for far-flung territories): the point goes to the country whose
containing polygon bbox it sits deepest inside; a point in no bbox resolves
to the nearest metropolitan center within 500 km, otherwise
`country: null` with an "international waters" note. No polygon data ships —
borders are coarse by design and the response names its resolution method
(`bbox` | `nearest-country` | `international-waters`). The resolver lives
in `src/geo.rs` (`resolve_country`, `country_polygon_bboxes`,
`point_in_country_bboxes`) so feeds can test membership against one
country's boxes. Feed sections reuse the normal endpoints' TTL cache paths:
`news` (GDELT `sourcecountry:` query), `conflicts` (GDACS country name),
`nuclear` (static registry), `flights` (aircraft lat/lon in the country's
boxes, `scope=all:limit=500`, capped at 50), `quakes` (epicenter lat/lon,
newest 10), `cameras` (registry lat/lon). Deliberately excluded and
documented in `src/country_inspect.rs`: `markets` (CoinGecko crypto is
global — no country attribution exists, and any instrument→country mapping
would be guesswork) and `cyber`/`threatfox` (IOCs are IPs/domains/hashes;
country resolution needs a GeoIP database we don't ship, and an embedded
prefix table would be too coarse and dangerous for intel use — no
reverse-DNS guessing). Missing/invalid coords → 400. In the UI,
left-clicking empty map area opens the Country panel (top resources per
category with a full-pack shortcut into the Countries drawer, plus
news/alerts/nuclear/aircraft/earthquakes/cameras sections); marker popups
and right-click dossier are unchanged.

### `GET /api/unrest` — per-country unrest signals (heuristic indicator)

`?country=<name|iso2|iso3>` returns a per-country unrest-signals score.
**This is a heuristic indicator, not a prediction or forecast** — the
response says so, and so does the UI. Everything that feeds the score is
visible in the response: each component exposes its source, signal
description, raw value, baseline value, normalized 0–1, and weight, plus
the methodology string, the disclaimer, and `generated_at`.

**Signals (all baseline-normalized — never raw counts, so large countries
don't always win):**

| Component | Weight | Signal |
|---|---|---|
| GDELT | 0.50 | `theme:PROTEST` articles from the country's own outlets (`sourcecountry:`) via the DOC 2.1 `timelinevol` 90-day mode. 7-day per-bin rate vs the prior ~83-day baseline per-bin rate (the DOC API exposes themes, not raw CAMEO codes; `theme:PROTEST` is the documented proxy for CAMEO 14*). |
| Google News | 0.35 | Two date-bounded queries: `<country> (protest OR strike OR riot OR unrest)` with `after:<7d>` (recent) and `after:<90d> before:<7d>` (baseline). Recent count vs baseline daily rate. |
| Bluesky | 0.15 | Two windowed `searchPosts` calls (`<country> protest`) with `since:`/`until:` bounds over the same 7d/83d windows. Country attribution is weak (global full-text search) — hence the low weight. |

**Normalization:** for a recent/baseline rate ratio `r`, each component maps
to `clamp((r − 1) / 4, 0, 1)` — at baseline → 0, 5× baseline → 1 — with a
low-evidence discount (`recent_count / 5`, capped at 1) so a handful of
articles can't max out a component. Zero baseline with recent activity is a
spike-from-silence (`recent / 25`). **Levels:** <25 low, <50 elevated,
<75 high, ≥75 severe.

**Cache discipline:** every component reuses its feed's normal cache path,
key format, and TTL — at most 5 upstream calls per country per TTL window
(1 GDELT + 2 Google News + 2 Bluesky), all cached; no new API keys, no new
upstreams. Window boundaries are day-quantized (UTC) so cache keys stay
stable for 24h.

**Honest failure modes:** a down feed marks its component `ok: false` with
the error string; the weight redistributes proportionally among the working
components and the response is marked `partial: true` with a `note` naming
the dead feeds — never silently zero.

**Limitations (read before citing):** each window is capped at 100 items by
the upstreams — a saturated window is flagged in the component's `note`
because a capped baseline biases the ratio upward. GDELT's
`sourcecountry:` reflects outlet registration, not event location. Google
News RSS has no country field; the query is keyword-matching. Bluesky has
no country attribution at all. The score measures *media signals about
unrest*, not unrest itself — quiet periods can precede major events and
loud periods can fizzle.

**Deliberately excluded:** Mastodon (the 40-item client-side snapshot can't
produce a baseline window); conflicts/GDACS (natural disasters — counting
them as unrest would be mislabeling); RSS outlets (global wires, no country
field); telegram/markets/cyber/flights/quakes/cameras/vessels (no
country-attributable unrest semantics). All documented in `src/unrest.rs`.

**Anti-saturation package:** upstream 100-item caps understate single-query
83-day baselines for high-coverage places (search returns most-recent-first),
exploding the ratio and pinning scores at 100. Three defenses: (1) weekly
chunked baselines — `scripts/risk_baseline_refresh.sh` issues 6 consecutive
14-day chunk queries per (place, family, leg) and sums them into
`data/risk_baselines.json` (git-ignored); fresh (<10d) chunked baselines
replace the legacy single-query baseline; (2) a saturated baseline nulls
the leg with `saturated_baseline` instead of scoring — weights redistribute,
and `score: null` + `level: "unknown"` when nothing is usable; (3)
baseline-free peer percentiles/ranks vs the 30-place watchlist, computed
from 7-day recent volumes in the weekly scoring run (capping can only tie
ranks, never reverse them). Per-leg diagnostics: `saturated`,
`baseline_source` (`chunked`|`legacy`|`unknown`), `cap_fill_hours` and
`cap_fill_intensity` (100 at ≤6h, hyperbolic decay — a truncation-proof
intensity readout). New endpoints: `GET /api/risk/baseline-plan`
(chunk windows + exact base queries for the refresh script) and
`GET /api/risk/window-count` (one cached upstream window count).
`/api/risk` also returns `percentile`/`rank`/`peer_count` for watchlist
places (with `score_basis`/`percentile_basis` documenting the two numbers)
and `/api/risk-map` serves the peer ranks per place.

Missing country → 400; unknown country → 404. In the UI, the country panel
gains an "Unrest signals" section (score bar, level chip, per-component
breakdown, methodology one click away in a `<details>`) between the OSINT
resources and News sections; existing sections are untouched.

### `GET /api/risk` — safety risk score (danger to persons)

```sh
curl "http://localhost:3000/api/risk?place=France"
curl "http://localhost:3000/api/risk?place=Paris&kind=city&country=France"
```

The headline score answers **"how physically safe is it to be in this
place?"** — danger to persons, nothing else. Three safety families, each
baseline-normalized like `/api/unrest` (7-day recent rate vs baseline,
ratio → `clamp((r-1)/4, 0, 1)`, low-evidence discount):

- `violent_crime` (0.40) — murder/homicide/stabbing/kidnapping/armed-robbery/assault keywords;
- `terror_conflict` (0.35) — bombing/shooting/mass-shooting/terrorist-attack/explosion/airstrike;
- `violent_unrest` (0.25) — riot/violent-protest/clashes/tear-gas/Molotov (unrest that threatens bystanders).

**Deliberately excluded from the safety score:** the generic `crime`
keyword, fraud/scam/cybercrime/smuggling/corruption/money-laundering
(transnational — they don't make streets unsafe), peaceful protest/strikes
(a political signal), and state crackdowns/arrests (enforcement). The
motivating case: China once scored 100/severe because "China executes
Myanmar scam-centre bosses" and "Taiwan criminalizes chip smuggling to
China" matched `China (crime OR …)` — enforcement and transnational
stories scored as maximum street danger. Person-directed keywords don't
make that mistake.

A separate `political_unrest` diagnostic (broad protest/strike/riot/unrest
keywords) is reported for supply-chain/operations context but **never
enters the composite score or the safety reading**. Response also carries
the `safety` inverse reading (High/Moderate/Low/Very low), thin-basis
flags (score kept but qualified when <50% of intended signal survives),
per-leg saturation diagnostics, and baseline-free peer
percentile/rank vs the 30-place watchlist. `kind=city` drops the GDELT leg
honestly (no city granularity). Heuristic, not a forecast, not travel
advice. In the UI the country panel shows a "Safety risk" section (danger
to persons) with the political-unrest diagnostic as a muted row; the Feeds
panel "Risk & safety" category renders families plus the diagnostic.

### `GET /api/threats/*` — threats-against-politicians scanning

**Protective scanning only — not a targeting system.** Public officials
only (78 figures: G20 heads of state/government, US president/VP/cabinet/
congressional leadership, EU institutional leadership, other major world
leaders; see `data/politicians.json` provenance notes). Every hit is a
keyword-flagged public mention requiring human review — never presented
as a verified threat.

- `GET /api/threats/politicians?country=` — the watchlist (name, aliases,
  role, country + ISO codes), filterable by country name/ISO2/ISO3.
- `GET /api/threats/scan?name=<name|alias>&severity=all|critical|high|medium`
  — scans Google News (`"<name>" (assassination OR assassinate OR "death
  threat" OR threaten OR threat OR attack OR kill)`), Bluesky (`"<name>"
  threat`), and the configured `TELEGRAM_CHANNELS` channels (figure-mention
  filter over channel snapshots) through their normal cache paths/TTLs (no
  new upstreams, no new keys; at most 2 upstream calls per figure per TTL
  for the query sources, plus one per Telegram channel, plus a cached
  assembled response). Each item is classified into severity tiers —
  **critical** (`assassinat*`, `death threat`, `kill` + the figure named),
  **high** (`threaten`, `threat against`, `attack`), **medium** (`harm`,
  `violence against`, `plot against`) — case-insensitive, word boundaries
  on the generic single words, first matching tier wins. Items sort by
  severity then recency, capped at 50. The response also carries an
  `accounts` array: social items (Bluesky handle / Telegram channel, with
  profile and post URLs) rolled up by (platform, author) with per-tier
  counts and up to 3 sample posts; news outlets are excluded. Account
  attribution means public accounts whose posts matched the keywords —
  including quoting, condemnation, and figurative use — never "threat
  actors". Every response carries the keyword-scan disclaimer and
  per-source ok/detail statuses; a failed source (or an unconfigured
  Telegram allowlist) degrades honestly, never silently.

**Deliberately excluded:** GDELT (no threat-relevant `theme:` value could
be verified — the DOC API and the GKG theme taxonomy lookup are
unreachable from this environment; inventing a theme is not acceptable);
non-English sources (keyword lists are English-first, stated in every
response). Missing name → 400; unknown figure → 404; bad severity → 400.

In the UI, the Feeds panel gains a "Threat scan" category under Global:
searchable figure dropdown (populated from the watchlist endpoint),
severity filter chips, and result cards with severity-colored chips,
source, date, link, and matched keywords; methodology + disclaimer one
click away in a `<details>`.

### `GET /api/sanctions` — sanctions/tariff/export-control keyword monitor

Keyword scanning of public sources for sanctions language touching a
curated 15-country watchlist (Russia, Iran, China, North Korea,
Venezuela, Belarus, Myanmar, Cuba, Syria, Nicaragua, Sudan, Zimbabwe,
Lebanon, Libya, Hong Kong; see `data/sanctions_watchlist.json`). Reuses
the threat-scan architecture: severity tiers (**critical** — designation
language: `designat*` + `sanction`, entity list, asset freeze, new
sanctions announced/imposed; **high** — `tariff`, `export controls`,
`secondary sanctions`; **medium** — sanctions debate, waivers,
relief/easing, extensions, plus a "sanctions mention" fallback for query
matches below every tier), word-boundary matching, per-item source
attribution, and the KEYWORD SCAN disclaimer style. Sources: Google News
RSS + Bluesky + configured `TELEGRAM_CHANNELS` (each through its normal
cache path/TTL; assembled per-country scan cached 900s).

- `GET /api/sanctions?country=<name|alias|iso|all>&severity=all|critical|high|medium`
  — `country` defaults to `all` (all 15 countries scanned concurrently,
  grouped per country with item counts); a single country does a deep
  scan. Items sort by severity then recency, capped (25/country, 150 in
  all-mode). Bad severity → 400; unknown country → 404.

**Not verified designations.** Every response states: verify against
OFAC's SDN list, the EU sanctions map, and other official sources before
trading or taking compliance action. English-language coverage only.
Feeds panel "Sanctions" category under Global: country input (default
all), severity filter, per-country count chips, and cards with severity
badges, source links, matched keywords, and the disclaimer.

### `GET /api/storms` — active tropical cyclones (NOAA NHC)

```sh
curl http://localhost:3000/api/storms
```
```json
{
  "generated_at": "2026-11-02T16:10:00.000Z",
  "none_active": true,
  "count": 0,
  "active": [],
  "source": "NOAA NHC nhc_active.kml — experimental active-storms product, keyless"
}
```

Keyless parse of NHC's experimental `nhc_active.kml` (120s refresh;
structure verified against an archived 2024-09-27 copy captured during
Hurricane Helene). When storms are active, each carries name, basin
(atlantic/east_pacific/central_pacific from the ATCF id), Saffir-Simpson
category (TD/TS/Cat1–5 from wind speed, PT for post-tropical), lat/lon,
wind (kt), pressure (mb), movement, advisory time, and the NHC headline.
When none are active the response carries `"none_active": true` — it
never invents a storm. Cached 600s. Feeds panel "Storms" category under
Global plus a map layer (off by default) with category-scaled markers.

### `GET /api/fusion` — proprietary fused indicators (the curation layer)

```sh
curl http://localhost:3000/api/fusion
curl "http://localhost:3000/api/fusion?indicator=hormuz-energy-risk"
```
```json
{
  "generated_at": "2026-11-02T16:10:00.000Z",
  "count": 5,
  "indicators": [{
    "id": "hormuz-energy-risk",
    "name": "Hormuz Energy Flow Risk",
    "desk": "energy",
    "score": 88.3,
    "level": "critical",
    "label": "Multiple pressure signals on Hormuz energy flows — desks should check tanker positions and Brent exposure now.",
    "confidence": "high",
    "live_inputs": "5/6",
    "evidence": [{"input": "iran_risk", "source": "/api/risk?place=Iran", "live": true, "weight": 0.3, "weight_used": 0.32, "value": "96.7 (severe)", "detail": "..."}],
    "methodology": "score = 0.32×iran_risk(96.7) + ... = 88.3 → critical. ..."
  }],
  "disclaimer": "FUSION INDICATORS — curated heuristic signals, not forecasts and not trading advice. ..."
}
```

Five curated indicators (`data/fusion_indicators.json`, embedded): Hormuz
energy flow risk, Russia supply risk, China–Strait tension, US macro event
risk, election unrest risk. Each fuses multiple feeds (risk scores,
sanctions critical counts, price moves, calendar events, chokepoint
status, vessel density) through proprietary weights and per-input
normalization transforms into a 0–100 score with a curated analyst label
per level (calm/watch/elevated/critical). Degraded inputs redistribute
their weight — never silently zeroed; missing inputs lower confidence,
never fabricate values. `score: null` + `level: "unknown"` when no input
is live. Cached 600s; all inputs share their feeds' normal cache entries,
so fusion adds no upstream traffic. Feeds panel "Fusion" category (first
under Global) renders the analyst-briefing cards. Unknown ids 404.

### Web UI — MapLibre globe

`GET /` serves the static bundle from `web/` (MapLibre GL JS 5.7.0 via CDN,
dark intel-terminal aesthetic with a light theme, zero build step — the Rust
binary serves it directly). Map-first UX modeled on OSIRIS:

- Full-viewport globe, floating HUD (brand, live UTC clock, SSE intel ticker),
  bottom status bar (per-feed health dots from `/api/health/feeds`, API latency).
- Left rail: 6 toggleable map layers (aircraft, vessels, wildfires,
  earthquakes, nuclear facilities, public cameras) with per-layer counts,
  manual refresh, and 5-minute auto-refresh; plus 8 data feeds (news, markets,
  conflicts, cyber IOCs, ThreatFox, Telegram, space weather, satellites) that
  open as list overlays. Click any point for a detail popup.
- Right side: SSE intel-feed panel, RECON toolkit drawer (DNS, WHOIS, cert
  transparency, IP/ASN, CVE, gated port scan with inline responsible-use
  notice), entity-graph explorer (search → expandable tree, v1), region
  dossier (right-click the map → `/api/dossier` composite).
- "Feeds" button: dedicated Feeds panel — a structured browser for all 16 feed
  categories (flights, vessels, fires, quakes, weather, satellites, overhead
  N2YO, news, cameras, markets, conflicts, cyber IOCs, ThreatFox, Telegram,
  space weather, nuclear). Category navigator with live counts and per-feed
  health dots from `/api/health/feeds`; tables for structured feeds, cards
  for news/messages, key-value for single-point feeds; per-category filters
  (flight scope, camera category, nuclear country, ThreatFox malware/days,
  news/telegram search, lat/lon for weather/overhead); manual refresh plus
  5-minute auto-refresh of the open category; "show on map" toggles that flip
  the corresponding layer-rail layer. Keyed feeds with no key configured show
  their informative degraded state.
- "Countries" button: Country OSINT directory drawer — searchable country
  list with coverage badges and filters; each pack renders grouped clickable
  resources with free/paid badges, plus a global-resources view.
- Keyboard: `F` fullscreen, `L` layers, `I` intel, `R` reset view, `S` copy
  share link, `?` help, `ESC` close. `S` encodes `#c=lat,lon,zoom&l=layers`
  and the app restores from the hash on load.
- Resilience: every fetch has a timeout; a down feed shows a toast and a red
  health dot, never a spinner forever. Works with no API keys configured
  (keyed feeds degrade server-side).

## Public cameras

`/api/cameras` serves a curated registry (`data/cameras.json`, 48 entries
as of 2026-11-01) of **intentionally-public** cameras: state DOT traffic
cams (Caltrans D4 Bay Area, Nebraska 511, Arizona 511, DriveBC), Finland
road-weather cams (Fintraffic Digitraffic, CC BY 4.0), and NPS Grand Canyon
park cams. Every `image_url` was verified live on 2026-11-01 (HTTP 200,
`image/*` content-type, non-trivial size); 57 candidates were checked, 0
failed verification, 9 were trimmed to keep geographic spread in the
48-entry target.

**The bar, and what we refuse:** every entry must be a camera its operator
publishes for public viewing, served keyless over http(s). We deliberately
exclude aggregators of unsecured *private* cameras (insecam-style sites):
a camera being reachable is not the same as being public, and indexing
people's driveways is not what this tool is for. Traffic, landmark,
weather, port, and park cameras are in scope; anything ethically gray is
out, no exceptions.

**Contributing a camera:** open a PR adding an entry to
`data/cameras.json` with `id` (unique, slug), `name`, `lat`/`lon`,
`category`, `image_url` (direct image, not a JS-walled player page),
`page_url` (the operator's page, for attribution), and `location`. The
registry validation tests (`feeds::cameras::tests`) enforce the shape;
verify the image URL live before submitting.

## Feed verification status

| Feed | Upstream | Live from this env (2026-11-01) | Notes |
|------|----------|----------------------------------|-------|
| quakes | USGS GeoJSON | ✅ VERIFIED (HTTP 200) | |
| weather | Open-Meteo | ✅ VERIFIED (HTTP 200) | |
| satellites | CelesTrak TLE | ✅ VERIFIED (HTTP 200) | |
| flights | adsb.lol v2 | ⚠️ UNVERIFIED | Reachable via curl, but this environment's egress proxy stalls reqwest's TLS connections to this host (consistent 5s connect timeouts; USGS/Open-Meteo/CelesTrak work through the same client, so this is environmental, not a code bug). Parser verified against a real saved payload (`benches/fixtures/adsb_mil.json`, 111 aircraft). |
| news | GDELT DOC 2.1 | ⚠️ UNVERIFIED | Same environmental proxy stall as adsb.lol (curl succeeds; reqwest stalls). Parser verified against a real saved payload (`benches/fixtures/gdelt.json`). |
| markets | CoinGecko | ✅ VERIFIED (HTTP 200) | Top-25 markets; fixture-verified parser. |
| helio | NOAA SWPC | ✅ VERIFIED (HTTP 200) | GOES X-ray flux + planetary Kp; fixture-verified parser. |
| fires | NASA FIRMS (VIIRS) | ⚠️ UNVERIFIED (no key) | Requires `FIRMS_API_KEY`; 503-with-detail when unset (tested). Parser verified against saved FIRMS CSV. Note: the VIIRS_SNPP_NRT stream retires 2026-11-02 — switch `SOURCE` to `VIIRS_NOAA20_NRT` if it starts erroring. |
| flights (auth) | OpenSky OAuth2 | ⚠️ UNVERIFIED (no creds) | Preferred over adsb.lol when `OPENSKY_CLIENT_ID`/`SECRET` set; token + normalize logic fixture-verified (`benches/fixtures/opensky.json`). |
| satellites catalog | N2YO `above` API | ⚠️ UNVERIFIED (no key) | `N2YO_API_KEY` unset → keyless CelesTrak fallback (tested); parser fixture-verified. |
| vessels | AISStream websocket | ⚠️ UNVERIFIED (no key) | Free `AIS_API_KEY` at aisstream.io; server keeps one global subscription and serves the rolling per-MMSI snapshot (`?bbox=`, `?limit=`). 503-with-detail when unset (tested). Parser fixture-verified against documented envelope shapes; `ShipStaticData` field names unverified. Best-effort sampling: provider drops messages for slow consumers. |
| cyber | URLhaus + ThreatFox | ⚠️ UNVERIFIED (sandbox) | Both abuse.ch APIs require Auth-Key (`THREATFOX_API_KEY`; HTTP 401 keyless verified live 2026-11-01). Parsers fixture-verified against the documented response shape. |
| threatfox | ThreatFox `get_iocs` | ⚠️ UNVERIFIED (no key) | Requires `THREATFOX_API_KEY`; 503-with-detail when unset (tested). Keyless → HTTP 401 verified live. Fixture is doc-derived (no live data response obtainable without a key). |
| telegram | `t.me/s` preview scrape | ✅ VERIFIED (parser) | Keyless; 20-msg fixture from live `t.me/s/telegram` page. MTProto plan dropped — no auth needed for public channels. Live reachability depends on network (Telegram may 429 datacenter IPs). |
| nuclear | static registry | ✅ (embedded, 189 stations) | 12 coordinate spot-checks; reactor_type omitted (source lacks it). |
| conflicts | GDACS RSS | ⚠️ PARTIAL | Feed is live and parseable (curl-verified 200/252 records), but reqwest stalls in this sandbox like adsb.lol/GDELT; parser fixture-verified. `note` field states scope honestly: natural disasters, not armed conflict (ACLED is the keyed alternative). |
| recon: ip/asn | ip-api.com → ipinfo.io | ✅ VERIFIED (ip-api path) | Keyed ipinfo path UNVERIFIED (no token). |
| recon: cve | NVD API v2 | ✅ VERIFIED | Known CVE returns data; unknown CVE → 404. |
| recon: dns/whois | hickory / TCP/43 | ⚠️ UNVERIFIED (sandbox) | Sandbox blocks raw TCP/53 and WHOIS port 43 (EPERM/RST) — honest 502s; deterministic + fixture tests green. |
| recon: ct | crt.sh | ⚠️ PARTIAL | Parser verified against 81 real rows; upstream intermittently 502s through the sandbox proxy. |
| recon: scan | in-process connect scan | ✅ (deterministic) | Gated by `RECON_ENABLED` (default false → 503 with responsible-use detail); localhost open/closed-port test green. |

UNVERIFIED means "not observed live from this sandbox" — not "broken".
The parse/normalize paths for all five feeds are covered by unit tests on
real upstream payloads, and the 502-with-detail behavior was exercised
against the two stalled upstreams.

## Tests

Six suites, each with one job:

| Suite | Location | What it proves | Network |
|-------|----------|----------------|---------|
| Unit | `src/` (`#[cfg(test)]`) | ingest + normalize is deterministic and correct, on real saved upstream payloads | none |
| Integration | `tests/api_routes.rs` | every route serves the exact HTTP contract a UI renders: status codes, `application/json` content-type, JSON shapes. Feed routes assert 200 **or** 502-with-`{"error","detail"}` — the contract, not liveness | yes (asserts either outcome) |
| Live | `tests/feeds_live.rs` | each input source is actually reachable and returns the expected shape, right now | yes (fails honestly on outage) |
| Keyed feeds | `tests/phase1_keyed.rs` | unset-key degradation (503/fallback) + live contracts for markets/helio | partial |
| RECON | `tests/phase2_recon.rs` | tool contracts; live probes fail honestly where the sandbox blocks raw TCP | partial |
| Dossier / graph / web | `tests/phase3_dossier.rs`, `tests/phase4_graph.rs`, `tests/phase5_web.rs` | geo-filtering, graph traversal, static UI + SSE shape | partial |

```sh
cargo test            # everything: unit + integration + live (needs network)
cargo test --lib      # unit only, no network
cargo test --test api_routes   # contract tests only
cargo test --test feeds_live   # liveness probes only
```

The live tests fail honestly with `LIVE-FAIL <feed> (<host>): <reason>`
diagnostics when an upstream is unreachable — a failed probe is
information, not a defect to hide. See the environment-quirk note at the
top of `tests/feeds_live.rs` for this sandbox's observed egress behavior.

## Architecture

```
src/
  main.rs        axum router, pooled reqwest client, TTL cache wiring
  routes.rs      handlers: cache check → fetch+normalize → serialize once → serve bytes;
                 /api/health/feeds live probes; canonical `app()` router
  cache.rs       bounded RwLock<HashMap> TTL cache of pre-serialized bodies
  error.rs       ApiError (JSON 4xx/502) + FeedError (per-feed failure)
  time.rs        dependency-free epoch-millis → ISO 8601 (Hinnant's algorithm)
  feeds/
    mod.rs       get_json / get_text helpers with feed-attributed errors
    flights.rs   adsb.lol: polymorphic alt_baro, feet→meters
    quakes.rs    USGS GeoJSON: [lon, lat, depth] → named fields
    weather.rs   Open-Meteo: "YYYY-MM-DDTHH:MM" → full ISO 8601
    satellites.rs CelesTrak TLE: 3-line sets → {name, norad_id, line1, line2}
    news.rs      GDELT: seendate "YYYYMMDDTHHMMSSZ" → ISO 8601
    cameras.rs   public-camera registry (embedded JSON): bbox/category/limit filters
data/
  cameras.json 48 verified public cameras (traffic/weather/park), embedded via include_str!
tests/
  api_routes.rs  contract tests: status codes, content-type, UI JSON shapes
  feeds_live.rs  liveness probes: one honest live test per input source
benches/
  parse_bench.rs criterion benches on real saved payloads (no networking)
  fixtures/      real upstream responses captured 2026-11-01
```

## Benchmarks

Criterion microbenchmarks of the parse+normalize hot paths on real saved
payloads — no networking. Measured 2026-11-01 on a 4-vCPU Linux VM,
release profile:

| Benchmark | Payload | Mean time | Throughput |
|-----------|---------|-----------|------------|
| `usgs_geojson_parse` | 10.6 KiB GeoJSON, 15 quakes | 42.8 µs | ~243 MiB/s |
| `adsb_json_parse` | 40.1 KiB JSON, 111 aircraft | 182.8 µs | ~214 MiB/s |
| `tle_parse` | 2.6 KiB TLE, 16 objects | 5.9 µs | ~433 MiB/s |
| `gdelt_json_parse` | 2.2 KiB JSON, article list | 17.7 µs | — |

Run them yourself: `cargo bench`.

These measure this codebase's parsing only. They are not a comparison
against OSIRIS, whose live instance was never benchmarked here.

## Performance design

- **Serialize once, serve bytes.** Handlers serialize the normalized
  response a single time and cache the bytes; hits cost a hash lookup, an
  expiry check, and a memcpy — no re-serialization, no re-normalization.
  The `X-Cache` response header reports HIT/MISS.
- **Bounded TTL cache.** `RwLock<HashMap>` with a 2048-entry bound and
  opportunistic expiry sweeps on insert — no background task, flat memory.
  TTLs mirror OSIRIS's conventions: 45–60s for fast feeds (flights,
  quakes, weather), 300s for rate-limited news, 600s for TLE sets.
- **Concurrent fan-out.** `/api/stats` resolves all feeds with
  `tokio::join!` — latency ≈ slowest feed, not the sum — and reuses the
  same cache entries the individual routes serve.
- **One pooled client.** A single `reqwest::Client` for all upstreams:
  connection reuse across feeds and requests, 12s request / 5s connect
  timeouts, 30s pool idle timeout. A hung upstream can never stall the
  server.
- **Graceful degradation.** Upstream failure is treated as normal:
  per-feed 502s with `{"error","detail"}`, and `/api/stats` degrades
  per-feed instead of failing the request.
- **Zero-copy-friendly parsing.** `serde_json` direct to owned structs;
  the TLE parser works on `&str` slices without allocation per line
  beyond the output records.

## Roadmap

- [ ] SGP4 propagation for `/api/satellites` (positions, not just elements)
- [ ] More OSIRIS feed parity: maritime, wildfire (FIRMS), cyber threats
- [ ] RECON toolkit: DNS/WHOIS/cert-transparency (passive only)
- [ ] WebSocket/SSE streaming for live layers
- [ ] Request coalescing (single-flight) for thundering-herd cache misses
- [ ] Structured `tracing` spans per feed for latency observability

Explicitly out of scope: the WebGL globe UI, the OI assist/forecast
engine, any active scanning, and anything requiring API keys.

## License

MIT.
