#!/usr/bin/env python3
"""tdvmm pgcluster driver — the test, running inside the cluster it tests.

This container is an ordinary member of the compose stack. It talks to Postgres
like any client would, and it talks to the tdvmm harness over the control socket
to inject faults into its own cluster. Both in one program, which is the whole
point: a fault can land in the middle of an operation this program has in flight.

The experiment:

  1. bring the cluster up and confirm the standby is really streaming;
  2. turn ON synchronous replication, so a COMMIT is not durable — or even
     complete — until the standby has confirmed the WAL;
  3. open a transaction and INSERT a row, but do NOT commit. The write is now
     genuinely IN FLIGHT: it exists on the primary and nowhere else;
  4. PARTITION the primary from the standby while that transaction is open;
  5. COMMIT. It must BLOCK — there is no synchronous standby to confirm to;
  6. HEAL. The same commit must now complete, and the row must appear on BOTH
     nodes.

Step 5 is the assertion that matters: it distinguishes a cluster that honors its
durability contract from one that silently commits anyway. A `finish(1)` at any
point fails the run.

Note what is NOT here: any sleeping-until-a-timestamp. Every wait is for an
OBSERVED state (the standby is streaming; the commit has or has not returned),
which is what makes the test reproducible even though the driver runs in real
time inside a fast-forwarded guest.
"""

import sys
import threading
import time

import psycopg2

sys.path.insert(0, "/app")
import tdvmm  # noqa: E402  (bind-mounted beside this file)

PRIMARY = "pg-primary"
STANDBY = "pg-standby"

#: How long to insist the partitioned commit stays blocked. Virtual seconds: the
#: guest is idle while we wait, so fast-forward makes this cost ~no real time.
BLOCKED_PROOF_S = 30

#: How long to allow the healed commit to finish. Also virtual seconds.
HEAL_COMPLETE_S = 60


def connect(host, timeout=10):
    return psycopg2.connect(
        host=host, user="postgres", dbname="appdb", connect_timeout=timeout
    )


def query1(conn, sql):
    """One scalar, on its own transaction."""
    with conn.cursor() as cur:
        cur.execute(sql)
        row = cur.fetchone()
        conn.rollback()
        return row[0] if row else None


