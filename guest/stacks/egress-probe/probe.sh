#!/bin/sh
# tdvmm egress-probe -- exercises the --allow-egress SOCKS5h path and emits
# greppable TDVMM_EGRESS_* sentinels + assertion events for the safety suite.
#
# Bind-mounted read-only into the pinned curl image and run as its entrypoint. It
# reaches the guest forwarder at its BRIDGE GATEWAY on :1080 (socks5h, so the host
# resolves the hostname), and the host connects to the test-owned loopback endpoint
# the URL names (127.0.0.1:<ephemeral>). No internet is ever involved.
#
# The behaviour is selected by tdvmm.egress_mode= on the kernel cmdline (readable
# from a container -- /proc/cmdline is not namespaced), with the endpoint URL and
# timing knobs alongside it. Each mode emits one `always` assertion event (folded
# into the scenario verdict) plus a raw sentinel line the shell suite greps for the
# numeric evidence, then `done` to end the run.
set -u

EV=/run/tdvmm/events
# emit <kind> <name> <ok> [details_json]. The event rides the FAST path (FIFO ->
# agent -> ttyS1 -> host JSONL) so its verdict bit AND its `details` (the numeric
# evidence) land reliably in the run-log -- unlike the container's stdout, which
# streams to serial asynchronously and can race the run-ending `done`.
emit()   { d="${4:-null}"; [ -w "$EV" ] && echo "{\"kind\":\"$1\",\"name\":\"$2\",\"ok\":$3,\"details\":$d}" > "$EV" 2>/dev/null || true; }
done_ev() { [ -w "$EV" ] && echo '{"kind":"done"}' > "$EV" 2>/dev/null || true; }
idle()   { while :; do sleep 3600; done; }   # stay alive (blocked -> FF-transparent)

getcmd() { # getcmd key default
  v=$(tr ' ' '\n' < /proc/cmdline 2>/dev/null | sed -n "s/^$1=//p" | head -1)
  [ -n "$v" ] && echo "$v" || echo "$2"
}
uptime_s() { awk '{print $1; exit}' /proc/uptime; }   # monotonic (virtual) seconds

