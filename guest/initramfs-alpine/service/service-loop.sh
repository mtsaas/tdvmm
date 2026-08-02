#!/bin/sh
# deterministic-vmm Step 2b workload service.
#
# A tiny closed-world service: every INTERVAL_SECONDS it INSERTs one row into
# Postgres and TRIMs the table down to at most MAX_ROWS newest rows, forever.
#
# Deliberately a POSIX-shell sleep-loop (NOT Go/JVM): their runtimes wake the
# CPU far too often and would wreck the later idle/fast-forward profile. Between
# inserts this process genuinely blocks in `sleep`, so the guest HLTs when idle.
#
# It reaches Postgres by NAME ("postgres") over the netavark bridge (aardvark
# DNS) -- no host networking. It prints one greppable marker per cycle:
#
#   DVMM_ROWCOUNT=<n> iter=<i> max=<MAX_ROWS> ...
#
# so the host acceptance test can prove rows accumulate at the interval and then
# cap at MAX_ROWS (never exceeding) as inserts continue past the cap.
set -u

PGHOST="${PGHOST:-postgres}"
PGPORT="${PGPORT:-5432}"
PGUSER="${PGUSER:-postgres}"
PGDATABASE="${PGDATABASE:-appdb}"
export PGHOST PGPORT PGUSER PGDATABASE
export PGCONNECT_TIMEOUT=5

INTERVAL_SECONDS="${INTERVAL_SECONDS:-3600}"
MAX_ROWS="${MAX_ROWS:-1000}"

echo "DVMM_SVC_START host=$PGHOST db=$PGDATABASE interval=${INTERVAL_SECONDS}s max_rows=$MAX_ROWS"

# 1. Wait for Postgres to accept connections (retry loop over pg_isready).
until pg_isready -q -h "$PGHOST" -p "$PGPORT" -U "$PGUSER"; do
  echo "DVMM_SVC_WAIT postgres not ready yet"
  sleep 1
done
echo "DVMM_SVC_PG_READY"

# 2. Ensure the table exists. Postgres also creates it on first start (see the
#    baked /docker-entrypoint-initdb.d schema); this is an idempotent guard.
if ! psql -v ON_ERROR_STOP=1 -q -c \
  "CREATE TABLE IF NOT EXISTS events (id bigserial PRIMARY KEY, ts timestamptz NOT NULL DEFAULT now(), value text);"
then
  echo "DVMM_SVC_FAIL create-table"
  exit 1
fi

# 3. Insert one row, trim to MAX_ROWS newest rows, report the count -- forever.
i=0
while :; do
  i=$((i + 1))

  if ! psql -v ON_ERROR_STOP=1 -q -c \
    "INSERT INTO events(value) VALUES ('tick-$i');"
  then
    echo "DVMM_SVC_FAIL insert iter=$i"
    exit 1
  fi

  if ! psql -v ON_ERROR_STOP=1 -q -c \
    "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT $MAX_ROWS);"
  then
    echo "DVMM_SVC_FAIL trim iter=$i"
    exit 1
  fi

  n="$(psql -tAX -v ON_ERROR_STOP=1 -c 'SELECT count(*) FROM events;')"
  if [ -z "$n" ]; then
    echo "DVMM_SVC_FAIL count iter=$i"
    exit 1
  fi

  echo "DVMM_ROWCOUNT=$n iter=$i max=$MAX_ROWS ts=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

  sleep "$INTERVAL_SECONDS"
done
