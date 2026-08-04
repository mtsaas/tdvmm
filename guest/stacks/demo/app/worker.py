#!/usr/bin/env python3
"""dvmm demo stack — the rollup worker (a real Postgres client).

Once per virtual hour it does an incremental roll-up: it reads the orders that
have not yet been summarized and writes one `summaries` row for the hour. It runs
a bit after the client's submit each hour, so it always has that hour's orders to
aggregate.

The "not-yet-summarized" watermark is derived entirely from Postgres state — the
number of orders already summarized is SUM(summaries.order_count) — so it stays
correct across a Postgres restart (both tables live in the same database and come
back together). No fragile in-memory cursor.

Resilience: if Postgres is unreachable it logs `pg unreachable, retrying`, retries
briefly, skips the hour, and carries on — it never exits. Between hours it sleeps
a full virtual hour, which dvmm fast-forwards.
"""
import os
import sys
import time
from datetime import datetime, timezone

import psycopg2

PGHOST = os.environ.get("PGHOST", "postgres")
PGUSER = os.environ.get("PGUSER", "postgres")
PGDATABASE = os.environ.get("PGDATABASE", "appdb")
INTERVAL = int(os.environ.get("INTERVAL_SECONDS", "3600"))
LAG = int(os.environ.get("ROLLUP_LAG_SECONDS", "300"))  # run after the client
RETRIES = int(os.environ.get("PG_RETRIES", "3"))

_pg = None


def log(msg):
    ts = datetime.now(timezone.utc).isoformat(timespec="seconds")
    print(f"{ts} {msg}", flush=True)


def pg_conn():
    global _pg
    for attempt in range(1, RETRIES + 1):
        try:
            if _pg is None or _pg.closed:
                _pg = psycopg2.connect(
                    host=PGHOST, user=PGUSER, dbname=PGDATABASE, connect_timeout=5
                )
                _pg.autocommit = False
            with _pg.cursor() as cur:
                cur.execute("SELECT 1")
            return _pg
        except psycopg2.Error:
            try:
                if _pg is not None:
                    _pg.close()
            except psycopg2.Error:
                pass
            _pg = None
            if attempt == 1:
                log("pg unreachable, retrying")
            if attempt < RETRIES:
                time.sleep(2)
    raise psycopg2.OperationalError(f"postgres {PGHOST} unreachable after {RETRIES} tries")


def rollup(hour):
    conn = pg_conn()
    with conn.cursor() as cur:
        # orders already reflected in a summary (the watermark, from DB state).
        cur.execute("SELECT COALESCE(SUM(order_count), 0) FROM summaries")
        already = cur.fetchone()[0]
        # the tail of orders past the watermark = this hour's un-summarized orders.
        cur.execute(
            "SELECT count(*), COALESCE(sum(amount), 0) FROM "
            "(SELECT amount FROM orders ORDER BY id OFFSET %s) t",
            (already,),
        )
        n, total = cur.fetchone()
        cur.execute(
            "INSERT INTO summaries (hour, order_count, total_amount) "
            "VALUES (%s, %s, %s) RETURNING id",
            (hour, n, total),
        )
        summary_id = cur.fetchone()[0]
    conn.commit()
    log(f"rollup h{hour}: {n} orders -> summary #{summary_id}")


def main():
    time.sleep(LAG)  # let the client's first submit land before the first rollup
    hour = 0
    while True:
        hour += 1
        try:
            rollup(hour)
        except psycopg2.Error:
            log(f"rollup h{hour} skipped (pg unreachable)")
        time.sleep(INTERVAL)  # genuine virtual-hour sleep -> fast-forwarded


if __name__ == "__main__":
    sys.exit(main())
