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
#
# Environment:
#   MALLARD_REQUIRE_BEHAVIORAL=1  Fail unless the DuckDB `behavioral` extension
#                                 is loaded and reports a version, and the five
#                                 behavioral endpoints answer 200. Without it
#                                 they may answer 503 (an offline machine is a
#                                 legitimate way to run this script). CI sets it:
#                                 a binary that cannot install the extension is
#                                 a broken release, not a skipped test (musl
#                                 builds cannot, because the community
#                                 repository has no linux_*_musl build).
#   MALLARD_SMOKE_PORT            Port to listen on (default 18321).

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

# SC2329 is the name newer ShellCheck gives SC2317; disable both so the same
# comment holds on the runner image's version and a current one.
# shellcheck disable=SC2317,SC2329  # invoked through the EXIT trap
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

# 200 when the extension is required, otherwise 503 (not installed) is also fine.
REQUIRE_BEHAVIORAL="${MALLARD_REQUIRE_BEHAVIORAL:-0}"
if [ "$REQUIRE_BEHAVIORAL" = "1" ]; then BEHAVIORAL_OK='^200$'; else BEHAVIORAL_OK='^(200|503)$'; fi

# ── Service surface ───────────────────────────────────────────────────────
check health          'ok'                          "$(curl -sS "$BASE/health")"
check ready           'ready|ok'                    "$(curl -sS "$BASE/health/ready")"
check detailed        'behavioral_extension_loaded' "$(curl -sS "$BASE/health/detailed")"
check metrics         'mallard_events_ingested_total' "$(curl -sS "$BASE/metrics")"
check metrics_http    'mallard_http_requests_total\{status="2xx"\}' "$(curl -sS "$BASE/metrics")"
check metrics_latency 'mallard_http_request_duration_seconds_bucket' "$(curl -sS "$BASE/metrics")"
check tracker         'sendBeacon'                  "$(curl -sS "$BASE/mallard.js")"
check tracker_alias   'sendBeacon'                  "$(curl -sS "$BASE/js/script.js")"
check extras_script   'window.mallard'              "$(curl -sS "$BASE/mallard-extras.js")"
check extras_cors     'access-control-allow-origin: \*' "$(curl -sSI "$BASE/mallard-extras.js" | tr '[:upper:]' '[:lower:]')"
check dashboard       '<div id="app">'              "$(curl -sS "$BASE/")"
check robots          'User-agent'                  "$(curl -sS "$BASE/robots.txt")"
check security_txt    'Contact'                     "$(curl -sS "$BASE/.well-known/security.txt")"
check security_header 'nosniff'                     "$(curl -sSI "$BASE/health")"

# ── The behavioral extension ──────────────────────────────────────────────
# The in-process tests load it from a normal home directory on glibc; only the
# assembled binary shows whether it can install where it is actually deployed.
if [ "$REQUIRE_BEHAVIORAL" = "1" ]; then
  DETAILED="$(curl -sS "$BASE/health/detailed")"
  check behavioral_loaded  '"behavioral_extension_loaded":true' "$DETAILED"
  check behavioral_version  '"behavioral_version":"[0-9]+\.[0-9]+' "$DETAILED"
  check behavioral_metric  'mallard_behavioral_extension 1'     "$(curl -sS "$BASE/metrics")"
fi

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
check sites_details '"events_30d":[0-9]+' "$(curl -sS "$BASE/api/sites")"
check export_csv 'date'                 "$(curl -sS "$BASE/api/stats/export?$Q")"
check export_raw 'timestamp'            "$(curl -sS "$BASE/api/stats/export?$Q&kind=raw")"
# A raw export streams: chunked, so no Content-Length to promise up front.
RAW_HEADERS="$(curl -sS -D - -o /dev/null "$BASE/api/stats/export?$Q&kind=raw")"
check export_raw_chunked 'transfer-encoding: chunked' "$RAW_HEADERS"
absent export_raw_no_length 'content-length' "$RAW_HEADERS"

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
# They answer 503 without the extension, which is correct rather than a failure
# unless MALLARD_REQUIRE_BEHAVIORAL=1, where anything but 200 is one.
for name in sessions funnel retention sequences flow; do
  case $name in
    sessions)  uri="$BASE/api/stats/sessions?$Q" ;;
    funnel)    uri="$BASE/api/stats/funnel?$Q&steps=page%3A%2F%2Cpage%3A%2Fpricing&window=1%20day" ;;
    retention) uri="$BASE/api/stats/retention?$Q&weeks=4" ;;
    sequences) uri="$BASE/api/stats/sequences?$Q&steps=page%3A%2F%2Cpage%3A%2Fpricing" ;;
    flow)      uri="$BASE/api/stats/flow?$Q&page=%2F" ;;
  esac
  check "$name" "$BEHAVIORAL_OK" "$(code "$uri")"
