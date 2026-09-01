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
absent() { # name unwanted-extended-regex actual
  if printf '%s' "$3" | grep -Eq "$2"; then
    echo "FAIL $1"
    echo "     did not expect /$2/"
    echo "     got      $3"
    fail=$((fail + 1))
  else
    pass=$((pass + 1))
  fi
}
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

# ── Service surface ───────────────────────────────────────────────────────
check health          'ok'                          "$(curl -sS "$BASE/health")"
check ready           'ready|ok'                    "$(curl -sS "$BASE/health/ready")"
check detailed        'behavioral_extension_loaded' "$(curl -sS "$BASE/health/detailed")"
check metrics         'mallard_events_ingested_total' "$(curl -sS "$BASE/metrics")"
check metrics_http    'mallard_http_requests_total\{status="2xx"\}' "$(curl -sS "$BASE/metrics")"
check metrics_latency 'mallard_http_request_duration_seconds_bucket' "$(curl -sS "$BASE/metrics")"
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
# A comparison window is only meaningful against real data on both sides; what
# matters end to end is that the field appears, names its dates, and that a bad
# value is refused rather than silently ignored.
check compare_previous  '"comparison"' \
  "$(curl -sS "$BASE/api/stats/main?$Q&compare=previous_period")"
check compare_dates     '"start_date"' \
  "$(curl -sS "$BASE/api/stats/main?$Q&compare=year_over_year")"
absent compare_absent_by_default 'comparison' "$(curl -sS "$BASE/api/stats/main?$Q")"
check compare_rejects_junk '400' "$(code "$BASE/api/stats/main?$Q&compare=last_tuesday")"
check timeseries '"visitors"'           "$(curl -sS "$BASE/api/stats/timeseries?$Q")"
check realtime   '"current_visitors":1' "$(curl -sS "$BASE/api/stats/realtime?site_id=$SITE")"
# Realtime used to accept `filters` and ignore them.
check realtime_filtered '"current_visitors":1' \
  "$(curl -sS "$BASE/api/stats/realtime?site_id=$SITE&filters=browsers%3D%3DChrome")"
check realtime_filtered_out '"current_visitors":0' \
  "$(curl -sS "$BASE/api/stats/realtime?site_id=$SITE&filters=browsers%3D%3DFirefox")"
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

# ── The extension seeder ──────────────────────────────────────────────────
# `--install-extension` is what makes the behavioral endpoints usable on a
# deployment with no outbound route, so it has to keep working. It needs the
# network, and CI has one; without it the command correctly fails, which is why
# only the "creates no database" invariant is asserted unconditionally.
SEED_DIR="$WORK/seed"
MALLARD_DATA_DIR="$SEED_DIR" "$BIN" --install-extension > "$WORK/seed.log" 2>&1
seed_status=$?
if [ "$seed_status" -eq 0 ]; then
  check seed_reports_where 'extensions' "$(cat "$WORK/seed.log")"
  check seed_wrote_extension 'behavioral.duckdb_extension' \
    "$(find "$SEED_DIR" -name 'behavioral.duckdb_extension' 2>/dev/null)"
else
  echo "note: --install-extension could not reach the network; skipping its output checks"
fi
# Seeding must never leave a half-initialised database behind, because it is
# documented as safe to run against a live data directory.
absent seed_creates_no_database 'mallard.duckdb' \
  "$(find "$SEED_DIR" -maxdepth 1 2>/dev/null)"

# ── Conditional requests ──────────────────────────────────────────────────
# The dashboard assets carry an ETag; nothing read `If-None-Match`, so every
# "cheap revalidation" re-sent the whole file.
ETAG=$(curl -sSI "$BASE/style.css" | tr -d '\r' | awk 'tolower($1) == "etag:" { print $2 }')
check asset_etag '"' "$ETAG"
check asset_304 '304' "$(code -H "If-None-Match: $ETAG" "$BASE/style.css")"
check asset_200_when_stale '200' "$(code -H 'If-None-Match: "0000000000000000"' "$BASE/style.css")"

# ── The server must still be healthy, and quiet ───────────────────────────
check still_alive '200' "$(code "$BASE/health")"
if grep -qiE '\bERROR\b|panicked' "$WORK/server.log"; then
  echo "FAIL server log contains errors:"
  grep -iE '\bERROR\b|panicked' "$WORK/server.log" | head -10
  fail=$((fail + 1))
fi

# ── The admin password must survive a restart ─────────────────────────────
# It used to live only in memory, so a restart put the instance back into
# first-run mode and the next request to reach it — from anyone — could claim
# it. Only a real restart can catch that, which is why it is tested here and
# not in the in-process suite.
kill "$SRV" 2>/dev/null
wait "$SRV" 2>/dev/null

MALLARD_HOST=127.0.0.1 \
MALLARD_PORT="$PORT" \
MALLARD_DATA_DIR="$WORK/data" \
MALLARD_FLUSH_COUNT=1 \
MALLARD_FLUSH_INTERVAL=1 \
  "$BIN" > "$WORK/server2.log" 2>&1 &
SRV=$!

restarted=0
for _ in $(seq 1 60); do
  if curl -fsS "$BASE/health" >/dev/null 2>&1; then restarted=1; break; fi
  kill -0 "$SRV" 2>/dev/null || break
  sleep 1
done

if [ "$restarted" -ne 1 ]; then
  echo "FAIL the server did not come back up after a restart:"
  cat "$WORK/server2.log"
  fail=$((fail + 1))
else
  check setup_not_required_after_restart '"setup_required":false' \
    "$(curl -sS "$BASE/api/auth/status")"
  check setup_refused_after_restart '409' \
    "$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/setup" \
        -H 'Content-Type: application/json' -d '{"password":"an-attackers-password"}')"
  # The password set before the restart is the one that still works — proof the
  # stored hash is the original and not something regenerated.
  JAR2="$WORK/cookies2"
  check login_after_restart '200' \
    "$(curl -sS -o /dev/null -w '%{http_code}' -c "$JAR2" -X POST "$BASE/api/auth/login" \
        -H 'Content-Type: application/json' -d '{"password":"a-sufficiently-long-password"}')"
  # Reads are protected again, and the events written before the restart are
  # still there — the analytics data and the credential both survived.
  check reads_protected_after_restart '401' "$(code "$BASE/api/stats/main?$Q")"
  check data_survived_restart '"unique_visitors":1' \
    "$(curl -sS -b "$JAR2" "$BASE/api/stats/main?$Q")"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "smoke: $pass checks passed"
  exit 0
fi
echo "smoke: $fail failed, $pass passed"
exit 1
