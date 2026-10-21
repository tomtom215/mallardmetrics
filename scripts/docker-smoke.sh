#!/usr/bin/env bash
#
# Smoke-test a built Mallard Metrics container image the way it is deployed.
#
# Usage:  scripts/docker-smoke.sh [image]        (default: mallard-metrics:test)
# Exit:   0 when every check passes, 1 otherwise, 2 when docker is unusable.
#
# This is the check that would have caught the musl image: it passed its
# healthcheck and ingested events, yet could not install the `behavioral`
# extension, so half the product answered 503. It asserts, against the real
# image:
#   1. the container runs as UID 65532 with the production restrictions
#      (--read-only, all capabilities dropped, no-new-privileges), on a fresh
#      named volume, and reaches "healthy" through the image's own HEALTHCHECK;
#   2. /health/detailed reports a behavioral extension version;
#   3. the behavioral endpoints answer 200, not 503;
#   4. after a restart on an internal network with no route to the internet
#      (how docker-compose.production.yml runs it), the extension is loaded
#      from the data volume and the events written earlier are still there.
#
# Needs: docker, curl, outbound network for the first run (the extension is
# downloaded from community-extensions.duckdb.org), and a Linux host: the
# containers are probed on their bridge IP addresses, which Docker Desktop does
# not route to the host.

set -uo pipefail

IMAGE="${1:-mallard-metrics:test}"
RUN_ID="mallard-smoke-$$"
VOLUME="${RUN_ID}-data"
NET_INTERNAL="${RUN_ID}-internal"
SITE="docker-smoke.test"
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36'

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "error: docker is not available (client missing or daemon unreachable)" >&2
  exit 2
fi

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
# SC2329 is the name newer ShellCheck gives SC2317; disable both so the same
# comment holds on the runner image's version and a current one.
# shellcheck disable=SC2317,SC2329  # invoked through the EXIT trap
cleanup() {
  docker rm -f "${RUN_ID}-1" "${RUN_ID}-2" >/dev/null 2>&1
  docker network rm "$NET_INTERNAL" >/dev/null 2>&1
  docker volume rm "$VOLUME" >/dev/null 2>&1
}
trap cleanup EXIT

docker volume create "$VOLUME" >/dev/null
docker network create --internal "$NET_INTERNAL" >/dev/null

# Same restrictions as docker-compose.production.yml.
run_container() { # name network
  docker run -d --name "$1" --network "$2" \
    --read-only --tmpfs /tmp:size=64m,mode=1777 \
    --cap-drop ALL --security-opt no-new-privileges:true \
    --user 65532:65532 -v "$VOLUME:/data" \
    -e MALLARD_SECRET=docker-smoke-secret-not-used-anywhere-else \
    -e MALLARD_FLUSH_COUNT=1 -e MALLARD_FLUSH_INTERVAL=1 \
    "$IMAGE" >/dev/null
}
container_ip() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"; }
wait_healthy() { # name -> 0 when the image HEALTHCHECK reports healthy
  local status
  for _ in $(seq 1 90); do
    status="$(docker inspect -f '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$1" 2>/dev/null)"
    case "$status" in
      "running healthy") return 0 ;;
      exited*|dead*)     return 1 ;;
    esac
    sleep 2
  done
  return 1
}
curl_ip() { command curl -sS --max-time 20 -A "$UA" "$@"; }
logs_tail() { docker logs --tail 40 "$1" 2>&1; }

# ── Run 1: default network, so the extension can be downloaded ─────────────
run_container "${RUN_ID}-1" bridge || { echo "FAIL could not start the container"; exit 1; }
if ! wait_healthy "${RUN_ID}-1"; then
  echo "FAIL the container never became healthy (HEALTHCHECK or startup failure):"
  logs_tail "${RUN_ID}-1"
  exit 1
fi
IP="$(container_ip "${RUN_ID}-1")"
BASE="http://${IP}:8000"

check runs_as_65532 '^65532:65532$' "$(docker inspect -f '{{.Config.User}}' "${RUN_ID}-1")"
check read_only_root '^true$' "$(docker inspect -f '{{.HostConfig.ReadonlyRootfs}}' "${RUN_ID}-1")"

DETAILED="$(curl_ip "$BASE/health/detailed")"
check behavioral_loaded  '"behavioral_extension_loaded":true' "$DETAILED"
check behavioral_version '"behavioral_version":"[0-9]+\.[0-9]+' "$DETAILED"
check behavioral_metric  'mallard_behavioral_extension 1' "$(curl_ip "$BASE/metrics")"

TODAY="$(date -u +%F)"
START="$(date -u -d '7 days ago' +%F)"
Q="site_id=$SITE&start_date=$START&end_date=$TODAY"
for path in / /pricing; do
  curl_ip -o /dev/null -X POST "$BASE/api/event" -H 'Content-Type: application/json' \
    -d "{\"d\":\"$SITE\",\"n\":\"pageview\",\"u\":\"https://$SITE$path\",\"w\":1440}"
done
sleep 4 # MALLARD_FLUSH_INTERVAL=1
code() { curl_ip -o /dev/null -w '%{http_code}' "$@"; }
check sessions_200 '^200$' "$(code "$BASE/api/stats/sessions?$Q")"
check funnel_200   '^200$' "$(code "$BASE/api/stats/funnel?$Q&steps=page%3A%2F%2Cpage%3A%2Fpricing&window=1%20day")"
check flow_200     '^200$' "$(code "$BASE/api/stats/flow?$Q&page=%2F")"
check events_stored '"unique_visitors":1' "$(curl_ip "$BASE/api/stats/main?$Q")"

# The extension must now be on the volume, not in the (read-only) image.
docker stop -t 45 "${RUN_ID}-1" >/dev/null
docker rm "${RUN_ID}-1" >/dev/null

# ── Run 2: internal network, no route out: extension must load from /data ──
run_container "${RUN_ID}-2" "$NET_INTERNAL" || { echo "FAIL could not start the second container"; exit 1; }
if ! wait_healthy "${RUN_ID}-2"; then
  echo "FAIL the container did not become healthy without network access:"
  logs_tail "${RUN_ID}-2"
  exit 1
fi
BASE="http://$(container_ip "${RUN_ID}-2"):8000"
check offline_behavioral_loaded '"behavioral_extension_loaded":true' "$(curl_ip "$BASE/health/detailed")"
check offline_sessions_200 '^200$' "$(code "$BASE/api/stats/sessions?$Q")"
check offline_data_survived '"unique_visitors":1' "$(curl_ip "$BASE/api/stats/main?$Q")"

echo
if [ "$fail" -eq 0 ]; then
  echo "docker-smoke: $pass checks passed ($IMAGE)"
  exit 0
fi
echo "docker-smoke: $fail failed, $pass passed ($IMAGE)"
exit 1
