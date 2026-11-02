#!/usr/bin/env bash
# Weekly chunked-baseline refresh for the risk/unrest scorer.
#
# The anti-saturation package: upstream news/social windows are capped at
# 100 items, so a single 83-day baseline query returns the 100 *most
# recent* items and understates the true baseline for high-coverage
# places (ratio explodes, scores pin at 100). This script instead issues
# 6 consecutive 14-day chunk queries per (place, family, leg) and sums
# them, defeating the per-query cap.
#
# Query construction lives server-side: the script fetches the chunk plan
# from /api/risk/baseline-plan (windows + exact base queries) and counts
# each chunk via /api/risk/window-count (same cache paths the scorer
# uses). The script is pure orchestration.
#
# SEQUENTIAL with a sleep between calls — parallel bursts stall this
# sandbox's egress proxy (verified 2026-11-02). Full run: 30 places x
# 3 families x 2 legs x 6 chunks = ~1080 calls ≈ 3-4h. Weekly background
# job; bail out early per (place,family,leg) on 2 consecutive zero
# chunks (dead query, don't burn calls).
#
# Writes data/risk_baselines.json atomically (temp + rename):
#   {generated_at, baselines: {"<subject>|<family>|<leg>":
#     {chunks:[6 counts], total, saturated_chunks, window_days:84,
#      early_bail:bool}}}
# The scorer (src/unrest.rs) uses entries fresher than 10 days.
#
# Flags: --limit N (first N places; dry-run), --families a,b (subset).
set -u

APP="${APP_URL:-http://localhost:3000}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WATCHLIST="$ROOT/data/risk_watchlist.json"
OUT="${RISK_BASELINES_PATH:-$ROOT/data/risk_baselines.json}"
SLEEP_SECS="${RISK_BASELINE_SLEEP:-10}"
LIMIT=0
FAMILIES_FILTER=""

while [ $# -gt 0 ]; do
  case "$1" in
    --limit) LIMIT="$2"; shift 2 ;;
    --families) FAMILIES_FILTER="$2"; shift 2 ;;
    *) echo "unknown arg: $1 (usage: $0 [--limit N] [--families violent_crime,terror_conflict])" >&2; exit 2 ;;
  esac
done

log() { echo "[risk_baseline_refresh] $*"; }

if ! curl -s -o /dev/null --max-time 15 "$APP/healthz"; then
  log "ERROR: app unreachable at $APP/healthz"
  exit 1
fi

urlencode() { python3 -c "import urllib.parse,sys; print(urllib.parse.quote(sys.argv[1], safe=''))" "$1"; }

places_tsv="$(python3 -c "
import json
wl = json.load(open('$WATCHLIST'))
for p in wl['places']:
    print(p['name'] + chr(31) + p['kind'] + chr(31) + p.get('country_hint',''))
")"
if [ "$LIMIT" -gt 0 ]; then
  places_tsv="$(echo "$places_tsv" | head -n "$LIMIT")"
fi
total_places="$(echo "$places_tsv" | grep -c .)"
log "refreshing baselines for $total_places places (families: ${FAMILIES_FILTER:-all})"

entries_tmp="$(mktemp)"
trap 'rm -f "$entries_tmp"' EXIT
: > "$entries_tmp"

n=0
while IFS="$(printf '\037')" read -r name kind hint; do
  n=$((n+1))
  plan_url="$APP/api/risk/baseline-plan?place=$(urlencode "$name")&kind=$kind"
  [ -n "$hint" ] && plan_url="$plan_url&country=$(urlencode "$hint")"
  plan="$(curl -s --max-time 60 "$plan_url")"
  subject="$(echo "$plan" | python3 -c "
import json,sys
d = json.load(sys.stdin)
print(d['subject'])
" 2>/dev/null)"
  if [ -z "$subject" ]; then
    log "[$n/$total_places] $name: plan failed, skipping place"
    continue
  fi
  # queries as TSV: family \t leg \t base_query ; chunks as TSV: after \t before
  queries_tsv="$(echo "$plan" | python3 -c "