MODE=$(getcmd tdvmm.egress_mode t5)
URL=$(getcmd tdvmm.egress_url http://127.0.0.1:80/)
D=$(getcmd tdvmm.egress_d 2)
NAPS=$(getcmd tdvmm.egress_sleep 600)
MAXTIME=$(getcmd tdvmm.egress_maxtime 30)

# The proxy host is this container's default gateway (the guest forwarder listens
# on 0.0.0.0:1080 in the guest netns, reachable at the bridge gateway).
GW=$(ip route show default 2>/dev/null | awk '/default/{print $3; exit}')
[ -z "$GW" ] && GW=10.88.0.1
PROXY="socks5h://$GW:1080"

echo "TDVMM_EGRESS_PROBE_START mode=$MODE url=$URL proxy=$PROXY d=$D sleep=$NAPS maxtime=$MAXTIME"

# The forwarder is started at guest init, well before compose finishes; give the
# bridge a moment to come up before the (timing-sensitive) first request.
sleep 2

case "$MODE" in
  t1)
    # Clock must NOT jump while the request is in flight: the endpoint holds D real
    # seconds; under correct gating the guest's monotonic clock advances ~D, not the
    # orders-of-magnitude more a fast-forward would produce.
    u0=$(uptime_s)
    http=$(curl -sS --proxy "$PROXY" --max-time 60 -o /tmp/body -w '%{http_code}' "$URL" 2>/tmp/err); rc=$?
    u1=$(uptime_s)
    el=$(awk "BEGIN{print $u1-$u0}")
    echo "TDVMM_EGRESS_T1 rc=$rc http=$http virtual_elapsed=$el d=$D body=$(cat /tmp/body 2>/dev/null)"
    ok=false
    if [ "$rc" = 0 ] && [ "$http" = 200 ] && grep -q EGRESSOK /tmp/body 2>/dev/null; then
      ok=$(awk "BEGIN{print ($el>=($D-0.5) && $el<=($D+2))?\"true\":\"false\"}")
    fi
    echo "TDVMM_EGRESS_T1_OK=$ok"
    emit always t1_no_jump "$ok" "{\"virtual_elapsed\":$el,\"d\":$D}"
    ;;

  t2)
    # FF re-engages once the session drains: do a quick request (server closes), then
    # sleep a long virtual interval. Fast-forward must collapse it back to ~no real
    # time -- proven host-side (jumps>0, virtual>=NAPS, real small) via --metrics-out.
    http=$(curl -sS --proxy "$PROXY" --retry 5 --retry-connrefused --retry-delay 1 \
           --max-time 30 -o /tmp/body -w '%{http_code}' "$URL" 2>/tmp/err); rc=$?
    echo "TDVMM_EGRESS_T2_REQ rc=$rc http=$http body=$(cat /tmp/body 2>/dev/null)"
    u0=$(uptime_s); sleep "$NAPS"; u1=$(uptime_s)
    slept=$(awk "BEGIN{print $u1-$u0}")
    echo "TDVMM_EGRESS_T2_SLEEP virtual_slept=$slept target=$NAPS"
    ok=$(awk "BEGIN{print ($rc==0 && $http==200 && $slept>=($NAPS*0.95))?\"true\":\"false\"}")
    echo "TDVMM_EGRESS_T2_OK=$ok"
    emit always t2_ff_reengaged "$ok" "{\"virtual_slept\":$slept}"
    ;;

  t3b)
    # Negative control: hold a connection open so a session is established, then let
    # the guest idle. With TDVMM_EGRESS_UNSAFE_JUMPS=1 the VMM attempts a jump against
    # the open session and the always-on tripwire ABORTS the run -- so this curl
    # normally never returns. Reaching the line below means the gate did NOT fire.
    echo "TDVMM_EGRESS_T3B_OPEN holding a connection open (expect the run to abort)"
    curl -sS --proxy "$PROXY" --retry 5 --retry-connrefused --retry-delay 1 \
         --max-time 60 -o /tmp/body "$URL" 2>/tmp/err; rc=$?
    echo "TDVMM_EGRESS_T3B_DONE rc=$rc (reached only if the gate did NOT abort)"
    emit always t3b_not_aborted false
    ;;

  t3c)
    # Adversarial dribble: the endpoint drips the body one byte per real interval. The
    # idle gaps must NOT be fast-forwarded past the guest's read timeout, or the read
    # never completes. Passes iff curl reads the whole body within --max-time.
    u0=$(uptime_s)
    http=$(curl -sS --proxy "$PROXY" --max-time "$MAXTIME" -o /tmp/body -w '%{http_code}' "$URL" 2>/tmp/err); rc=$?
    u1=$(uptime_s)
    el=$(awk "BEGIN{print $u1-$u0}")
    n=$(wc -c < /tmp/body 2>/dev/null | tr -d ' ')
    echo "TDVMM_EGRESS_T3C rc=$rc http=$http bytes=$n virtual_elapsed=$el maxtime=$MAXTIME"
    ok=$(awk "BEGIN{print ($rc==0 && $http==200 && $n==6)?\"true\":\"false\"}")
    echo "TDVMM_EGRESS_T3C_OK=$ok"
    emit always t3c_dribble_completed "$ok" "{\"bytes\":$n,\"virtual_elapsed\":$el}"
    ;;

  t5)
    # Transport control under --ff off: no gating involved, just the SOCKS5h -> mux
    # -> backend -> socket -> response path. Passes iff the response comes back whole.
    http=$(curl -sS --proxy "$PROXY" --retry 5 --retry-connrefused --retry-delay 1 \
           --max-time 30 -o /tmp/body -w '%{http_code}' "$URL" 2>/tmp/err); rc=$?
    echo "TDVMM_EGRESS_T5 rc=$rc http=$http body=$(cat /tmp/body 2>/dev/null)"
    ok=false
    [ "$rc" = 0 ] && [ "$http" = 200 ] && grep -q EGRESSOK /tmp/body 2>/dev/null && ok=true
    echo "TDVMM_EGRESS_T5_OK=$ok"
    emit always t5_transport_ok "$ok" "{\"http\":$http}"
    ;;

  t6)
    # Closed-world identity (flag OFF): the forwarder is not running, so the proxy
    # connect must fail fast. Passes iff curl could NOT reach the proxy.
    http=$(curl -sS --proxy "$PROXY" --connect-timeout 5 --max-time 15 \
           -o /tmp/body -w '%{http_code}' "$URL" 2>/tmp/err); rc=$?
    echo "TDVMM_EGRESS_T6 rc=$rc http=$http (expect a nonzero rc -- no forwarder)"
    ok=false
    [ "$rc" != 0 ] && ok=true
    echo "TDVMM_EGRESS_T6_OK=$ok"
    emit always t6_closed_world "$ok" "{\"rc\":$rc}"
    # Stay alive past the compose census's engine idle-exit (~30 virtual s) so its
    # TDVMM_CLOSED_WORLD_OK marker prints before `done` ends the run. Fast-forward
    # collapses this wait to ~no real time (egress is off, the guest is idle).
    sleep 60
    ;;

  *)
    echo "TDVMM_EGRESS_PROBE_UNKNOWN_MODE $MODE"
    emit always probe_mode_known false
    ;;
esac

done_ev
idle
