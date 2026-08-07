"""tdvmm — drive the test harness from inside your own container.

A tdvmm guest runs your compose stack in a single-vCPU VM whose idle time is
fast-forwarded. This module is how a container inside that stack talks back to
the harness: it injects faults into its own cluster (partition, kill, stop,
start, heal), and it ends the run with a verdict.

The point is that the workload and the faults are ONE program. You can cut the
network *while a request is in flight* and observe what the cluster does with a
half-delivered operation — the Jepsen shape — instead of scripting faults from
outside against wall-clock guesses:

    import tdvmm

    h = tdvmm.connect()
    fut = pool.submit(db.write, "orders", row)   # in flight to the cluster
    h.partition("pg-primary", "pg-standby")      # cut it mid-write
    assert "no quorum" in str(fut.exception())
    h.heal()
    h.finish(0)

## What you are talking to

`/run/tdvmm/ctl/sock`, a unix socket the in-guest `tdvmm-agent` serves. It is
bind-mounted into every container in the stack, so any of them can drive. The
wire protocol is one JSON object per line (`tdvmm-proto`) — the SAME protocol
and the SAME handler the VMM itself uses, so a container has exactly the fault
vocabulary the harness has, no more.

## Faults are applied before the call returns

Every fault call is synchronous: it returns only after the agent has actually
installed the rule (nftables) or the container has actually reached its new
state (`podman kill` + `podman wait`). So

    h.partition("a", "b")
    fire_request()          # the network is ALREADY cut here

is deterministic — there is no "did it land yet?" window to guess at. Order
your fault and your workload however you need; the ordering is program order.

## Virtual time

`time.sleep(3600)` costs microseconds of wall time: the guest goes idle, the VMM
fast-forwards the clock, and your sleep returns having "taken" an hour. That is
how you write "wait a day, then partition" without waiting a day. Sleep is the
virtual-time API — there is nothing else to call.

## Ending the run

`finish(code)` is what makes a run a test. It ends the run and its code becomes
the verdict: 0 passes, anything else fails. The FIRST finish wins. If your
driver crashes without calling it, the run is bounded by `--wall-timeout` /
`--max-virtual-time` instead and does not pass.
"""

from __future__ import annotations

import json
import os
import socket
import time
from typing import Any, Iterable, Sequence

__all__ = [
    "Harness",
    "TdvmmError",
    "CommandError",
    "connect",
    "SOCKET_PATH",
]

#: The control socket, bind-mounted into every container in the stack. Matches
#: `tdvmm_proto::CONTROL_SOCKET_PATH`; override with `TDVMM_CONTROL_SOCKET`.
SOCKET_PATH = os.environ.get("TDVMM_CONTROL_SOCKET", "/run/tdvmm/ctl/sock")

#: How long to wait for the agent's reply to one command. Generous because a
#: fault call blocks until the fault is really applied (a `podman kill` waits for
#: the container to actually stop), and guest seconds are cheap.
DEFAULT_TIMEOUT_S = 120.0


class TdvmmError(Exception):
    """Base class for every error this module raises."""


class CommandError(TdvmmError):
    """The agent refused or could not perform a command.

    `code` is the stable machine-matchable prefix of the agent's error string
    (``no_container``, ``nft``, ``podman_op``, ``unknown_op``, ...), so a test can
    branch on the failure kind without string-matching the whole message.
    """

    def __init__(self, op: str, error: str) -> None:
        super().__init__(f"{op}: {error}")
        self.op = op
        self.error = error
        self.code = error.split(":", 1)[0] if ":" in error else ""