import json,sys
d = json.load(sys.stdin)
ff = set('$FAMILIES_FILTER'.split(',')) if '$FAMILIES_FILTER' else None
for q in d['queries']:
    if ff and q['family'] not in ff: continue
    print(q['family'] + chr(31) + q['leg'] + chr(31) + q['base_query'])
")"
  chunks_tsv="$(echo "$plan" | python3 -c "
import json,sys
d = json.load(sys.stdin)
for c in d['chunks']:
    print(c['after'] + chr(31) + c['before'])
")"
  while IFS="$(printf '\037')" read -r family leg base_query; do
    [ -z "$family" ] && continue
    chunks_json="[]"; sat=0; zero_streak=0; bailed="false"
    chunk_idx=0
    while IFS="$(printf '\037')" read -r after before; do
      [ -z "$after" ] && continue
      chunk_idx=$((chunk_idx+1))
      if [ "$zero_streak" -ge 2 ]; then
        chunks_json="$(echo "$chunks_json" | python3 -c "import json,sys; a=json.load(sys.stdin); a.append(0); print(json.dumps(a))")"
        bailed="true"
        continue
      fi
      wc_url="$APP/api/risk/window-count?leg=$leg&base_query=$(urlencode "$base_query")&after=$after&before=$before"
      wc="$(curl -s --max-time 120 "$wc_url")"
      parsed="$(echo "$wc" | python3 -c "
import json,sys
try:
    d = json.load(sys.stdin)
    print(str(int(d['count'])) + ' ' + ('1' if d.get('saturated') else '0'))
except Exception:
    print('ERR')
" 2>/dev/null)"
      if [ "$parsed" = "ERR" ] || [ -z "$parsed" ]; then
        log "  $subject|$family|$leg chunk $chunk_idx: count failed, treating as 0"
        count=0; s=0
      else
        count="${parsed%% *}"; s="${parsed##* }"
      fi
      chunks_json="$(echo "$chunks_json" | python3 -c "import json,sys; a=json.load(sys.stdin); a.append($count); print(json.dumps(a))")"
      sat=$((sat + s))
      if [ "$count" -eq 0 ]; then zero_streak=$((zero_streak+1)); else zero_streak=0; fi
      sleep "$SLEEP_SECS"
    done <<< "$chunks_tsv"
    total="$(echo "$chunks_json" | python3 -c "import json,sys; print(sum(json.load(sys.stdin)))")"
    key="$subject|$family|$leg"
    # JSON-escape via argv (keys may contain quotes/unicode)
    entry="$(python3 - "$key" "$chunks_json" "$total" "$sat" "$bailed" <<'PYEOF2'
import json, sys
key, chunks_json, total, sat, bailed = sys.argv[1:6]
print(json.dumps({
    "key": key,
    "chunks": json.loads(chunks_json),
    "total": int(total),
    "saturated_chunks": int(sat),
    "window_days": 84,
    "early_bail": bailed == "true",
}))
PYEOF2
)"
    echo "$entry" >> "$entries_tmp"
    log "[$n/$total_places] $key total=$total saturated_chunks=$sat$([ "$bailed" = true ] && echo ' (early bail)' || true)"
  done <<< "$queries_tsv"
done <<< "$places_tsv"

# --- assemble + atomic write ---
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
python3 - "$entries_tmp" "$TS" "$OUT" <<'EOF'
import json, sys, os
entries_tmp, ts, out = sys.argv[1], sys.argv[2], sys.argv[3]
baselines = {}
for line in open(entries_tmp):
    line = line.strip()
    if not line:
        continue
    e = json.loads(line)
    baselines[e.pop("key")] = e
doc = {"generated_at": ts, "baselines": baselines}
tmp = out + ".tmp"
with open(tmp, "w") as f:
    json.dump(doc, f)
os.replace(tmp, out)
print(f"wrote {out}: {len(baselines)} baselines")
EOF
log "done"
