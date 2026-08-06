#!/usr/bin/env python3
"""tdvmm demo stack — the load client (a real gRPC client).

Drives the api's OrderService over the compose network, by name, once per virtual
hour: it submits a deterministic batch of orders and then reads the stats back.
Between hours it genuinely sleeps for a virtual hour, so the guest HLTs and tdvmm
fast-forwards the idle gap — a whole virtual day of traffic runs in seconds.

A fresh channel is opened per cycle (and closed after), so no client-side gRPC
poller lingers during the hour-long idle windows. If the api (or Postgres behind
it) is down, the call fails; the client logs `api call failed, retrying`, retries
briefly, and carries on to the next hour — it never exits.
"""
import os
import sys
import time
from datetime import datetime, timezone

import grpc

import orders_pb2
import orders_pb2_grpc

API_ADDR = os.environ.get("API_ADDR", "api:50051")
INTERVAL = int(os.environ.get("INTERVAL_SECONDS", "3600"))
CALL_RETRIES = int(os.environ.get("CALL_RETRIES", "3"))


def log(msg):
    ts = datetime.now(timezone.utc).isoformat(timespec="seconds")
    print(f"{ts} {msg}", flush=True)


def wait_for_api():
    """Block until the api gRPC server accepts a connection (bounded, retrying)."""
    for _ in range(60):
        try:
            with grpc.insecure_channel(API_ADDR) as ch:
                grpc.channel_ready_future(ch).result(timeout=5)
            log(f"api reachable at {API_ADDR}")
            return
        except grpc.FutureTimeoutError:
            time.sleep(2)
    log(f"api not reachable at {API_ADDR} after retries; starting anyway")


def one_cycle(hour):
    """Submit this hour's batch + read stats back. Returns True on success."""
    count = 8 + (hour % 7)  # deterministic batch size
    for attempt in range(1, CALL_RETRIES + 1):
        try:
            with grpc.insecure_channel(API_ADDR) as ch:
                stub = orders_pb2_grpc.OrderServiceStub(ch)
                ack = stub.SubmitOrders(
                    orders_pb2.OrderBatch(hour=hour, count=count), timeout=10
                )
                stats = stub.GetStats(orders_pb2.StatsRequest(), timeout=10)
            log(
                f"hour {hour}: submitted {count} orders via gRPC -> "
                f"{stats.order_count} total orders (cache={stats.cache_total})"
            )
            return True
        except grpc.RpcError as e:
            if attempt == 1:
                log(f"hour {hour}: api call failed ({e.code().name}), retrying")
            time.sleep(3)
    log(f"hour {hour}: api still unavailable, skipping this hour")
    return False


def main():
    wait_for_api()
    hour = 0
    while True:
        hour += 1
        one_cycle(hour)
        time.sleep(INTERVAL)  # genuine virtual-hour sleep -> fast-forwarded


if __name__ == "__main__":
    sys.exit(main())