class Harness:
    """A live connection to the in-guest harness.

    Use :func:`connect` rather than constructing this directly. Safe to use as a
    context manager; the connection is cheap and holding it open costs nothing
    (an idle socket arms no timer, so it never blocks fast-forward).
    """

    def __init__(self, sock: socket.socket, timeout: float = DEFAULT_TIMEOUT_S) -> None:
        self._sock = sock
        self._file = sock.makefile("rwb")
        self._next_id = 1
        self._timeout = timeout
        self._finished = False

    # -- lifecycle ---------------------------------------------------------

    def close(self) -> None:
        """Close the connection. Does NOT end the run — see :meth:`finish`."""
        try:
            self._file.close()
        finally:
            self._sock.close()

    def __enter__(self) -> "Harness":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    # -- the wire ----------------------------------------------------------

    def request(self, op: str, **fields: Any) -> dict:
        """Send one raw command and return the agent's reply.

        The escape hatch: every helper below is a thin wrapper over this, so a
        new agent op is usable before this module knows about it.

        Raises :class:`CommandError` if the agent reports failure.
        """
        req = {"id": self._next_id, "op": op}
        self._next_id += 1
        req.update({k: v for k, v in fields.items() if v is not None})

        line = json.dumps(req, separators=(",", ":")).encode() + b"\n"
        try:
            self._sock.settimeout(self._timeout)
            self._file.write(line)
            self._file.flush()
            raw = self._file.readline()
        except OSError as e:
            raise TdvmmError(f"control socket failed during {op!r}: {e}") from e
        if not raw:
            raise TdvmmError(
                f"control socket closed while waiting for the reply to {op!r} "
                "(did the run already end?)"
            )
        try:
            reply = json.loads(raw)
        except ValueError as e:
            raise TdvmmError(f"malformed reply to {op!r}: {raw!r}") from e
        if not reply.get("ok"):
            raise CommandError(op, reply.get("error") or "the agent reported no reason")
        return reply

    # -- network faults ----------------------------------------------------

    def partition(self, a: str, b: str) -> None:
        """Drop ALL traffic between two services, both directions.

        Returns once the rule is installed, so a request fired after this call
        is guaranteed to meet the partition. Services are compose service names.
        """
        self.request("partition", container=a, peer=b)

    def heal(self, a: str | None = None, b: str | None = None) -> None:
        """Undo one partition, or ALL of them when called with no arguments."""
        if (a is None) != (b is None):
            raise TdvmmError("heal takes two services or none (heal-all)")
        self.request("heal", container=a, peer=b)

    # -- container faults --------------------------------------------------

    def kill(self, service: str) -> None:
        """SIGKILL a service's container and wait for it to actually be dead."""
        self.request("kill", container=service)

    def stop(self, service: str) -> None:
        """Gracefully stop a service's container (SIGTERM, then SIGKILL)."""
        self.request("stop", container=service)

    def start(self, service: str) -> None:
        """Start a previously stopped/killed container. Idempotent if running."""
        self.request("start", container=service)

    # -- observation -------------------------------------------------------

    def containers(self) -> list[dict]:
        """The container census: name, service, state, exit_code, health."""
        return self.request("containers").get("containers") or []

    def running(self) -> set[str]:
        """The set of services with a running container."""
        return {c["service"] for c in self.containers() if c.get("state") == "running"}

    def exec(
        self,
        service: str,
        cmd: str | Sequence[str],
        timeout_s: int | None = None,
    ) -> dict:
        """Run a command inside ANOTHER service's container.

        A string runs through ``sh -c``; a list is exec'd directly. Returns the
        reply dict (``exit``, ``stdout``, ``stderr``). A nonzero exit is NOT an
        error here — it is a result; only the agent failing to run it raises.
        """
        argv = ["sh", "-c", cmd] if isinstance(cmd, str) else list(cmd)
        return self.request("exec", container=service, cmd=argv, timeout_s=timeout_s)

    def logs(self, service: str, max_bytes: int | None = None) -> str:
        """Read a service's container log from the start (paged to the end)."""
        out, cursor = [], 0
        while True:
            r = self.request("logs", container=service, cursor=cursor, max_bytes=max_bytes)
            out.append(r.get("data") or "")
            cursor = r.get("next_cursor", cursor)
            if r.get("eof", True):
                return "".join(out)

    def ping(self) -> dict:
        """Round-trip the agent; returns its identity, wire schema, and build."""
        return self.request("ping")

    # -- waiting -----------------------------------------------------------

    def wait_until(
        self,
        predicate,
        timeout_s: float = 120.0,
        every_s: float = 1.0,
        what: str = "condition",
    ) -> None:
        """Poll `predicate` until it is true, in VIRTUAL time.

        The sleeps between attempts are ordinary `time.sleep`, so an idle guest
        fast-forwards through them: a 120-second timeout costs no real time if
        nothing else is running. Raises :class:`TdvmmError` on timeout.
        """
        deadline = time.monotonic() + timeout_s
        while True:
            try:
                if predicate():
                    return
            except Exception:  # noqa: BLE001 — a probe that throws is just "not yet"
                pass
            if time.monotonic() >= deadline:
                raise TdvmmError(f"timed out after {timeout_s}s waiting for {what}")
            time.sleep(every_s)

    def wait_for_services(self, services: Iterable[str], timeout_s: float = 180.0) -> None:
        """Wait until every named service has a running container."""
        want = set(services)
        self.wait_until(
            lambda: want <= self.running(),
            timeout_s=timeout_s,
            what=f"services {sorted(want)} to be running",
        )

    # -- ending the run ----------------------------------------------------

    def finish(self, code: int = 0, message: str | None = None) -> None:
        """End the run with a verdict. 0 passes; anything else fails.

        This is what makes a run a test. It returns normally — the harness tears
        the VM down immediately afterwards, so treat it as the last statement
        your driver executes. The FIRST finish decides the run; a second one is
        refused (and raises :class:`CommandError`).
        """
        self.request("finish", exit=int(code), message=message)
        self._finished = True

    def fail(self, message: str) -> None:
        """End the run as a FAILURE with a reason. Shorthand for ``finish(1, …)``."""
        self.finish(1, message)


def connect(path: str = SOCKET_PATH, timeout: float = DEFAULT_TIMEOUT_S,
            retry_s: float = 30.0) -> Harness:
    """Connect to the harness.

    Retries briefly, because a driver container can start before the agent has
    bound the socket. Raises :class:`TdvmmError` if the socket never appears —
    which usually means the stack was baked by an older tdvmm, or you are not
    running inside a tdvmm guest.
    """
    deadline = time.monotonic() + retry_s
    last: OSError | None = None
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(timeout)
            s.connect(path)
            return Harness(s, timeout)
        except OSError as e:
            last = e
            s.close()
            if time.monotonic() >= deadline:
                raise TdvmmError(
                    f"cannot reach the tdvmm control socket at {path}: {last}. "
                    "Is this container running inside a tdvmm guest?"
                ) from last
            time.sleep(0.5)
