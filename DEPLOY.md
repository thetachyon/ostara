# Deploying OSTARA — split setup

**Frontend** (static: `web/`) → **Vercel**
**Backend** (Rust API) → **fly.io** (Railway / Render work too — notes at the end)

```
browser ──https──▶ your-site.vercel.app ──/api/*──▶ serverless proxy ──https──▶ ostara-api.fly.dev
                         (static web/)        (api/[[...path]].js,            (Rust, :3000,
                                               reads BACKEND_URL)             serves /api/*)
```

The frontend calls same-origin relative URLs (`/api/prices`, `/api/fusion`, …) —
no JS changes were needed. On Vercel, `/api/*` is handled by a tiny proxy
function (`api/[[...path]].js`) that forwards to `BACKEND_URL` at request time.
(Why a function and not a `vercel.json` rewrite: rewrite destinations are
static and cannot read environment variables.)

Map tiles/library load from the unpkg CDN (`maplibre-gl@5.7.0`) — the browser
needs to reach unpkg.com.

---

## 1. Backend first (you need its URL before Vercel)

### fly.io

```bash
# one-time
curl -L https://fly.io/install.sh | sh
fly auth login

cd ~/workspace/ostara   # or wherever you cloned thetachyon/ostara
# fly.toml is already in the repo; launch keeps it and asks for an app name
fly launch --no-deploy
```

Set secrets — **only the keys you actually have; everything is optional**
(the app degrades gracefully: keyed feeds answer 503 or fall back to keyless
sources, nothing panics). The one worth setting is the AIS vessel key:

```bash
fly secrets set AIS_API_KEY="<your aisstream.io key>"
# optional extras:
fly secrets set THREATFOX_API_KEY="<auth.abuse.ch key>" FIRMS_API_KEY="<nasa firms key>"
```

Then:

```bash
fly deploy
fly status
curl https://ostara-api.fly.dev/healthz        # → 200
curl "https://ostara-api.fly.dev/api/prices?symbols=AAPL,CL=F"  # → quotes
```

Note your URL: `https://<your-app-name>.fly.dev` — Vercel needs it next.

### Railway (alternative)

Railway auto-detects the `Dockerfile`. `railway init` → link the repo →
Variables tab → add the keys you have → `railway up`. The service URL is shown
in the dashboard; the app honors Railway's injected `PORT` automatically.

### Render (alternative)

New → Web Service → point at the repo → Runtime: Docker → add env vars in the
dashboard → Deploy. Render also injects `PORT`; honored automatically.

---

## 2. Frontend on Vercel

1. Push is already on GitHub: `github.com/thetachyon/ostara`.
2. Vercel dashboard → **Add New… → Project** → Import `thetachyon/ostara`.
3. Settings are pre-filled by the repo's `vercel.json`:
   - Framework Preset: **Other**
   - Build Command: *(none — static site)*
   - Output Directory: **`web`** (from `vercel.json`)
   - The `api/[[...path]].js` function is auto-detected as a serverless function.
4. **Environment Variables** → add:
   - `BACKEND_URL` = `https://ostara-api.fly.dev` (your backend URL from step 1)
5. **Deploy.**

Verify: open the site → the globe renders → Feeds panel shows live data →
`https://<your-site>.vercel.app/api/health/feeds` returns feed health JSON
(proxied to the backend).

### One known limitation

The SSE intel ticker (`/api/intel/stream`) is an infinite stream. Serverless
functions have a maximum execution duration, so on Hobby the proxy will cut the
stream periodically and the browser reconnects (the ticker keeps working, with
reconnects). If you want a persistent stream, open the backend URL directly or
use a plan with longer function durations.

---

## 3. Weekly risk-map cron

The `risk-map-weekly-refresh` cron scores the 30-place watchlist through the
API and writes snapshots the map layer reads. It currently targets the local
backend. After the move, either:

- keep a local backend running (the systemd service) and change nothing, or
- point the scoring script at fly: set the API base in
  `scripts/risk_map_score.sh` / `scripts/risk_baseline_refresh.sh` to
  `https://ostara-api.fly.dev` (they default to `http://localhost:3000`).

---

## 4. Environment variable inventory

Every variable is **optional**. Unset keys never panic and never break other
feeds — the route answers 503 with `{"error","detail"}` or falls back to a
keyless source.

