#!/usr/bin/env python3
"""Cross-store consistency check, run by the demo scenario via `podman exec` in the
api container. Exits 0 iff Postgres' order count equals Redis' orders:total — the
invariant the api maintains (it bumps Redis only after the Postgres write commits),
which must still hold after the fault+recovery.
"""
import sys

import psycopg2
import redis

c = psycopg2.connect(host="postgres", user="postgres", dbname="appdb", connect_timeout=5)
cur = c.cursor()
cur.execute("SELECT count(*) FROM orders")
n = cur.fetchone()[0]
t = int(redis.Redis(host="redis", socket_connect_timeout=5).get("orders:total") or -1)
print(f"orders={n} redis_total={t} consistent={n == t}")
sys.exit(0 if n == t else 1)
