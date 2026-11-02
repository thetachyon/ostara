#!/usr/bin/env bash
# Weekly risk-map scoring pipeline.
#
# Scores every place in data/risk_watchlist.json via the local
# /api/risk endpoint, SEQUENTIALLY with a sleep between calls. Sequential
# is required: parallel bursts stall this sandbox's egress proxy and
# produce all-failed 0.0 scores (verified 2026-11-02).
#
# Writes data/risk_snapshots/snapshot-YYYYMMDD-HHMMSS.json and atomically
# updates data/risk_snapshots/latest.json. Keeps the 8 most recent
# snapshots, prunes older ones.
#
# Exits nonzero if fewer than 80% of places score with at least one ok
# component, or if the app is unreachable.
set -u

APP="${APP_URL:-http://localhost:3000}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WATCHLIST="$ROOT/data/risk_watchlist.json"
SNAPDIR="${RISK_SNAPSHOT_DIR:-$ROOT/data/risk_snapshots}"
SLEEP_SECS="${RISK_SCORE_SLEEP:-10}"
MIN_OK_PCT=80

mkdir -p "$SNAPDIR"
TS="$(date -u +%Y%m%d-%H%M%S)"
TMP="$SNAPDIR/.snapshot-$TS.tmp"
OUT="$SNAPDIR/snapshot-$TS.json"

log() { echo "[risk_map_score $TS] $*"; }

# --- app must be up ---
if ! curl -s -o /dev/null --max-time 15 "$APP/healthz"; then
  log "ERROR: app unreachable at $APP/healthz"
  exit 1
fi

urlencode() { python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1]))" "$1"; }

places_json="$(python3 -c "
import json
wl = json.load(open('$WATCHLIST'))
for p in wl['places']:
    hint = p.get('country_hint','')
    print(p['name'] + chr(31) + p['kind'] + chr(31) + hint + chr(31) + str(p['lon']) + chr(31) + str(p['lat']))
")"
total="$(echo "$places_json" | grep -c .)"
log "scoring $total places from $WATCHLIST (sequential, ${SLEEP_SECS}s between calls)"

results_tmp="$(mktemp)"
trap 'rm -f "$results_tmp"' EXIT
: > "$results_tmp"

n=0
while IFS="$(printf '\037')" read -r name kind hint lon lat; do
  n=$((n+1))
  url="$APP/api/risk?place=$(urlencode "$name")&kind=$kind"
  [ -n "$hint" ] && url="$url&country=$(urlencode "$hint")"
  body="$(curl -s --max-time 170 "$url")"
  ok="$(echo "$body" | python3 -c "
import json,sys
try:
    d = json.load(sys.stdin)
    fams = d.get('families', [])
    comps = [c for f in fams for c in f.get('components', [])]
    okc = sum(1 for c in comps if c.get('ok'))
    vol = sum(float(c.get('raw_value') or 0) for c in comps if c.get('ok'))
    fam = []
    for f in fams:
        fvol = sum(float(c.get('raw_value') or 0) for c in f.get('components', []) if c.get('ok'))
        fam.append({'family': f['family'], 'weight': f['weight'],
                    'score_0_100': round(f.get('score_0_100',0),1), 'volume': round(fvol,1),
                    'thin_basis': bool(f.get('thin_basis'))})
    print(json.dumps({'ok': okc, 'n': len(comps), 'score': d.get('score'), 'level': d.get('level'),
                      'safety': d.get('safety'), 'partial': d.get('partial'),
                      'thin_basis': bool(d.get('thin_basis')), 'confidence': d.get('confidence'),
                      'recent_volume': round(vol,1), 'families': fam}))
except Exception as e:
    print(json.dumps({'ok': 0, 'n': 0, 'error': str(e)[:120]}))
")"
  echo "$name|$kind|$lon|$lat|$ok" >> "$results_tmp"
  log "[$n/$total] $name ($kind): $ok"
  [ "$n" -lt "$total" ] && sleep "$SLEEP_SECS"
done <<< "$places_json"

