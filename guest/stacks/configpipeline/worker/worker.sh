#!/bin/sh
# dvmm corpus (configpipeline) worker service.
#
# Proves, in one service, three supported-subset capabilities at once:
#   (a) the relative RW bind was MATERIALIZED into the guest image -- it reads the
#       baked ./config/app.conf seed (DVMM_CONFIG_SEED);
#   (b) the RW bind is WRITABLE -- it writes a file under /etc/app and reads it
#       back (DVMM_CONFIG_WRITE_OK);
#   (c) the NAMED VOLUME is writable and SHARED -- it writes /var/state/latest.txt
#       each cycle (DVMM_STATE_WRITE_OK), which the sidecar reads back.
# All writes are ephemeral (guest tmpfs) -- fine in the closed, single-writer world.
set -u

INTERVAL_SECONDS="${INTERVAL_SECONDS:-3600}"
CFG=/etc/app/app.conf
STATE=/var/state/latest.txt

echo "DVMM_WORKER_START interval=${INTERVAL_SECONDS}s"
# (a) materialized RW bind: read the baked seed token.
echo "DVMM_CONFIG_SEED=$(grep '^seed=' "$CFG" 2>/dev/null | cut -d= -f2)"
# (b) RW bind write + read back.
echo "generated-by-worker" > /etc/app/generated.txt 2>/dev/null
echo "DVMM_CONFIG_WRITE_OK=$(cat /etc/app/generated.txt 2>/dev/null)"

# (c) publish to the shared named volume each cycle; the sidecar consumes it.
i=0
while :; do
  i=$((i + 1))
  echo "worker-iter-$i" > "$STATE"
  echo "DVMM_STATE_WRITE_OK iter=$i wrote=$(cat "$STATE" 2>/dev/null) ts=$(date -u '+%H:%M:%S')"
  sleep "$INTERVAL_SECONDS"
done
