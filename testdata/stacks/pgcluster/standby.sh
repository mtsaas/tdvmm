#!/bin/sh
# tdvmm pgcluster — the streaming standby's entrypoint.
#
# Clone the primary with pg_basebackup, then run as a hot standby. `-R` writes
# standby.signal and primary_conninfo, so the server comes up in recovery and
# starts streaming on its own.
#
# Runs as ROOT and drops to postgres with su-exec (the image's own mechanism),
# because the anonymous volume mounted at PGDATA arrives root-owned and postgres
# refuses to start on a data directory it does not own. That chown is exactly
# what the image's normal entrypoint does before dropping privileges; we bypassed
# that entrypoint to clone instead of initdb, so we have to do it ourselves.
#
# The clone is skipped if PGDATA already holds a cluster, so a container that is
# stopped and started again by a fault-injection test recovers instead of
# re-cloning (which is exactly the behavior a recovery test wants to observe).
set -e

PGDATA="${PGDATA:-/var/lib/postgresql/data}"
PRIMARY="${PRIMARY_HOST:-pg-primary}"

mkdir -p "$PGDATA"
chown -R postgres:postgres "$PGDATA"
chmod 0700 "$PGDATA"

echo "[standby] waiting for $PRIMARY to accept connections"
until su-exec postgres pg_isready -h "$PRIMARY" -U postgres -q; do
  sleep 1
done

if [ ! -s "$PGDATA/PG_VERSION" ]; then
  echo "[standby] cloning $PRIMARY with pg_basebackup"
  rm -rf "${PGDATA:?}"/* 2>/dev/null || true
  su-exec postgres pg_basebackup -h "$PRIMARY" -U postgres -D "$PGDATA" -Fp -Xs -R
  echo "[standby] clone complete"
else
  echo "[standby] existing cluster found; recovering instead of re-cloning"
fi

# `wal_receiver_timeout=0` keeps a partitioned standby from tearing the stream
# down on a timer — the point of the test is the partition, not a reconnect race.
exec su-exec postgres postgres \
  -c bgwriter_delay=10000 \
  -c wal_writer_delay=10000 \
  -c checkpoint_timeout=3600 \
  -c autovacuum=off \
  -c hot_standby=on \
  -c wal_receiver_timeout=0 \
  -c wal_receiver_status_interval=1