done

# ── Setup opens the admin surface ─────────────────────────────────────────
COOKIE_JAR="$WORK/cookies"
check setup '200' \
  "$(curl -sS -o /dev/null -w '%{http_code}' -c "$COOKIE_JAR" -X POST "$BASE/api/auth/setup" \
      -H 'Content-Type: application/json' -d '{"password":"a-sufficiently-long-password"}')"
check keys_authed   '200' "$(code -b "$COOKIE_JAR" "$BASE/api/keys")"
check backup_unauth '401' "$(code -X POST "$BASE/api/admin/backup")"
check backup_created '201' \
  "$(curl -sS -o "$WORK/backup.json" -w '%{http_code}' -b "$COOKIE_JAR" -X POST "$BASE/api/admin/backup?name=smoke")"
check backup_reports_files '"parquet_files":[0-9]+' "$(cat "$WORK/backup.json")"
check backup_contents 'MANIFEST.json' "$(ls "$WORK/data/backups/smoke")"
check backup_has_database 'mallard.duckdb' "$(ls "$WORK/data/backups/smoke")"
check backup_is_not_overwritten '409' "$(code -b "$COOKIE_JAR" -X POST "$BASE/api/admin/backup?name=smoke")"
check backup_rejects_a_path '400' "$(code -b "$COOKIE_JAR" -X POST "$BASE/api/admin/backup?name=..")"
# ── Share links and saved segments ────────────────────────────────────────
# A share link is a public credential, so it is checked end to end against the
# assembled binary: confinement, headers, what reaches the disk and the log.
check share_admin_unauth     '401' "$(code "$BASE/api/share-links")"
check segments_list_unauth   '401' "$(code "$BASE/api/segments?site_id=$SITE")"
SEGMENT="$(curl -sS -b "$COOKIE_JAR" -X POST "$BASE/api/segments" \
  -H 'Content-Type: application/json' \
  -d "{\"site_id\":\"$SITE\",\"name\":\"Chrome\",\"filters\":\"browsers==Chrome\"}")"
check segment_created        '"filters":"browsers==Chrome"' "$SEGMENT"
SEGMENT_ID="$(printf '%s' "$SEGMENT" | sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p')"
check segment_listed         'Chrome' "$(curl -sS -b "$COOKIE_JAR" "$BASE/api/segments?site_id=$SITE")"
check segment_rejects_junk   '400' \
  "$(code -b "$COOKIE_JAR" -X POST "$BASE/api/segments" -H 'Content-Type: application/json' \
      -d "{\"site_id\":\"$SITE\",\"name\":\"bad\",\"filters\":\"entry-pages==/\"}")"

LINK="$(curl -sS -b "$COOKIE_JAR" -X POST "$BASE/api/share-links" -H 'Content-Type: application/json' \
  -d "{\"label\":\"smoke\",\"site_id\":\"$SITE\",\"segment_id\":\"$SEGMENT_ID\",\"expires_in_days\":7}")"
check share_created          '"token":"ms_[0-9a-f]{64}"' "$LINK"
TOKEN="$(printf '%s' "$LINK" | sed -n 's/.*"token":"\(ms_[0-9a-f]*\)".*/\1/p')"
LINK_ID="$(printf '%s' "$LINK" | sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p')"
TOKEN_BODY="${TOKEN#ms_}"