| Variable | If unset |
|---|---|
| `PORT` | Container hosts (fly.io, Railway, Render) inject this. **Takes precedence** over `OSIRIS_PORT`. |
| `OSIRIS_PORT` | Listen port for self-hosted runs. Default `3000`. |
| `AIS_API_KEY` | `/api/vessels` → 503 (live AIS stream is websocket-only; no keyless fallback). **The one key worth setting.** |
| `THREATFOX_API_KEY` | `/api/threatfox` → 503; `/api/cyber/threats` serves URLhaus data only. Free at auth.abuse.ch. |
| `FIRMS_API_KEY` | `/api/fires` → 503 (no keyless fallback). Free at firms.modaps.eosdis.nasa.gov. |
| `OPENSKY_CLIENT_ID` / `OPENSKY_CLIENT_SECRET` | Flights fall back to adsb.lol (keyless). |
| `N2YO_API_KEY` | Satellites fall back to CelesTrak TLE (keyless). |
| `TELEGRAM_CHANNELS` | Comma-separated allowlist; empty = any public channel via `?channel=`. |
| `NVD_API_KEY` | CVE lookup works keyless at lower rate limits. |
| `IPINFO_TOKEN` | IP/ASN enrichment falls back to ip-api.com (keyless). |
| `RECON_ENABLED` | Default `false` → `/api/recon/scan` answers 503. Only enable for infrastructure you own or are authorized to test. |
| `PORT_SCAN_MAX_CONCURRENCY` / `PORT_SCAN_TIMEOUT_MS` / `PORT_SCAN_MAX_PORTS` | Defaults `64` / `1500` / `1000`. |
| `SDK_INGEST_KEY` | Reserved; unset = authenticated ingest disabled (fails closed). |
| `RISK_SNAPSHOT_DIR` | Default `data/risk_snapshots` (relative to working dir). Baked into the image as a bootstrap; refresh via the weekly script. |
| `RISK_BASELINES_PATH` | Default `data/risk_baselines.json`. Missing/stale file = scorer falls back to the legacy live baseline. |
| `HTTPS_PROXY` / `https_proxy` (plus `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` honored by the HTTP client) | Only used when set. **Leave unset on fly.io/Railway/Render** — direct connections are correct there. (The sandbox dev setup needs them; production hosts don't.) |
| `RUST_LOG` | Log filter, e.g. `info`. Default warn-ish. |

**Secrets hygiene:** the repo's `.env` (with the real AIS key) is gitignored and
excluded from the Docker image by `.dockerignore`. On fly.io use
`fly secrets set`; on Vercel/Railway/Render use their env-var dashboards.
Never commit a populated `.env` — only `.env.example` belongs in git.

---

## 5. Cost notes (approximate — verify current pricing)

- **Vercel Hobby**: free tier covers personal projects (bandwidth + serverless
  invocations within generous limits). The `/api` proxy adds one function
  invocation per API call — fine at dashboard scale.
- **fly.io**: paid platform (a payment method is required); a single
  `shared-cpu-1x` machine with 256–512 MB runs this comfortably for a few
  dollars/month. `fly.toml` keeps `min_machines_running = 1` so the vessel
  websocket and caches stay warm.
- **Railway / Render**: usage-based; both have low-cost entry points — check
  their current pricing pages.

---

## 6. Troubleshooting

| Symptom | Likely cause |
|---|---|
| `/api/*` returns `{"error":"misconfigured"}` | `BACKEND_URL` not set on the Vercel project (or not redeployed after setting it). |
| `/api/*` returns `{"error":"bad_gateway"}` | Backend unreachable: not deployed yet, wrong URL, or `fly` machine stopped (`fly status`). |
| Globe is a black screen, ticker says "map library failed to load" | Browser can't reach unpkg.com (CDN) — corporate firewall / offline. |
| `/api/vessels` → 503 | `AIS_API_KEY` not set on the backend (expected without the key). |
| `/api/risk-map` → 503 "no snapshot yet" | Weekly scoring hasn't run against this backend; run `scripts/risk_map_score.sh` against its URL once. |
| Intel ticker reconnects every ~minute | Expected on Vercel Hobby (function duration limit) — see §2 note. |
| `fly deploy` fails on the health check | App didn't bind `$PORT` in time — check `fly logs`; the Rust log line on startup shows the listen address. |

## What wasn't verified here

No real accounts were available, so nothing was actually deployed: the
`fly.toml` health-check syntax, the Vercel function behavior at the edge, and
unpkg reachability from your browser are all untested in production. The
Dockerfile was not built here either (no docker daemon in this environment) —
the multi-stage layout follows the standard `rust:1.99-bookworm` pattern for
this toolchain.

## CI

Pushes to `main` auto-deploy the backend to Fly.io via `.github/workflows/fly-deploy.yml` (flyctl --remote-only). Manual runs: Actions tab -> Deploy to Fly.io -> Run workflow.
