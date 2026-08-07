#!/usr/bin/env python3
"""tdvmm driverlab driver — exercises what a driver can do to a run's outcome.

One baked artifact, several behaviors, chosen per run with
`--cmdline ... tdvmm.drivermode=<mode>`. See ../compose.yml.
"""

import os
import sys
import time

sys.path.insert(0, "/app")
import tdvmm  # noqa: E402


def log(msg):
    print(f"[driver] {msg}", flush=True)


def main() -> None:
    mode = os.environ.get("DRIVER_MODE", "pass").strip()
    log(f"mode={mode!r}")
    h = tdvmm.connect()
    log(f"connected: agent schema {h.ping().get('schema')}")

    if mode == "pass":
        h.finish(0, "driverlab pass")

    elif mode == "fail":
        h.finish(3, "driverlab deliberate failure")

    elif mode == "faults":
        # A container fault, through the same agent op the host used to use.
        h.wait_for_services(["peer"], timeout_s=120)
        h.kill("peer")
        # kill() returns only once the container is really dead, so this census
        # needs no retry loop — that is the apply-and-ack property.
        if "peer" in h.running():
            h.finish(1, "peer still running immediately after kill()")
            return
        log("peer is down")
        h.start("peer")
        h.wait_until(lambda: "peer" in h.running(), timeout_s=120, what="peer to restart")
        log("peer is back")
        # A fault against a service that does not exist must fail cleanly, not
        # wedge the agent or the run.
        try:
            h.partition("peer", "nope")
            h.finish(1, "partitioning an unknown service unexpectedly succeeded")
            return
        except tdvmm.CommandError as e:
            log(f"unknown service rejected as expected: {e.code}")
        h.finish(0, "driverlab faults ok")

    elif mode == "hang":
        # Never finish: the run must be ended by the wall-clock safety timeout.
        log("hanging deliberately; the wall-clock timeout should end this run")
        while True:
            time.sleep(3600)

    else:
        h.finish(1, f"unknown driver mode {mode!r}")


if __name__ == "__main__":
    main()
