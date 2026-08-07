# Design — driver-controlled tests

**Status:** implemented on `feat/driver-control-socket`.

A test is a run with a driver. `tdvmm run` boots a stack. If a container connects
to the in-guest control socket and calls the `finish` op, that call's exit code
becomes the run's verdict. A run where no container does this behaves exactly like
a plain `run`.

The driver is one of the stack's own containers. It drives the workload and the
harness in the same program: it can partition, kill, or heal peers while its own
requests to the cluster are in flight, then end the run with a pass/fail verdict.

There is no `tdvmm test` verb, no scenario file, and no host-side timeline. A
container talks to the socket through the Go SDK (`sdk/go/`, package `tdvmm`); the
API is documented in `sdk/go/README.md`.

## What was built

| Piece | Where |
|---|---|
| The socket `/run/tdvmm/ctl/sock`, served by the agent under its blocked poll | `tdvmm-agent/src/bridge.rs` (`run_loop`, `handle_ctl_line`) |
| The wire: `Request`/`Reply` line JSON, `SCHEMA` 4 | `tdvmm-proto/src/lib.rs` |
| The terminal op `finish(exit, message)`, first-wins | `tdvmm-agent/src/agent.rs` (`do_finish`) |
| The bind into every service | `src/compose/emit.rs` (`rewrite_volumes`) |
| Host verdict + exit-code mapping | `src/driver.rs`, `src/exit.rs` |
| The SDK | `sdk/go/` + `sdk/go/README.md` |
| Worked example (partition mid-write) | `testdata/stacks/pgcluster/` |
| Verdict-contract gate | `scripts/driver_test.sh`, `testdata/stacks/driverlab/` |

## Exit-code mapping

`tdvmm run` owns the 0/1/2/3 contract. A driver's raw exit code is not returned
verbatim; a nonzero verdict collapses to 1 so it cannot be confused with 2 (the
tool broke) or 3 (the horizon fired). The raw code is kept in the run summary and
`--metrics-out` (`driver_exit_raw`).

| outcome | `tdvmm run` exits |
|---|---|
| `finish(0)` | 0 — PASS |
| `finish(n)`, n ≠ 0 | 1 — FAIL |
| no `finish`, guest stopped on its own | 0 |
| no `finish`, `--max-virtual-time` fired | 3 |
| no `finish`, `--wall-timeout` fired | 2 |
| bad artifact / agent never came up | 2 |

`scripts/driver_test.sh` checks every row. The FAIL case uses `finish(3)` to
confirm a driver cannot impersonate a VMM outcome.

## The control socket

`/run/tdvmm/ctl/sock` is a unix-domain socket the agent serves in-guest. Its
directory is bind-mounted read-write into every container, like the events FIFO,
because the guest is a closed test world. Any container can drive the harness.

The socket speaks the same `tdvmm-proto` line JSON as the host channel. A socket
request is dispatched through the same `Agent::handle` as a host request, so a
container injects the same faults the host can, with no fault code duplicated. The
agent mirrors each command to the host as a `ctl` event, which the host stamps
with the virtual time it arrived — the run's fault trace. The terminal `finish` op
becomes a `finish` event that the host ends the run on.

Lifecycle: init creates `/run/tdvmm/ctl/` before the agent starts; the agent binds
the socket before writing its hello, so agent-ready implies socket-ready. The bind
is the directory, not the socket file, so the inode the agent creates by `bind()`
is visible through the mount.

Connection discipline: one request line in, one reply line out, correlated by
`id`. Serving is sequential. A socket EOF is not a signal; a driver can issue many
commands, or disconnect, without ending the run.

## The command surface

The socket reuses the existing `Request` op vocabulary. `finish` is the one added
op.

| Op | Notes |
|---|---|
| `kill` / `stop` / `start` | Container lifecycle; each waits for the container to reach its new state. |
| `partition` / `heal` | Drop or restore all traffic between two services; `heal` with no services clears all. |
| `containers` | The container census (name, service, state, exit_code, health). |
| `exec` | Run a command in another container. |
| `logs` | Read a container's log, cursor-paged. |
| `ping` | Round-trip; returns agent identity, schema, build. |
| `finish` | End the run with a verdict. First call wins; a second is refused. |

## Ending the run

`finish(exit, message)` records the verdict and returns. It does not stop anything
in the guest; the run ends when the host sees the `finish` event. The first
`finish` decides the run; a later one is rejected. The host maps the verdict onto
the 0/1/2/3 contract above.

## Virtual time

The control loop is guest-internal: the driver, the socket, the agent, podman, and
nft all run in the guest. Nothing holds host-side real-time state, so fast-forward
needs no gate for the control socket (unlike host-mediated egress). A fast-forward
jump fires only in the HLT park, when no guest process is runnable, so "a control
command is in flight" and "the guest is jumping" never overlap.

`sleep` is the virtual-time API. A driver that sleeps 24 hours idles the guest; the
VMM jumps the clock to the next timer, so the sleep costs microseconds of wall
time. Wait for an observed effect rather than a fixed duration where possible.

## Faults apply before the call returns

Every fault call is synchronous. The agent installs the nftables ruleset (waiting
for `nft` to exit) or waits for the container to stop (`podman wait`) before it
builds the reply the SDK is blocked on. So:

```go
h.Partition("a", "b") // returns only once the rule is installed
fireRequest()         // this request meets a partitioned network
```

Ordering is program order on a single-threaded driver. There is no "did it land
yet?" window. This guarantees ordering, not simultaneity: real milliseconds pass
between the two calls, during which the network is partitioned but idle.

## Coupling a fault to a workload event

To make a fault land in the middle of an operation, open the operation and hold it
across the fault. `testdata/stacks/pgcluster/` opens a transaction, INSERTs without
committing, and partitions while that write is in flight, so the operation spans
the fault by construction and no timing precision is needed. Prefer this
state-based in-flight window over timing.

Packet-level coupling ("partition after the Nth packet to the leader") is not
built. The cheapest credible form would be an nft `counter` rule plus an agent op
that blocks until the count passes a threshold, then applies a queued fault in the
same `nft -f` transaction. It gives packet-count granularity, not semantic
granularity, and puts a blocking op in the agent, which today blocks only on its
poll.

The VMM cannot help here. Faults are applied by the guest (the agent shells out to
`nft`), so pausing the vCPU pauses the mechanism that applies them. Fast-forward
moves the clock only when the guest is idle, and a live in-flight request means the
guest is busy, so there is no clock lever in that regime.

## Security

A non-driver container reaches exactly what it reached before: the events FIFO,
which can fail a run but cannot control it. The control socket grants full harness
control, so its boundary is the closed guest world, not the socket itself.

Two event kinds are agent-originated only: `finish` and `ctl`
(`tdvmm_proto::RESERVED_EVENT_KINDS`). A line claiming one of these that arrives
over the shared events FIFO is rewritten to `invalid`, so no container can forge a
verdict or a fault-trace entry.

## Determinism

Driver runs are reproducible in program order — command order, mirror events, and
the verdict are total and repeatable on a single vCPU with one agent loop — but not
in timestamps. A busy period consumes virtual time at real rate, so two runs of the
same driver produce the same sequence at different times. Driver runs are therefore
excluded from byte-identity gates. Write drivers that assert on observed effects,
not on wall-adjacent timing.