def main() -> int:
    h = tdvmm.connect()
    log = lambda m: print(f"[driver] {m}", flush=True)  # noqa: E731
    log(f"harness ready: {h.ping().get('agent')} schema {h.ping().get('schema')}")

    # -- 1. the cluster is up -------------------------------------------------
    h.wait_for_services([PRIMARY, STANDBY], timeout_s=300)
    log("both nodes have running containers")

    primary = None
    def primary_up():
        nonlocal primary
        primary = connect(PRIMARY)
        primary.autocommit = True
        return True

    h.wait_until(primary_up, timeout_s=300, what="the primary to accept connections")
    log("primary accepting connections")

    with primary.cursor() as cur:
        cur.execute(
            "CREATE TABLE IF NOT EXISTS orders "
            "(id serial PRIMARY KEY, item text NOT NULL, ts timestamptz DEFAULT now())"
        )
    log("schema ready")

    # The standby must be a live REPLICA, not merely a running container. Two
    # traps here, both hit for real while writing this test:
    #   * `pg_stat_replication` also lists pg_basebackup's own connection while
    #     the clone is still running, so counting rows says "streaming" before a
    #     replica exists. The walreceiver identifies itself as `walreceiver`;
    #     the clone identifies itself as `pg_basebackup`.
    #   * a container that starts and immediately dies still shows as running in
    #     one census and is gone by the next command, so ask the standby itself.
    h.wait_until(
        lambda: query1(
            primary,
            "SELECT count(*) FROM pg_stat_replication "
            "WHERE state = 'streaming' AND application_name = 'walreceiver'",
        ) == 1,
        timeout_s=300,
        every_s=2,
        what="the standby's walreceiver to start streaming",
    )
    standby = connect(STANDBY)
    standby.autocommit = True
    if query1(standby, "SELECT pg_is_in_recovery()") is not True:
        h.finish(1, "the standby is not in recovery — it is not a replica of the primary")
        return 1
    log("standby is streaming and in recovery")

    # -- 2. synchronous replication ON ---------------------------------------
    # From here a COMMIT on the primary is not complete until the standby has
    # confirmed it. That is the contract the partition will test.
    with primary.cursor() as cur:
        cur.execute("ALTER SYSTEM SET synchronous_standby_names = '*'")
        cur.execute("SELECT pg_reload_conf()")
    h.wait_until(
        lambda: query1(primary, "SELECT sync_state FROM pg_stat_replication "
                                "LIMIT 1") == "sync",
        timeout_s=60,
        what="the standby to become the synchronous replica",
    )
    log("synchronous replication is ON (commits now require the standby)")

    baseline = query1(primary, "SELECT count(*) FROM orders")

    # -- 3. a write, in flight ------------------------------------------------
    # A second connection opens a transaction and INSERTs, then waits. The row
    # exists on the primary and nowhere else; nothing has been committed.
    writer = connect(PRIMARY)
    writer.autocommit = False
    with writer.cursor() as cur:
        cur.execute("INSERT INTO orders (item) VALUES ('widget-during-partition')")
    log("transaction open on the primary with an uncommitted INSERT")

    commit_returned = threading.Event()
    commit_error = []

    def do_commit():
        try:
            writer.commit()
        except Exception as e:  # noqa: BLE001 — reported, then asserted on
            commit_error.append(e)
        finally:
            commit_returned.set()

    # -- 4. cut the cluster in half, mid-transaction --------------------------
    # partition() returns only once the rule is actually installed, so the commit
    # fired next is guaranteed to meet a partitioned network. No race, no guess.
    h.partition(PRIMARY, STANDBY)
    log(f"PARTITIONED {PRIMARY} <-x-> {STANDBY} with the write still in flight")

    # -- 5. the commit must not complete --------------------------------------
    threading.Thread(target=do_commit, daemon=True).start()
    if commit_returned.wait(BLOCKED_PROOF_S):
        why = commit_error[0] if commit_error else "it succeeded"
        h.heal()
        h.finish(
            1,
            f"the commit completed while the synchronous standby was unreachable ({why})",
        )
        return 1
    log(f"commit is still blocked after {BLOCKED_PROOF_S}s of virtual time — correct")

    # The primary must still be serving reads while its replica is unreachable.
    if query1(primary, "SELECT count(*) FROM orders") != baseline:
        h.heal()
        h.finish(1, "the uncommitted row was visible to another session")
        return 1
    log("the in-flight row is correctly invisible to other sessions")

    # -- 6. heal, and the same commit completes -------------------------------
    h.heal()
    log("HEALED; the commit should now be able to finish")

    if not commit_returned.wait(HEAL_COMPLETE_S):
        h.finish(1, f"the commit never completed within {HEAL_COMPLETE_S}s of healing")
        return 1
    if commit_error:
        h.finish(1, f"the commit failed after healing: {commit_error[0]}")
        return 1
    log("the commit completed after healing")

    # The row must be on BOTH nodes: that is what synchronous replication bought.
    if query1(primary, "SELECT count(*) FROM orders") != baseline + 1:
        h.finish(1, "the committed row is missing from the primary")
        return 1

    try:
        h.wait_until(
            lambda: query1(standby, "SELECT count(*) FROM orders") == baseline + 1,
            timeout_s=60,
            what="the healed standby to carry the committed row",
        )
    except tdvmm.TdvmmError as e:
        h.finish(1, f"the standby never received the committed row: {e}")
        return 1
    log("the row is present on BOTH nodes")

    h.finish(0, "synchronous commit blocked under partition and completed after heal")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as e:  # noqa: BLE001
        # An unhandled error in the driver is a failed test, not a hung run: say
        # so over the socket rather than leaving the harness to time out.
        print(f"[driver] UNHANDLED: {e!r}", flush=True)
        try:
            tdvmm.connect(retry_s=5).finish(1, f"driver crashed: {e!r}")
        except Exception:  # noqa: BLE001
            pass
        raise SystemExit(1)
