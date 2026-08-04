#!/bin/sh
# tdvmm corpus (svcchain) backend service (the middle tier).
#
# Bind-mounted READ-ONLY into the pinned Postgres image and run as that
# container's entrypoint. It starts only after `db` is healthy (compose gate),
# connects to the db BY NAME, ensures the schema, then touches /tmp/ready so its
# OWN healthcheck (test -f /tmp/ready) flips it to healthy -- which in turn
# unblocks the `frontend` (the second hop of the gated chain). It then runs the
# insert/trim workload so the corpus runner can assert cadence under fast-forward.
set -u

PGHOST="${PGHOST:-db}"
PGUSER="${PGUSER:-postgres}"
PGDATABASE="${PGDATABASE:-appdb}"
export PGHOST PGUSER PGDATABASE
export PGCONNECT_TIMEOUT=5
INTERVAL_SECONDS="${INTERVAL_SECONDS:-3600}"
MAX_ROWS="${MAX_ROWS:-1000}"

echo "TDVMM_BACKEND_START host=$PGHOST db=$PGDATABASE interval=${INTERVAL_SECONDS}s max_rows=$MAX_ROWS"
until pg_isready -q -h "$PGHOST" -U "$PGUSER"; do
  echo "TDVMM_BACKEND_WAIT db not ready yet"; sleep 1
done
if ! psql -v ON_ERROR_STOP=1 -q -c \
  "CREATE TABLE IF NOT EXISTS events (id bigserial PRIMARY KEY, ts timestamptz NOT NULL DEFAULT now(), value text);"
then
  echo "TDVMM_SVC_FAIL create-table"; exit 1
fi

# Signal readiness: the backend's healthcheck tests for this file, so this is the
# moment the middle tier becomes healthy and the frontend gate can resolve.
touch /tmp/ready
echo "TDVMM_BACKEND_READY connected to db + schema ensured"

i=0
while :; do
  i=$((i + 1))
  if ! psql -v ON_ERROR_STOP=1 -q -c "INSERT INTO events(value) VALUES ('tier-$i');"; then
    echo "TDVMM_SVC_FAIL insert iter=$i"; exit 1
  fi
  if ! psql -v ON_ERROR_STOP=1 -q -c \
    "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT $MAX_ROWS);"
  then
    echo "TDVMM_SVC_FAIL trim iter=$i"; exit 1
  fi
  n="$(psql -tAX -v ON_ERROR_STOP=1 -c 'SELECT count(*) FROM events;')"
  [ -z "$n" ] && { echo "TDVMM_SVC_FAIL count iter=$i"; exit 1; }
  echo "TDVMM_ROWCOUNT=$n iter=$i max=$MAX_ROWS ts=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  sleep "$INTERVAL_SECONDS"
done