# --- assemble snapshot (also counts ok places) ---
# Baseline-free peer ranks: competition rank + percentile from 7-day
# recent composite volumes, overall and per family. Mirrors
# feeds::risk_map::rank_percentile in Rust: rank 1 = hottest,
# percentile = 100*(n-rank)/(n-1). Capping can only tie ranks, never
# reverse them — the saturation-robust headline.
counts="$(python3 - "$results_tmp" "$TS" "$TMP" <<'EOF'
import json, sys, datetime
results_tmp, ts, tmp = sys.argv[1], sys.argv[2], sys.argv[3]

def rank_percentile(vals):
    n = len(vals)
    out = []
    for v in vals:
        rank = 1 + sum(1 for u in vals if u > v)
        pct = round(100.0 * (n - rank) / (n - 1), 1) if n > 1 else 100.0
        out.append((rank, pct))
    return out

places = []
for line in open(results_tmp):
    line = line.rstrip('\n')
    name, kind, lon, lat, payload = line.split('|', 4)
    p = json.loads(payload)
    places.append({
        "name": name, "kind": kind, "lon": float(lon), "lat": float(lat),
        "score": round(p["score"], 1) if p.get("score") is not None else None,
        "level": p.get("level"), "safety": p.get("safety"),
        "partial": bool(p.get("partial")),
        "thin_basis": bool(p.get("thin_basis")), "confidence": p.get("confidence"),
        "ok_components": p.get("ok", 0),
        "recent_volume": p.get("recent_volume"),
        "families": p.get("families", []),
    })

n = len(places)
peer_count = n
for (place, (rank, pct)) in zip(places, rank_percentile([p.get("recent_volume") or 0.0 for p in places])):
    place["percentile"] = pct
    place["rank"] = rank
    place["peer_count"] = peer_count
# per-family peer ranks
fam_ids = []
for p in places:
    for f in p["families"]:
        if f["family"] not in fam_ids:
            fam_ids.append(f["family"])
for fid in fam_ids:
    vols = []
    refs = []
    for p in places:
        f = next((x for x in p["families"] if x["family"] == fid), None)
        vols.append((f or {}).get("volume") or 0.0)
        refs.append(f)
    for f, (rank, pct) in zip(refs, rank_percentile(vols)):
        if f is not None:
            f["percentile"] = pct
            f["rank"] = rank

snap = {
    "generated_at": datetime.datetime.strptime(ts, "%Y%m%d-%H%M%S").strftime("%Y-%m-%dT%H:%M:%SZ"),
    "place_count": len(places),
    "places": places,
    "caveat": "Weekly heuristic media-signal snapshot — not a forecast. Scores compare current signals to baselines (chunked weekly refresh when fresh); percentile/rank are baseline-free peer ranks vs the 30-place watchlist (rank 1 = hottest). Big cities/capitals are systematically over-covered. English-language sources only. Not travel advice.",
}
open(tmp, 'w').write(json.dumps(snap))
ok_places = sum(1 for p in places if p["ok_components"] >= 1)
print(f"{ok_places} {len(places)}")
EOF
)"
ok_places="$(echo "$counts" | cut -d' ' -f1)"
total_places="$(echo "$counts" | cut -d' ' -f2)"
pct=$(( ok_places * 100 / total_places ))
log "scored ok: $ok_places/$total_places ($pct%, need >= $MIN_OK_PCT%)"

mv "$TMP" "$OUT"
# atomic latest.json (copy + rename; the API reads the file directly)
cp "$OUT" "$SNAPDIR/.latest-$TS.tmp" && mv "$SNAPDIR/.latest-$TS.tmp" "$SNAPDIR/latest.json"

# prune: keep 8 most recent snapshots (+ latest.json)
ls -1 "$SNAPDIR"/snapshot-*.json 2>/dev/null | sort | head -n -8 | xargs -r rm -f
log "wrote $OUT ($(wc -c < "$OUT") bytes); latest.json updated"

if [ "$pct" -lt "$MIN_OK_PCT" ]; then
  log "ERROR: only $pct% of places scored ok (< $MIN_OK_PCT%)"
  exit 1
fi
log "done: $ok_places/$total places ok"