viewer() { curl -sS -H "X-Share-Token: $TOKEN" "$@"; }
check share_session          '"segment":"Chrome"' "$(viewer "$BASE/api/shared/session")"
check share_view             '"unique_visitors":1' "$(viewer "$BASE/api/shared/stats/main?$Q")"
check share_view_site_filled '"unique_visitors":1' "$(viewer "$BASE/api/shared/stats/main?period=7d")"
check share_other_site       '403' "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/main?site_id=other.test&period=7d")"
check share_segment_holds    '"total_pageviews":0' \
  "$(viewer "$BASE/api/shared/stats/main?$Q&filters=browsers%21%3DChrome")"
check share_no_export        '404' "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/export?$Q&kind=raw")"
check share_no_sites         '404' "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/shared/sites")"
check share_is_not_a_key     '401' "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/stats/main?$Q")"
check share_is_not_a_bearer  '401' "$(code -H "Authorization: Bearer $TOKEN" "$BASE/api/stats/main?$Q")"
check share_is_not_admin     '401' "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/share-links")"
check share_post_refused     '405' "$(code -X POST -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/main?$Q")"
check share_no_token         '401' "$(code "$BASE/api/shared/stats/main?$Q")"
check share_bad_token        '401' "$(code -H 'X-Share-Token: ms_0000' "$BASE/api/shared/stats/main?$Q")"
SHARE_HEADERS="$(curl -sS -D - -o /dev/null -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/main?$Q")"
check share_no_referrer      'referrer-policy: no-referrer' "$(printf '%s' "$SHARE_HEADERS" | tr '[:upper:]' '[:lower:]')"
check share_no_store         'cache-control: .*no-store' "$(printf '%s' "$SHARE_HEADERS" | tr '[:upper:]' '[:lower:]')"
SHARE_LISTING="$(curl -sS -b "$COOKIE_JAR" "$BASE/api/share-links")"
check share_listed           '"label":"smoke"' "$SHARE_LISTING"
absent share_listing_has_no_token "$TOKEN_BODY" "$SHARE_LISTING"
absent share_listing_has_no_hash  'token_hash|"token"' "$SHARE_LISTING"
check share_file_mode        '^600$' "$(stat -c '%a' "$WORK/data/share_links.json")"
absent share_file_has_no_token "$TOKEN_BODY" "$(cat "$WORK/data/share_links.json")"
check share_dashboard_is_static '<div id="app">' "$(curl -sS "$BASE/")"
# A token pasted into a URL by mistake must not reach the log.
code "$BASE/s/$TOKEN" >/dev/null
code "$BASE/api/shared/stats/main?$Q&t=$TOKEN" >/dev/null

# ── Parquet export (admin) ────────────────────────────────────────────────
PQ="$BASE/api/admin/export/parquet?site_id=$SITE&period=7d"
check parquet_unauth '401' "$(code "$PQ")"
check parquet_bad_site '400' "$(code -b "$COOKIE_JAR" "$BASE/api/admin/export/parquet?site_id=..%2Fx&period=7d")"
check parquet_ok '200' \
  "$(curl -sS -o "$WORK/export.parquet" -D "$WORK/export.headers" -w '%{http_code}' -b "$COOKIE_JAR" "$PQ")"
check parquet_content_type 'application/vnd\.apache\.parquet' "$(cat "$WORK/export.headers")"
check parquet_disposition 'mallard-events-smoke\.test-.*\.parquet' "$(cat "$WORK/export.headers")"
check parquet_row_count '[Xx]-[Mm]allard-[Rr]ows: [1-9]' "$(cat "$WORK/export.headers")"
check parquet_magic_head 'PAR1' "$(head -c 4 "$WORK/export.parquet")"
check parquet_magic_tail 'PAR1' "$(tail -c 4 "$WORK/export.parquet")"
absent parquet_has_no_visitor_id 'visitor_id' "$(strings "$WORK/export.parquet" 2>/dev/null)"
sleep 1
check parquet_leaves_no_file '^0$' "$(find "$WORK/data/exports" -type f 2>/dev/null | wc -l | tr -d ' ')"
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

