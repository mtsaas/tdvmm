#!/bin/sh
# deterministic-vmm corpus (webstack) api service.
#
# Runs as the built api image's entrypoint. Because compose gates this service on
# `postgres: service_healthy` AND `redis: service_healthy`, reaching this code at
# all proves BOTH health gates resolved (the guest-side ticker flipped both to
# healthy and compose unblocked). It then re-proves reachability to both backends
# BY NAME over the compose network, and every INTERVAL_SECONDS inserts a row into
# Postgres, trims to MAX_ROWS, and bumps a Redis request counter -- printing one
# greppable marker per cycle so the corpus runner can assert functional
# correctness + cadence under fast-forward.
set -u

PGHOST="${PGHOST:-postgres}"
PGUSER="${PGUSER:-postgres}"
PGDATABASE="${PGDATABASE:-appdb}"
export PGHOST PGUSER PGDATABASE
export PGCONNECT_TIMEOUT=5
REDIS_HOST="${REDIS_HOST:-redis}"
INTERVAL_SECONDS="${INTERVAL_SECONDS:-3600}"
MAX_ROWS="${MAX_ROWS:-1000}"

echo "DVMM_API_UP both deps healthy: pg=$PGHOST redis=$REDIS_HOST interval=${INTERVAL_SECONDS}s max_rows=$MAX_ROWS"

# Re-prove backend reachability by name (the health gate already guaranteed both
# are healthy; this confirms the api can actually talk to them).
if pg_isready -q -h "$PGHOST" -U "$PGUSER"; then echo "DVMM_API_PG_OK"; else echo "DVMM_API_PG_FAIL"; fi
PONG="$(redis-cli -h "$REDIS_HOST" ping 2>/dev/null)"
echo "DVMM_API_REDIS_PING=${PONG:-none}"

# Idempotent schema guard (Postgres also runs the baked schema on first start).
if ! psql -v ON_ERROR_STOP=1 -q -c \
  "CREATE TABLE IF NOT EXISTS events (id bigserial PRIMARY KEY, ts timestamptz NOT NULL DEFAULT now(), value text);"
then
  echo "DVMM_SVC_FAIL create-table"; exit 1
fi

i=0
while :; do
  i=$((i + 1))
  if ! psql -v ON_ERROR_STOP=1 -q -c "INSERT INTO events(value) VALUES ('tick-$i');"; then
    echo "DVMM_SVC_FAIL insert iter=$i"; exit 1
  fi
  if ! psql -v ON_ERROR_STOP=1 -q -c \
    "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT $MAX_ROWS);"
  then
    echo "DVMM_SVC_FAIL trim iter=$i"; exit 1
  fi
  n="$(psql -tAX -v ON_ERROR_STOP=1 -c 'SELECT count(*) FROM events;')"
  [ -z "$n" ] && { echo "DVMM_SVC_FAIL count iter=$i"; exit 1; }
  # bump a cache counter in Redis (proves the cache is live + reachable by name).
  hits="$(redis-cli -h "$REDIS_HOST" incr requests 2>/dev/null)"
  echo "DVMM_ROWCOUNT=$n iter=$i max=$MAX_ROWS cache_requests=${hits:-?} ts=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  sleep "$INTERVAL_SECONDS"
done
