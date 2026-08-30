#!/usr/bin/env bash
#
# End-to-end smoke test against a real running binary.
#
# Unit and integration tests drive the router in-process. This drives the actual
# executable over a real socket, with real configuration, a real DuckDB file and
# real Parquet on disk. That difference is not academic: it is how a `500` on
# /api/stats/realtime and an unauthenticated /api/keys both reached a green test
# suite.
#
# Usage:  scripts/smoke-test.sh [path-to-binary]
# Exit:   0 when every check passes, 1 otherwise.

set -uo pipefail

BIN="${1:-target/debug/mallard-metrics}"
if [ ! -x "$BIN" ]; then
  echo "error: $BIN is not an executable. Build it first (cargo build)." >&2
  exit 2
fi

WORK="$(mktemp -d)"
PORT="${MALLARD_SMOKE_PORT:-18321}"
BASE="http://127.0.0.1:$PORT"
SITE="smoke.test"

# Bot filtering is on by default and curl's own User-Agent is a bot, so every
# request needs a browser one or ingestion is accepted-and-silently-dropped.
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'
curl() { command curl -A "$UA" "$@"; }

cleanup() {
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

MALLARD_HOST=127.0.0.1 \
MALLARD_PORT="$PORT" \
MALLARD_DATA_DIR="$WORK/data" \
MALLARD_FLUSH_COUNT=1 \
MALLARD_FLUSH_INTERVAL=1 \
  "$BIN" > "$WORK/server.log" 2>&1 &
SRV=$!

for _ in $(seq 1 60); do
  curl -fsS "$BASE/health" >/dev/null 2>&1 && break
  if ! kill -0 "$SRV" 2>/dev/null; then
    echo "error: the server exited during startup:" >&2
    cat "$WORK/server.log" >&2
    exit 1
  fi
  sleep 1
done

fail=0
pass=0
check() { # name expected-extended-regex actual
  if printf '%s' "$3" | grep -Eq "$2"; then
    pass=$((pass + 1))
  else
    echo "FAIL $1"
    echo "     expected /$2/"
    echo "     got      $3"
    fail=$((fail + 1))
  fi
}
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

# ── Service surface ───────────────────────────────────────────────────────
check health          'ok'                          "$(curl -sS "$BASE/health")"
check ready           'ready|ok'                    "$(curl -sS "$BASE/health/ready")"
check detailed        'behavioral_extension_loaded' "$(curl -sS "$BASE/health/detailed")"
check metrics         'mallard_events_ingested_total' "$(curl -sS "$BASE/metrics")"
check tracker         'sendBeacon'                  "$(curl -sS "$BASE/mallard.js")"
check tracker_alias   'sendBeacon'                  "$(curl -sS "$BASE/js/script.js")"
check dashboard       '<div id="app">'              "$(curl -sS "$BASE/")"
check robots          'User-agent'                  "$(curl -sS "$BASE/robots.txt")"
check security_txt    'Contact'                     "$(curl -sS "$BASE/.well-known/security.txt")"
check security_header 'nosniff'                     "$(curl -sSI "$BASE/health")"

# ── Admin routes must be shut before setup ────────────────────────────────
# A key minted here would keep working after the operator sets a password.
check keys_unauth  '401' "$(code "$BASE/api/keys")"
check erase_unauth '401' "$(code -X DELETE "$BASE/api/gdpr/erase?site_id=$SITE&start_date=2024-01-01&end_date=2024-01-02")"

# ── Ingestion ─────────────────────────────────────────────────────────────
post_event() {
  curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/event" \
    -H 'Content-Type: application/json' -d "$1"
}
for path in / /pricing /about; do
  check "ingest $path" '202' \
    "$(post_event "{\"d\":\"$SITE\",\"n\":\"pageview\",\"u\":\"https://$SITE$path\",\"r\":\"https://www.google.com/\",\"w\":1440}")"
done
check ingest_custom '202' \
  "$(post_event "{\"d\":\"$SITE\",\"n\":\"purchase\",\"u\":\"https://$SITE/checkout\",\"p\":\"{\\\"plan\\\":\\\"pro\\\"}\",\"ra\":25.5,\"rc\":\"usd\"}")"
check pixel '200' "$(code "$BASE/api/event?d=$SITE&n=email_open&u=https%3A%2F%2F$SITE%2Fmail")"
check ingest_rejects_bad_site '400' \
  "$(post_event "{\"d\":\"../etc/passwd\",\"n\":\"pageview\",\"u\":\"https://x/\"}")"

sleep 3

# ── Analytics ─────────────────────────────────────────────────────────────
TODAY=$(date -u +%F)
START=$(date -u -d '7 days ago' +%F)
Q="site_id=$SITE&start_date=$START&end_date=$TODAY"

check main       '"unique_visitors":1'  "$(curl -sS "$BASE/api/stats/main?$Q")"
check timeseries '"visitors"'           "$(curl -sS "$BASE/api/stats/timeseries?$Q")"
check realtime   '"current_visitors":1' "$(curl -sS "$BASE/api/stats/realtime?site_id=$SITE")"
check revenue    '"currency":"USD"'     "$(curl -sS "$BASE/api/stats/revenue?$Q")"
check goals      'purchase'             "$(curl -sS "$BASE/api/stats/goals?$Q")"
check props      'plan'                 "$(curl -sS "$BASE/api/stats/properties?$Q")"
check propvalues 'pro'                  "$(curl -sS "$BASE/api/stats/property-values?$Q&key=plan")"
check sites      "$SITE"                "$(curl -sS "$BASE/api/sites")"
check export_csv 'date'                 "$(curl -sS "$BASE/api/stats/export?$Q")"
check export_raw 'timestamp'            "$(curl -sS "$BASE/api/stats/export?$Q&kind=raw")"

# Every dimension the router accepts, so a new one cannot ship unexercised.
for dim in pages entry-pages exit-pages referrers sources countries regions \
           cities browsers browser-versions os os-versions devices \
           screen-sizes utm-sources utm-mediums utm-campaigns utm-contents \
           utm-terms events; do
  check "breakdown/$dim" '200' "$(code "$BASE/api/stats/breakdown/$dim?$Q")"
done
check breakdown_unknown '400' "$(code "$BASE/api/stats/breakdown/not-a-dimension?$Q")"

# ── Segment filters ───────────────────────────────────────────────────────
# Chrome was the User-Agent every ingest above used, so the segment holds all
# the pageviews and its complement holds none.
check filter_matches    '"unique_visitors":1' \
  "$(curl -sS "$BASE/api/stats/main?$Q&filters=browsers%3D%3DChrome")"
check filter_excludes   '"total_pageviews":0' \
  "$(curl -sS "$BASE/api/stats/main?$Q&filters=browsers%3D%3DFirefox")"
check filter_negation   '"total_pageviews":0' \
  "$(curl -sS "$BASE/api/stats/main?$Q&filters=browsers%21%3DChrome")"
check filter_breakdown  '200' \
  "$(code "$BASE/api/stats/breakdown/pages?$Q&filters=browsers%3D%3DChrome")"
check filter_timeseries '200' \
  "$(code "$BASE/api/stats/timeseries?$Q&filters=browsers%3D%3DChrome")"
check filter_bad_dim    '400' "$(code "$BASE/api/stats/main?$Q&filters=nope%3D%3Dx")"
check filter_no_op      '400' "$(code "$BASE/api/stats/main?$Q&filters=browsers")"
check filter_entry_page '400' "$(code "$BASE/api/stats/main?$Q&filters=entry-pages%3D%3D%2F")"
# A value that looks like SQL must be data, and the server must stay healthy.
check filter_injection  '200' \
  "$(code "$BASE/api/stats/main?$Q&filters=pages%3D%3D%27%20OR%201%3D1%20--")"

# ── Behavioral endpoints ──────────────────────────────────────────────────
# They answer 503 without the extension, which is correct rather than a failure.
for name in sessions funnel retention sequences flow; do
  case $name in
    sessions)  uri="$BASE/api/stats/sessions?$Q" ;;
    funnel)    uri="$BASE/api/stats/funnel?$Q&steps=page%3A%2F%2Cpage%3A%2Fpricing&window=1%20day" ;;
    retention) uri="$BASE/api/stats/retention?$Q&weeks=4" ;;
    sequences) uri="$BASE/api/stats/sequences?$Q&steps=page%3A%2F%2Cpage%3A%2Fpricing" ;;
    flow)      uri="$BASE/api/stats/flow?$Q&page=%2F" ;;
  esac
  check "$name" '200|503' "$(code "$uri")"
done

# ── Setup opens the admin surface ─────────────────────────────────────────
COOKIE_JAR="$WORK/cookies"
check setup '200' \
  "$(curl -sS -o /dev/null -w '%{http_code}' -c "$COOKIE_JAR" -X POST "$BASE/api/auth/setup" \
      -H 'Content-Type: application/json' -d '{"password":"a-sufficiently-long-password"}')"
check keys_authed   '200' "$(code -b "$COOKIE_JAR" "$BASE/api/keys")"
check keys_unauthed_after_setup '401' "$(code "$BASE/api/keys")"
check login_wrong_password '401' \
  "$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/login" \
      -H 'Content-Type: application/json' -d '{"password":"not-the-password"}')"

# ── The server must still be healthy, and quiet ───────────────────────────
check still_alive '200' "$(code "$BASE/health")"
if grep -qiE '\bERROR\b|panicked' "$WORK/server.log"; then
  echo "FAIL server log contains errors:"
  grep -iE '\bERROR\b|panicked' "$WORK/server.log" | head -10
  fail=$((fail + 1))
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "smoke: $pass checks passed"
  exit 0
fi
echo "smoke: $fail failed, $pass passed"
exit 1