absent share_token_not_in_log "$TOKEN_BODY" "$(cat "$WORK/server.log")"

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
  # A share link, its segment and its revocation are state too.
  check share_survives_restart '"unique_visitors":1' \
    "$(curl -sS -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/main?$Q")"
  check share_revoke '200' "$(code -b "$JAR2" -X DELETE "$BASE/api/share-links/$LINK_ID")"
  check share_revoked_at_once '401' \
    "$(code -H "X-Share-Token: $TOKEN" "$BASE/api/shared/stats/main?$Q")"
  absent share_token_not_in_log_after_restart "$TOKEN_BODY" "$(cat "$WORK/server2.log")"
fi

# ── Importing another product's export ────────────────────────────────────
# The server must be stopped: DuckDB locks the file. The importer is the
# binary itself, so this is also the check that the subcommand is wired up.
FIXTURE="${MALLARD_IMPORT_FIXTURE:-tests/fixtures/ga4-bigquery-sample.ndjson}"
if [ -f "$FIXTURE" ]; then
  IMPORT_SITE="imported.test"
  imp() { MALLARD_DATA_DIR="$WORK/data" "$BIN" import ga4-bigquery --site-id "$IMPORT_SITE" "$@"; }
  # A dry run opens no database, so it works against the running server.
  check import_dry_run '(^|\n)valid_events: 6' "$(imp --file "$FIXTURE" --dry-run 2>&1)"
  # The real import must refuse while the server holds the lock.
  imp --file "$FIXTURE" > "$WORK/import-locked.log" 2>&1
  check import_refused_while_running '1' "$?"
  check import_says_why 'Stop it first' "$(cat "$WORK/import-locked.log")"
  # An invalid line aborts with its number and writes nothing.
  { head -n 2 "$FIXTURE"; echo 'not json'; } > "$WORK/bad.ndjson"
  imp --file "$WORK/bad.ndjson" --dry-run > "$WORK/import-bad.log" 2>&1
  check import_bad_line_exit '1' "$?"
  check import_bad_line_number 'line 3: not valid JSON' "$(cat "$WORK/import-bad.log")"

  kill "$SRV" 2>/dev/null
  wait "$SRV" 2>/dev/null
  check import_runs '(^|\n)inserted: 6' "$(imp --file "$FIXTURE" 2>&1)"
  check import_is_idempotent '(^|\n)inserted: 0' "$(imp --file "$FIXTURE" 2>&1)"
  check import_writes_partitions 'date=2026-03-14' \
    "$(ls "$WORK/data/events/site_id=$IMPORT_SITE")"

  MALLARD_HOST=127.0.0.1 MALLARD_PORT="$PORT" MALLARD_DATA_DIR="$WORK/data" \
    MALLARD_FLUSH_COUNT=1 MALLARD_FLUSH_INTERVAL=1 \
    "$BIN" > "$WORK/server-import.log" 2>&1 &
  SRV=$!
  for _ in $(seq 1 60); do curl -fsS "$BASE/health" >/dev/null 2>&1 && break; sleep 1; done
  JAR_IMP="$WORK/cookies-import"
  curl -sS -o /dev/null -c "$JAR_IMP" -X POST "$BASE/api/auth/login" \
    -H 'Content-Type: application/json' -d '{"password":"a-sufficiently-long-password"}'
  IQ="site_id=$IMPORT_SITE&start_date=2026-03-14&end_date=2026-03-14"
  IMPORTED_MAIN="$(curl -sS -b "$JAR_IMP" "$BASE/api/stats/main?$IQ")"
  check imported_pageviews '"total_pageviews":4' "$IMPORTED_MAIN"
  check imported_visitors '"unique_visitors":3' "$IMPORTED_MAIN"
  check imported_in_pages '/blog/duck-care' "$(curl -sS -b "$JAR_IMP" "$BASE/api/stats/breakdown/pages?$IQ")"
  absent imported_raw_has_no_ids 'imp:' \
    "$(curl -sS -b "$JAR_IMP" "$BASE/api/stats/export?$IQ&kind=raw&format=csv")"
  kill "$SRV" 2>/dev/null
  wait "$SRV" 2>/dev/null
else
  echo "note: $FIXTURE not found; skipping the import checks"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "smoke: $pass checks passed"
  exit 0
fi
echo "smoke: $fail failed, $pass passed"
exit 1
