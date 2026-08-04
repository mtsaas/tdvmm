#!/bin/sh
# dvmm TigerBeetle stack -- replica bootstrap (closed world).
#
# ADDRESSING (the main unknown, SOLVED):
#   TigerBeetle's --addresses accepts ONLY IPv4/IPv6 literals -- passing compose
#   service names is rejected at startup with:
#       error(vsr): --addresses: invalid IPv4 or IPv6 address
#   To stay inside the closed world (netavark DNS only; no host networking, no
#   hard-coded host IPs) we RESOLVE the three replica service NAMES to their
#   netavark-assigned IPs at start, then build the --addresses list from those.
#
#   getent (musl, nsswitch "hosts: files dns") resolves service names against
#   aardvark-dns correctly. busybox `nslookup` does NOT -- it fails against
#   aardvark-dns with "write to '...': Message too large" -- so we use getent.
#
#   The list is built in FIXED replica order (replica0,replica1,replica2) on
#   every replica and the client, because TigerBeetle requires addresses[i] to
#   correspond to replica i and the order to match across the whole cluster.
#
# --development is REQUIRED here for two reasons:
#   1. the data file lives on the guest's ephemeral (RAM-backed) layer, which is
#      tmpfs -- tmpfs does not support Direct IO, and without --development
#      TigerBeetle refuses to start ("SystemOutdated" / Direct IO unavailable);
#   2. it selects the smallest batch/cache sizes. Combined with an explicit tiny
#      --cache-grid this is the smallest memory footprint TigerBeetle offers.
#      (See the compose.yml header for the RAM finding -- it is still too large.)
set -eu

N="$1"

resolve() { getent hosts "$1" | awk '{print $1; exit}'; }

A0=""; A1=""; A2=""; i=0
while [ "$i" -lt 120 ]; do
  A0=$(resolve replica0 || true)
  A1=$(resolve replica1 || true)
  A2=$(resolve replica2 || true)
  [ -n "$A0" ] && [ -n "$A1" ] && [ -n "$A2" ] && break
  i=$((i + 1)); sleep 1
done

ADDRESSES="$A0:3000,$A1:3000,$A2:3000"
echo "replica$N resolved addresses=$ADDRESSES"
# Publish for scenario execs (they source this to reach the cluster by IP).
echo "ADDRESSES=$ADDRESSES" > /tmp/tb-addresses

DATA="/data/$N.tigerbeetle"
mkdir -p /data
if [ ! -f "$DATA" ]; then
  /tigerbeetle format --cluster=0 --replica="$N" --replica-count=3 --development "$DATA"
fi

exec /tigerbeetle start \
  --addresses="$ADDRESSES" \
  --cache-grid=64MiB \
  --development \
  "$DATA"
