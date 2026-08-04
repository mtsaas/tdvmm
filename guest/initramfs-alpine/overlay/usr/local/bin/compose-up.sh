#!/bin/sh
# tdvmm Phase-2a stack launcher (closed world, no runtime network).
#
# Replaces the hand-rolled workload.sh with the GENERIC compose path:
#   1. start `podman system service` (Docker-compatible API) with an IDLE TIMEOUT,
#   2. `docker compose -f compose.lock.yml up --pull=never -d` against it,
#   3. after the stack is up, the API service idle-EXITS -- so the guest is left
#      holding only conmon + the workload containers (NO resident engine). We
#      print a process census proving it.
#   4. enforce/observe the closed world (no egress route; external DNS fails),
#   5. stream the containers' logs to the serial console forever.
#
# Everything is pre-baked; compose runs --pull=never (zero guest-runtime network).
# The machine never powers off in this mode; the host acceptance test watches the
# serial markers and stops the VMM when it has its proof.
#
# Cadence/cap knobs come from the kernel cmdline (tdvmm.interval= / tdvmm.maxrows=)
# and are handed to compose via ${TDVMM_INTERVAL}/${TDVMM_MAXROWS} interpolation, so
# one baked lockfile serves both the fast FF demo and the small smoke test.
set -u

LOCK=/var/lib/tdvmm-stack/compose.lock.yml
SOCK=/run/podman/podman.sock
DOCKER_HOST="unix://$SOCK"; export DOCKER_HOST
IDLE_TIMEOUT="${TDVMM_ENGINE_IDLE:-30}"     # podman system service idle-exit seconds
PROJECT="$(cat /etc/tdvmm-stack-project 2>/dev/null)"
[ -n "$PROJECT" ] && export COMPOSE_PROJECT_NAME="$PROJECT"

# cadence/cap knobs (kernel cmdline overrides the lockfile defaults)
# tdvmm.memsample=1 opts into the peak-RAM console sampler below (default OFF).
TDVMM_MEMSAMPLE=0
for tok in $(cat /proc/cmdline 2>/dev/null); do
  case "$tok" in
    tdvmm.interval=*)  export TDVMM_INTERVAL="${tok#tdvmm.interval=}" ;;
    tdvmm.maxrows=*)   export TDVMM_MAXROWS="${tok#tdvmm.maxrows=}" ;;
    tdvmm.memsample=*) TDVMM_MEMSAMPLE="${tok#tdvmm.memsample=}" ;;
  esac
done

fail() { echo "TDVMM_STACK_FAIL: $*"; podman ps -a 2>/dev/null | sed 's/^/[stack][ps] /'; exit 1; }

[ -f "$LOCK" ] || fail "no compose.lock.yml at $LOCK (guest not baked in stack mode)"
command -v docker-compose >/dev/null 2>&1 || fail "docker-compose engine not installed"

echo "TDVMM_STACK_START project=$PROJECT interval=${TDVMM_INTERVAL:-<lock>} max_rows=${TDVMM_MAXROWS:-<lock>}"
echo "[stack] compose.lock.yml:"; sed 's/^/[stack][lock] /' "$LOCK"

# 1. start the Docker-compatible API with an idle timeout -----------------------
mkdir -p /run/podman
echo "[stack] starting podman system service (idle-exit ${IDLE_TIMEOUT}s)"
podman system service --time="$IDLE_TIMEOUT" "$DOCKER_HOST" >/tmp/podman-service.log 2>&1 &
ENGINE_PID=$!

# wait for the API socket
tries=0
until [ -S "$SOCK" ]; do
  tries=$((tries + 1)); [ "$tries" -gt 100 ] && { sed 's/^/[stack][engine] /' /tmp/podman-service.log; fail "API socket never appeared"; }
  sleep 0.1
done
echo "TDVMM_ENGINE_UP pid=$ENGINE_PID sock=$SOCK"

# 1b. healthcheck ticker (2b items 3&4) -----------------------------------------
# If the stack declares any healthcheck, start the guest-side ticker BEFORE
# `compose up`, because a depends_on: {condition: service_healthy} gate makes
# `up` BLOCK until the dependency is healthy -- and podman has no auto-runner to
# get it there without systemd. The ticker discovers the containers as compose
# creates them and runs their checks, so the gate resolves and `up` proceeds.
# Skipped entirely for stacks with no healthcheck (zero extra wakeups).
HC_PID=""
if grep -qE '^[[:space:]]*healthcheck:' "$LOCK"; then
  echo "[stack] starting healthcheck ticker (podman has no systemd auto-runner)"
  ( /usr/local/bin/healthcheck-ticker.sh 2>&1 | sed 's/^/[hc] /' ) &
  HC_PID=$!
