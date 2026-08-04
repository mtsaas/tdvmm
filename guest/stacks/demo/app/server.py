#!/usr/bin/env python3
"""dvmm demo stack — the api service (a real gRPC server).

A genuine, interpreted microservice: it serves the OrderService gRPC contract
(orders.proto) over the compose network and talks to two real backends by name —
Postgres (psycopg2) and Redis (redis-py) — with their real wire protocols.

  SubmitOrders(hour, count) -> INSERT `count` orders into Postgres, then
                               INCRBY the Redis "orders:total" counter by count.
  GetStats()                -> SELECT count(*)/sum(amount) FROM orders + the
                               cached Redis total.

Resilience: if Postgres is unreachable (e.g. the demo scenario SIGKILLs it mid
run) the handler logs `pg unreachable, retrying`, retries briefly, and returns a
gRPC error to the caller — it never exits. When Postgres comes back the next call
reconnects transparently. Redis is incremented only AFTER the Postgres write
commits, so the two stores stay consistent across a fault.

Fast-forward friendly: an idle gRPC server parks on its sockets, so the guest
HLTs between the client's once-per-virtual-hour calls and dvmm collapses the gap.
"""
import os
import sys
import time
from concurrent import futures
from datetime import datetime, timezone

import grpc
import psycopg2
import psycopg2.extras
import redis

import orders_pb2
import orders_pb2_grpc

PGHOST = os.environ.get("PGHOST", "postgres")
PGUSER = os.environ.get("PGUSER", "postgres")
PGDATABASE = os.environ.get("PGDATABASE", "appdb")
REDIS_HOST = os.environ.get("REDIS_HOST", "redis")
LISTEN = os.environ.get("API_LISTEN", "0.0.0.0:50051")
RETRIES = int(os.environ.get("PG_RETRIES", "3"))


def log(msg):
    """One RFC3339-stamped line to stdout (podman captures it per-service)."""
    ts = datetime.now(timezone.utc).isoformat(timespec="seconds")
    print(f"{ts} {msg}", flush=True)


_pg = None


def pg_conn():
    """Return a live Postgres connection, reconnecting if the last one died.

    Retries a bounded number of times with a genuine sleep between attempts (so
    the guest HLTs and fast-forward collapses the wait). Raises if Postgres stays
    unreachable — the caller turns that into a logged retry + gRPC error.
    """
    global _pg
    for attempt in range(1, RETRIES + 1):
        try:
            if _pg is None or _pg.closed:
                _pg = psycopg2.connect(
                    host=PGHOST, user=PGUSER, dbname=PGDATABASE, connect_timeout=5
                )
                _pg.autocommit = False
            # cheap liveness check; forces a reconnect if the socket is dead.
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


def redis_client():
    return redis.Redis(host=REDIS_HOST, socket_connect_timeout=5, socket_timeout=5)


class OrderService(orders_pb2_grpc.OrderServiceServicer):
    def __init__(self):
        self.rds = redis_client()

    def SubmitOrders(self, request, context):
        hour, count = request.hour, request.count
        # deterministic per-order amounts (no RNG -> the same inputs replay same).
        rows = [(100 + ((hour * 7 + i) % 50),) for i in range(count)]
        try:
            conn = pg_conn()
            with conn.cursor() as cur:
                psycopg2.extras.execute_values(
                    cur, "INSERT INTO orders (amount) VALUES %s", rows
                )
            conn.commit()
        except psycopg2.Error:
            log("pg unreachable, retrying")
            context.set_code(grpc.StatusCode.UNAVAILABLE)
            context.set_details("postgres unreachable")
            return orders_pb2.SubmitAck()
        # Redis is bumped only after the Postgres write committed -> the counter
        # never counts orders that failed to persist (stores stay consistent).
        try:
            total = self.rds.incrby("orders:total", count)
        except redis.RedisError:
            log("redis unreachable, retrying")
            total = -1
        log(f"hour {hour}: received {count} orders (redis total={total})")
        return orders_pb2.SubmitAck(received=count, redis_total=total)

    def GetStats(self, request, context):
        try:
            conn = pg_conn()
            with conn.cursor() as cur:
                cur.execute("SELECT count(*), COALESCE(sum(amount), 0) FROM orders")
                order_count, total_amount = cur.fetchone()
            conn.commit()
        except psycopg2.Error:
            log("pg unreachable, retrying")
            context.set_code(grpc.StatusCode.UNAVAILABLE)
            context.set_details("postgres unreachable")
            return orders_pb2.StatsReply()
        try:
            cached = self.rds.get("orders:total")
            cache_total = int(cached) if cached is not None else 0
        except redis.RedisError:
            log("redis unreachable, retrying")
            cache_total = -1
        log(f"GET /stats  pg+redis OK ({order_count} orders, cache={cache_total})")
        return orders_pb2.StatsReply(
            order_count=order_count, cache_total=cache_total, total_amount=total_amount
        )


def main():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    orders_pb2_grpc.add_OrderServiceServicer_to_server(OrderService(), server)
    server.add_insecure_port(LISTEN)
    server.start()
    log(f"api gRPC OrderService listening on {LISTEN} (pg={PGHOST} redis={REDIS_HOST})")
    server.wait_for_termination()


if __name__ == "__main__":
    sys.exit(main())
