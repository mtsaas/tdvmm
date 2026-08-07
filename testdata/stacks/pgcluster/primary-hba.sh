#!/bin/sh
# Allow the standby to open a replication connection.
#
# The postgres image's POSTGRES_HOST_AUTH_METHOD=trust appends only
# `host all all all trust` to pg_hba.conf. Replication connections are matched by
# the literal database name `replication`, which that line does not cover, so
# pg_basebackup from another container would be rejected. Runs as an initdb
# script: the entrypoint restarts the server afterwards, so the new rule is live
# before anything connects.
set -e
echo "host replication all all trust" >> "$PGDATA/pg_hba.conf"
echo "[primary-hba] replication connections enabled for the standby"