fi

# 2. compose up (offline) -------------------------------------------------------
echo "[stack] docker compose up --pull=never -d"
if docker-compose -f "$LOCK" up --pull never -d >/tmp/compose-up.log 2>&1; then
  sed 's/^/[stack][up] /' /tmp/compose-up.log
  echo "TDVMM_STACK_UP"
else
  sed 's/^/[stack][up] /' /tmp/compose-up.log
  fail "compose up"
fi

# 3. prove the engine idle-exits, then take a process census --------------------
#    (background: logs stream in the foreground below while we wait for the exit).
census() {
  t=0
  while kill -0 "$ENGINE_PID" 2>/dev/null; do
    t=$((t + 1)); [ "$t" -gt 120 ] && break   # 60s guard
    sleep 0.5
  done
  if kill -0 "$ENGINE_PID" 2>/dev/null; then
    echo "TDVMM_ENGINE_RESIDENT WARNING: podman system service still running after idle window"
  else
    echo "TDVMM_ENGINE_EXITED podman system service has exited (no resident engine)"
  fi
  echo "==== TDVMM_PROCESS_CENSUS (after compose up; engine exited) ===="
  # busybox ps: show every process; the census must be conmon + container procs
  # + this launcher + log tailers -- and NO `podman system service`.
  ps -o pid,args 2>/dev/null || ps 2>/dev/null || ps aux 2>/dev/null
  echo "---- podman containers (via CLI, no API) ----"
  podman ps --format '{{.Names}} {{.Status}} {{.Image}}' 2>/dev/null | sed 's/^/[census] /'
  if pgrep -f 'system service' >/dev/null 2>&1; then
    echo "TDVMM_CENSUS_ENGINE_PRESENT (unexpected)"
  else
    echo "TDVMM_CENSUS_NO_ENGINE ok: no 'podman system service' process"
  fi
  echo "==== TDVMM_PROCESS_CENSUS_END ===="

  # 4. closed-world observations ------------------------------------------------
  if [ -z "$(ip route show default 2>/dev/null)" ]; then
    echo "TDVMM_CLOSED_WORLD_OK no default route on the guest (egress cannot leave)"
  else
    echo "TDVMM_CLOSED_WORLD_WARN a default route exists: $(ip route show default 2>/dev/null)"
  fi
  # best-effort: external name resolution from inside a container must fail.
  c="$(podman ps --format '{{.Names}}' 2>/dev/null | head -1)"
  if [ -n "$c" ]; then
    if podman exec "$c" getent hosts example.com >/dev/null 2>&1; then
      echo "TDVMM_EGRESS_DNS_WARN external lookup resolved (egress DNS reachable)"
    else
      echo "TDVMM_CLOSED_WORLD_DNS_OK external lookup failed (service-name DNS only)"
    fi
  fi
}
census &

# RAM sampler (opt-in via tdvmm.memsample=1): lets the host record actual peak
# guest RAM (MemTotal - min MemAvail). OFF by default so a normal run -- and
# especially a fast-forward run -- is never flooded with TDVMM_MEM console lines;
# scripts/smoke_test_workload.sh passes tdvmm.memsample=1 to collect its samples.
if [ "$TDVMM_MEMSAMPLE" = "1" ]; then
  ( while :; do
      grep -E '^(MemTotal|MemFree|MemAvailable|Cached):' /proc/meminfo \
        | awk '{printf "%s%s ", $1, $2} END{print ""}' | sed 's/^/TDVMM_MEM /'
      sleep 5
    done ) &
fi

# 5. stream every container's logs to serial, forever (podman CLI reads the log
#    files directly -- it does NOT revive the API engine).
sleep 1
NAMES="$(podman ps --format '{{.Names}}' 2>/dev/null)"
[ -n "$NAMES" ] || fail "no containers running after compose up"
for n in $NAMES; do
  ( podman logs -f "$n" 2>&1 | sed "s/^/[$n] /" ) &
done
echo "TDVMM_STACK_STREAMING $(echo "$NAMES" | tr '\n' ' ')"
wait
