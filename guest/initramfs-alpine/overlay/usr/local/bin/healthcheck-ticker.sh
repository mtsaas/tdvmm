#!/bin/sh
# tdvmm guest-side healthcheck ticker (Phase 2b, items 3&4).
#
# WHY THIS EXISTS. podman has NO healthcheck auto-runner without a systemd timer,
# and this guest runs busybox init (no systemd). Left alone, a container's health
# stays "starting" forever, so any compose `depends_on: {condition:
# service_healthy}` gate would hang the whole `compose up`. This ticker IS that
# runner: on a fixed cadence it runs `podman healthcheck run <c>` for every
# running container that DECLARES a healthcheck, so podman evaluates the check and
# flips the container's health to "healthy" -- which compose (polling the
# Docker-compat API) then observes, unblocking the gate and starting dependents.
#
# FF-FRIENDLY. This is a plain `sleep` loop, NOT a busy poll: between ticks the
# process genuinely blocks, so the guest HLTs and fast-forward collapses the
# waits. The periodic wakes are cheap and realistic (Fable's ruling).
#
# It talks to the store via the podman CLI (like `podman logs`/`podman ps` in
# compose-up.sh) -- short-lived invocations, NOT a resident engine, so the
# "no resident engine after compose up" property still holds.
#
# Only started by compose-up.sh WHEN the baked compose.lock.yml declares a
# healthcheck, so stacks without healthchecks get ZERO extra wakeups.
set -u

# Tick cadence in seconds. A small default; overridable via tdvmm.hc_tick= on the
# kernel cmdline. Test stacks set their healthcheck interval to match this.
TICK="${TDVMM_HC_TICK:-2}"
for tok in $(cat /proc/cmdline 2>/dev/null); do
  case "$tok" in tdvmm.hc_tick=*) TICK="${tok#tdvmm.hc_tick=}" ;; esac
done

# running containers that declare a healthcheck (Test has >0 elements).
hc_list() {
  for c in $(podman ps --format '{{.Names}}' 2>/dev/null); do
    n="$(podman inspect "$c" \
          --format '{{if .Config.Healthcheck}}{{len .Config.Healthcheck.Test}}{{else}}0{{end}}' \
          2>/dev/null)"
    case "${n:-0}" in ''|0) : ;; *) echo "$c" ;; esac
  done
}

status_of() {
  podman inspect "$1" \
    --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
    2>/dev/null
}

echo "TDVMM_HC_TICKER_START tick=${TICK}s"
ANNOUNCED=" "   # space-delimited set of containers already reported healthy
while :; do
  for c in $(hc_list); do
    podman healthcheck run "$c" >/dev/null 2>&1
    st="$(status_of "$c")"
    echo "TDVMM_HC_RUN container=$c status=${st:-unknown} ts=$(date -u +%H:%M:%S)"
    if [ "$st" = "healthy" ]; then
      case "$ANNOUNCED" in
        *" $c "*) : ;;
        *) echo "TDVMM_HC_HEALTHY container=$c ts=$(date -u +%H:%M:%S)"
           ANNOUNCED="$ANNOUNCED$c " ;;
      esac
    fi
  done
  sleep "$TICK"
done
