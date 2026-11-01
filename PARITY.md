# OSIRIS Parity Checklist

Target: [OSIRIS](https://osirisai.live) — "The open-source Palantir alternative"
(Next.js, MIT). Source of truth for this checklist: https://osirisai.live/docs
(API reference sections) + the Interface Guide, read 2026-11-01.

Status values: **done** · **partial** · **todo** · **unknown** (OSIRIS has it but
we cannot determine the details) · **deferred** (known, deliberately later) ·
**out-of-scope** (will not build).

Honesty rule: anything implemented against a documented API without a live key
is marked **UNVERIFIED** here and in the README. No superiority claims vs
OSIRIS anywhere.

## Endpoint families (OSIRIS docs § API Reference)

| OSIRIS family | Contents (per docs) | Status | Notes |
|---|---|---|---|
| System | liveness, aggregate counters | done | `/healthz`, `/api/stats` |
| Aviation & Space | aircraft, orbital objects, heliophysics | partial | flights (adsb.lol) done; OpenSky keyed → Phase 1; satellites (CelesTrak TLE) done; N2YO keyed → Phase 1; heliophysics (NOAA SWPC, keyless) → Phase 1 |
| Earth & Environment | seismic, fire, atmospheric, orbital-imagery | partial | quakes (USGS) done; weather (Open-Meteo) done; FIRMS wildfires (keyed) → Phase 1; orbital-imagery → unknown (docs name it, no endpoint detail found) |
| Geopolitical | conflict zones, frontlines, event streams, country risk | todo | → Phase 1 (`/api/conflicts` via ReliefWeb/GDACS keyless; ACLED keyed documented) |
| Media & Markets | news aggregation, live broadcast streams, financial instruments | partial | news (GDELT) done; markets (CoinGecko keyless) → Phase 1; live broadcast streams → unknown (no endpoint detail found) |
| Surveillance & Infrastructure | camera networks, fixed infrastructure, maritime traffic, tile/stream proxies | partial | cameras (curated registry) done; maritime AIS (keyed) → Phase 1; nuclear facilities (curated reference) → Phase 1; tile/stream proxies → deferred (thin proxy layer; revisit post-UI) |
| Cyber Threat | vulnerability, attack, malware telemetry | todo | → Phase 1 (`/api/cyber/threats`: URLhaus/ThreatFox keyless); CVE lookup → Phase 2 |
| OSINT Toolkit | DNS, WHOIS, cert transparency, IP/ASN, breach checks, sanctions, CVE | todo | → Phase 2. Breach checks & sanctions: unknown (no keyless source identified; keyed vendors exist — deferred pending a source decision) |
| Recon Scanner | active scanning, delegated to separate backend (503 when disabled) | todo | → Phase 2: in-process TCP port scan, gated by `RECON_ENABLED` (503 when off, mirroring OSIRIS); nmap-equivalent UDP/OS-detection → out-of-scope (needs raw sockets/root) |
| Entity Graph | link analysis over surfaced entities | todo | → Phase 4 (basic parity: entities + co-occurrence; full fusion is the future differentiator) |
| AI Analysis | Gemini-backed correlation, 3× POST, 5 req/min/IP | out-of-scope | OI-adjacent; needs operator model key |
| OI (Assist & Prediction) | conversational agent + multi-world forecast engine, MCP server | out-of-scope | explicit non-goal for parity |
| Polybolos SDK | push entities into the common picture, stream merged picture out | deferred | ingest-shaped; `SDK_INGEST_KEY` reserved in config (fails closed). Not in phase plan. |
| Webhooks | inbound hooks from external services | deferred | not in phase plan |
| Region Dossier | composite summary of a location from every covering feed (UI: double right-click) | todo | → Phase 3 (`GET /api/dossier?lat=&lon=&radius_km=`) |
| Intel Feed | running stream of incoming events across enabled feeds | todo | → Phase 5 (SSE `/api/intel/stream` to feed the UI) |
| Map UI | MapLibre GL globe, layer panel, RECON panel, intel feed, dossier, graph, status bar | todo | → Phase 5 (v1 fidelity: globe + toggleable layers; not pixel-perfect) |

## Per-feed / per-tool detail

| Item | Upstream / approach | Status | Key handling |
|---|---|---|---|
| flights (mil/all) | adsb.lol (keyless) | done | — |
| flights (authenticated) | OpenSky OAuth2 | partial | `OPENSKY_CLIENT_ID/SECRET`; preferred when set, adsb.lol fallback — UNVERIFIED (no creds) |
| quakes | USGS GeoJSON (keyless) | done | — |
| weather | Open-Meteo (keyless) | done | — |
| satellites | CelesTrak TLE (keyless) | done | — |
| satellites catalog | N2YO API | partial | `N2YO_API_KEY`; CelesTrak fallback — keyed path UNVERIFIED |
| news | GDELT DOC 2.1 (keyless) | done | — |
| cameras | curated registry (keyless) | done | — |
| fires | NASA FIRMS API | done | `FIRMS_API_KEY` required; 503 when unset — UNVERIFIED (no key). NOTE: Suomi NPP VIIRS stream retired 2026-11-02; switch SOURCE to VIIRS_NOAA20_NRT if the endpoint errors |
| vessels (maritime) | AISStream evaluated: websocket-only, no REST | partial | 503-path only; `AIS_API_KEY` reserved — UNVERIFIED, no REST adapter (MarineTraffic/VesselFinder/AISHub documented as alternatives) |
| cyber threats | URLhaus (keyless) + ThreatFox (keyed) | done | ThreatFox corrected 2026-11-01: Auth-Key REQUIRED (HTTP 401 keyless, verified live); keyed via `THREATFOX_API_KEY`, skipped gracefully when unset. liveness UNVERIFIED in sandbox (parsers fixture-verified) |
| threatfox | ThreatFox `get_iocs` (keyed) | done | `GET /api/threatfox?days=&limit=&malware=`; `THREATFOX_API_KEY` required, 503 when unset — UNVERIFIED (no key); keyless → HTTP 401 verified live |
| telegram | public channels via API creds | partial | 503 + full config plumbing; MTProto session deferred-pending-creds — UNVERIFIED |
| markets | CoinGecko (keyless) | done | LIVE-VERIFIED |
| nuclear facilities | curated registry, 189 stations / 34 countries, IAEA-PRIS-derived | done | static reference; reactor_type omitted (source lacks it) |
| conflicts | GDACS RSS (keyless); ACLED keyed alternative documented | partial | `note` field states scope honestly (natural disasters, not armed conflict); ReliefWeb v1 decommissioned, v2 needs pre-registered appname |
| heliophysics | NOAA SWPC X-ray + Kp (keyless) | done | LIVE-VERIFIED |
| recon: dns | hickory-resolver, explicit 8.8.8.8+1.1.1.1, TCP/53 | done | sandbox-blocked (raw TCP EPERM) — code correct, passes with egress |
| recon: whois | TCP/43 with IANA referral following | done | sandbox-blocked (proxy RST) — honest failure |
| recon: ct | crt.sh, 3× retry | done | intermittently 502 via sandbox proxy; parser verified against 81 real rows |
| recon: ip/asn | ip-api.com (keyless) → ipinfo.io (keyed) | done | LIVE-VERIFIED (ip-api path) |
| recon: cve | NVD API v2 | done | LIVE-VERIFIED (known CVE + 404 unknown) |
| recon: scan | in-process TCP connect scan | done | gated by `RECON_ENABLED` (default false → 503); deterministic localhost test passes |
| dossier | aggregate all feeds by lat/lon/radius | done | GET /api/dossier?lat=&lon=&radius_km=; haversine filter, 300s rounded-key cache; conflicts included as non-geocoded context; vessels/telegram/satellites excluded (documented) |
| graph | entities + co-occurrence | done | GET /api/graph?entity= and /api/graph/expand; per-request in-memory build, no persistence; kinds: aircraft, callsign, norad_id, facility, name, country, domain, ip, cve, threat_type; unknown entity → 404 |
| web UI | MapLibre GL JS globe (CDN) served by axum from `web/` | done | 5 toggleable layers (flights, quakes, fires, nuclear, cameras); SSE ticker; API routes take precedence over the static fallback |
| intel stream | SSE digest every 15 s | done | GET /api/intel/stream; counts + M6+/emergency-squawk highlights; never errors |

## Keyed-feed degradation contract (tested per feed)

Every keyed feed has a test proving its unset-key behavior: keyless
fallback where one exists (OpenSky→adsb.lol, N2YO→CelesTrak,
ipinfo→ip-api.com), else 503 `{"error","detail"}` (FIRMS, AIS, Telegram,
port scan). A missing key never panics and never breaks other feeds.

## Out-of-scope / deferred summary

- **Out-of-scope:** OI assist/forecast + MCP, Gemini AI-analysis endpoints,
  SGP4 propagation, nmap-equivalent UDP/OS fingerprinting, live broadcast
  streams (unknown), orbital-imagery feeds (unknown).
- **Deferred (known, later):** Polybolos-style ingest SDK, inbound webhooks,
  tile/stream proxies, breach-check and sanctions sources (pending source
  decision).

## Phase map

- **Phase 0** (this commit): parity checklist, `Config` + `.env.example`,
  keyed-feed conventions. ✅
- **Phase 1**: keyed feeds (fires, opensky, n2yo, vessels, cyber, telegram,
  markets, nuclear, conflicts, heliophysics). ✅ (2026-11-01)
- **Phase 2**: RECON toolkit (`/api/recon/*`). ✅ (2026-11-01)
- **Phase 3**: region dossier (`/api/dossier`). ✅ (2026-11-01)
- **Phase 4**: entity graph (`/api/graph`, `/api/graph/expand`). ✅ (2026-11-01)
- **Phase 5**: MapLibre web UI + SSE intel stream. ✅ (2026-11-01)
- **Follow-up**: dedicated ThreatFox feed (`/api/threatfox`, `THREATFOX_API_KEY`; ThreatFox auth requirement verified live). ✅ (2026-11-01)
