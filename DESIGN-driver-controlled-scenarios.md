# Design — driver-controlled scenarios: the harness driven from inside the stack

**Status:** DESIGN ONLY — nothing implemented. Fable design, 2026-08-06.

Owner ruling: the scenario system is too brute-ish. A host-side declarative YAML
timeline externally scripts faults at fixed `at:` virtual times, but realistically
one of the stack's own containers is already driving the application workload —
that same container should drive the *test*: trigger partitions, kill/heal peers,
assert, and end the run. Mechanism: expose a local socket the designated driver
container can reach; when that driver container exits, the test ends and its exit
becomes the verdict signal.

## 0. The recommendation in one paragraph

Add a **driver mode** beside (not replacing) the declarative engine. The
designated driver is one of the user's own compose services, marked at bake time
(`x-tdvmm-driver: true`), which alone receives a bind-mount of a control
directory `/run/tdvmm/ctl/` containing a **unix-domain socket served by the
in-guest `tdvmm-agent`** and a static `tdvmm-ctl` client binary. The socket
speaks the existing `tdvmm-proto` line-JSON `Request`/`Reply` verbatim; the
agent executes fault ops locally (it already owns kill/partition/heal) and
mirrors every command + result to the host over ttyS1 as bridged events, so the
host keeps the JSONL log, the assertion ledger, and the verdict. The agent
watches the driver container with a `podman wait` child under its existing
blocked poll; driver exit becomes a `driver_exit` event, and the host folds
(driver exit code, assertion ledger, final census) into the unchanged 0/1/2
`test` contract. Because the entire control loop is guest-internal, it holds
**zero host-side real-time state** — unlike egress, nothing pins real rate, and
the driver expresses virtual time with plain `sleep`, which HLTs the guest and
fast-forwards exactly like every workload today.

## 1. Current architecture — what gets inverted

The pieces, so the inversion is precise about what moves and what stays:

| Piece | Where | Role today |
|---|---|---|
| Scenario YAML | `src/scenario/schema.rs` | Host-side timeline: `at:`-timed `exec`/`expect`/`containers`/`wait_for` + fault actions `kill`/`stop`/`start`/`partition`/`heal`; `run.until: done`; static `expect_death` (`schema.rs:30-96`) |
| ScenarioEngine | `src/scenario/engine.rs` | Host-side conductor: one phase machine, one armed deadline, sends framed `Request`s down COM2 at scheduled vtsc (`engine.rs:371-468`), judges replies via `eval.rs`, folds bridged guest events via `ledger.rs`, decides 0/1/2 |
| Wire protocol | `tdvmm-proto/src/lib.rs` | Line JSON, `SCHEMA=3`: `Request{op,container,peer,cmd,…}`, permissive `Reply` (also carries the hello and id-less bridged `GuestEvent`s), `ErrorKind` taxonomy, framing helpers |
| Control channel | `src/control.rs` + `src/serial.rs` | COM2/ttyS1, a modeled 16550. `send_frame` queues, `pump` feeds the FIFO + raises IRQ3 at loop boundaries (single-writer law, `control.rs:19-29`) |
| In-guest agent | `tdvmm-agent/src/{agent,bridge}.rs` | Blocks in one `poll` on ttyS1 + the events FIFO (`bridge.rs:95-139`); executes ops via podman/nft: `kill`/`stop` = resolve running container → `podman kill/stop` → `podman wait` so the census is deterministic (`agent.rs:221-244`); `partition` = resolve both bridge IPs, insert the pair into a `BTreeMap`, atomically rebuild an nft `inet tdvmm_faults` forward-drop table (`agent.rs:274-326,418-463`); `heal` = remove pair (or all) + rebuild (`agent.rs:330-355`) |
| Events FIFO | `/run/tdvmm/events` | The existing guest→host channel: bind-mounted `rw` into EVERY service (`src/compose/emit.rs:146-166`), created by init before the agent (`overlay/init:125`), PIPE_BUF-atomic writes, agent bridges each line as an id-less `Reply{event,seq}` (`bridge.rs:126-138`); "the SDK is literally one line: echo JSON to the FIFO" (`testdata/stacks/insert-trim/events.yml`) |
| Egress precedent | `src/egress.rs`, `tdvmm-agent/src/forwarder.rs` | The one existing "socket exposed to containers": SOCKS5h on the bridge gateway `:1080` over COM4/ttyS3, opt-in. Because sessions terminate **on the host in real time**, fast-forward is phase-gated while any session is live (INV-E1..E4, `egress.rs:19-62`; the gate park `main.rs:1745-1772`; always-on assert `egress.rs:842`) |
| Fast-forward | `src/main.rs` | Jumps happen ONLY in the HLT park (`fast_forward_until_deliverable`, `main.rs:1698-1846`): guest HLTs → next queue deadline → bump the TSC offset, with the queue-discipline assert (`main.rs:1543-1548`). A busy guest runs at real rate. |
| Verdict contract | `src/exit.rs` | `test`: 0 pass / 1 assertion fail / 2 infra error (`exit.rs:17-21`); horizon mid-scenario = 2 (`scenario/engine.rs:212-237`) |

**What inverts:** scheduling and decision-making about *when to do what* moves
from the host YAML into a program running in the guest. **What must not move:**
the verdict authority, the JSONL evidence trail, closed-world security, and
fast-forward transparency — all four stay host-anchored and are the constraints
that shape every decision below.

## 2. The proposed model

```
driver container ──/run/tdvmm/ctl/sock──▶ tdvmm-agent ──ttyS1──▶ host engine
   (user's own          (line JSON,          executes faults      logs vtsc-stamped
    app/test code;       Request/Reply)      locally (podman/     mirror events,
    sleeps = virtual                         nft), mirrors        accrues ledger +
    time; exits =                            cmd+result as        expect_death,
    verdict signal)                          events; watches      folds verdict on
                                             driver exit          driver_exit
```

A run: host boots the artifact in driver mode → agent hello → host polls the
census (virtual-time scheduled, FF-collapsible) until the driver's container
exists → host sends `watch_driver` → agent arms a `podman wait` child on the
driver → the driver program does its thing (probe peers over the compose
network, `sleep` through the boring parts, issue `tdvmm-ctl kill db`, assert)
→ driver exits → agent emits `driver_exit{code}` → host runs the final census
and folds `(driver exit, ledger, census)` into the verdict.

## 3. Decision 1 — the control socket

**Recommendation: a unix-domain socket, `/run/tdvmm/ctl/sock`, served by the
agent in-guest, speaking `tdvmm-proto` `Request`/`Reply` line JSON verbatim.**

Options considered:

- **(a) UDS under `/run/tdvmm/`, served by the agent. ← RECOMMENDED.**
  - *Authorization is the mount.* Only the driver service gets the bind
    (decision 3), so reachability = privilege, exactly the closed-world idiom
    the FIFO established (bind the object, not a network). No auth protocol,
    no peer-credential guessing.
  - *It stays inside virtual time.* The whole request/response loop is
    guest-internal — this is the decisive property for §7. A UDS holds no
    host-side state, so nothing about it can ever need the egress-style
    real-rate gate.
  - *Zero new dependencies.* `std::os::unix::net::UnixListener` is std; the
    agent's "std + serde only, no libc" rule (`tdvmm-agent/src/main.rs:22-24`)
    holds. The listener fd and the accepted-connection fd join the existing
    blocked poll (`bridge.rs::poll2` generalizes from 2 fds to N) — infinite
    timeout, no timers, FF-transparent.
  - *Framing is already written.* The driver speaks exactly what the host
    speaks today: `encode_line(Request)` in, one `Reply` line out, `id`
    correlation, `ErrorKind` codes. The proto crate is the contract for a
    third party for the first time, which is what it was built for.
- (b) Extend the events FIFO into request/response. Rejected: a FIFO is
  one-way and shared by every service. Replies would need per-client return
  FIFOs and correlation, and — fatal — every service has the FIFO, so
  privilege separation is unobtainable without inventing auth inside the
  stream. The FIFO stays what it is: the fire-and-forget assertion channel.
- (c) TCP on the bridge gateway, mirroring egress (`forwarder.rs:76`).
  Rejected: the gateway is reachable from EVERY container, so a compromised
  ordinary service could kill peers — IP-based filtering inside the guest is
  auth-by-spoofable-address, strictly weaker than mount-based capability. The
  egress socket is safe *because* it grants only outward connectivity that the
  host mediates; a control socket grants god powers and needs a hard boundary.
- (d) Bridge the socket to the host and let the host engine execute (each
  driver command round-trips host-side). Rejected as the primary path: the
  agent already implements every fault op; the host's only roles are logging
  and verdict, and mirroring (below) provides both without doubling the serial
  hops. Crucially, (d) would put live host-side protocol state on the wire
  during driver waits and drag the ctl channel toward egress-style gating
  questions that (a) never raises.

**Who serves, who decides:** the agent serves and executes; the host observes
and judges. Every ctl command produces two agent-originated mirror events on
ttyS1 — `ctl_cmd` before execution, `ctl_result` after — vtsc-stamped by the
host on receipt, giving the JSONL the same `command`/`command_result` shape the
declarative engine logs today (`engine.rs:493-520`). The agent stays "transport
+ executor"; the verdict never leaves the host.

**Connection discipline:** `tdvmm-ctl` opens a connection per command
(connect → one request line → one reply line → close). Sequential serving; a
second connection queues behind the first. Socket EOF is *not* a death signal
(decision 4 owns that), so a driver can issue ten thousand commands or crash
mid-handshake without ambiguity.

**Lifecycle:** init creates `/run/tdvmm/ctl/` (0700) before the agent, like the
FIFO (`overlay/init:125`); the agent binds the listener before writing its
hello, so agent-ready implies socket-ready; compose-up runs after
(`overlay/init:158-159`), so the driver's bind always sees the live socket.
Gated like the forwarder: the VMM appends `tdvmm.driver=1` to the cmdline only
in driver mode (the `tdvmm.egress=1` pattern, `overlay/init:146-150`), and the
agent binds the listener only then — on a declarative run the ctl dir binds
empty and inert.

## 4. Decision 2 — the command surface

**Recommendation: reuse the existing `Request` op vocabulary verbatim; add two
ops; assertions ride the socket as an op that the agent re-emits as ordinary
guest events.**

| Op | Status | Notes |
|---|---|---|
| `kill` / `stop` / `start` / `partition` / `heal` | **verbatim reuse** | Same fields (`container`, `peer`, `timeout_s`), same agent code paths (`agent.rs:221-355`), same `ErrorKind` failures. The driver may not target itself (`kill`/`stop` with the driver's own service → error; its exit is the verdict signal, not a fault). |
| `containers` | verbatim reuse | The census — a driver's `wait_for` equivalent is a shell loop over this (or plain network probes, which are usually better: the driver is *inside* the stack). |
| `exec` | verbatim reuse | Run a command in a *peer* container (read a peer-local file, poke a process). Not privilege escalation — the driver already holds kill/partition. |
| `logs` | verbatim reuse | Available; rarely needed in-guest. |
| `event` | **new** | Body carries a `GuestEvent` (`always`/`sometimes`/`done` semantics unchanged, `ledger.rs:33-77`). The agent validates, stamps its seq, forwards as the same id-less bridged `Reply` the FIFO produces. Why via the socket rather than the FIFO the driver also has: strict program-order with the driver's own commands on one channel, and a confirming reply (the FIFO is fire-and-forget; loss only warns via seq gap). Non-driver services keep using the FIFO exactly as today. |
| `watch_driver` | **new, host→agent only** | Arms the driver-exit watcher (decision 6). The agent rejects it on the ctl socket. |

"The test passed/failed" and "end the run" are deliberately **not** ops: the
owner's mechanism — driver exit — is the one way to end, and its exit code is
the pass/fail intent. One mechanism, no second door to keep consistent.
(`event{kind:done}` remains accepted for compatibility but is redundant in
driver mode.)

`expect_death` becomes dynamic: the host accrues it from `ctl_cmd` mirrors of
`kill`/`stop` (service X deliberately killed at vtsc T), replacing the static
declaration (`schema.rs:30-35`) — the final census (`eval.rs:98-105`) then
exempts exactly the deaths the driver caused, plus the driver itself.

## 5. Decision 3 — designating the driver, and why the closed world holds

**Recommendation: one of the user's own compose services, marked at bake time
with `x-tdvmm-driver: true`; only that service receives the ctl bind.**

- The mark is a compose extension field on the service. `validate.rs` learns
  it: exactly one service may carry it; the driver may not set
  `restart: always` (a resurrecting driver breaks "exit ends the run").
  `emit_lock` strips the `x-` key from the emitted lock and appends
  `/run/tdvmm/ctl:/run/tdvmm/ctl:rw` to **that service only** — beside the
  FIFO append every service gets (`emit.rs:146-166`). `stack.lock` gains a
  `driver <service>` line, and the packed artifact metadata carries it so
  `tdvmm test` knows the driver without trusting the guest.
- Why bake-time, not a run-time flag: the socket bind must be in
  `compose.lock.yml`, which is fixed (and byte-gated) at bake. A run-time
  designation would need either the bind everywhere (privilege for all —
  rejected) or lock rewriting at run time (breaks the artifact identity
  model).
- Why bind the *directory*, not the socket file: a directory bind is
  race-free against listener (re)binding — the socket inode created by the
  agent after mount-set-up is visible through the bind; a file bind would pin
  a stale inode if the agent ever re-bound. The dir also carries the
  `tdvmm-ctl` client (§9). The FIFO's file-bind rationale ("no container can
  unlink the shared inode", `emit.rs:143-145`) doesn't transfer: only the
  trusted driver has this mount.
- Why not a tdvmm-provided sidecar: the owner's stated reality is that an app
  container drives the workload; forcing a sidecar splits the driver's world
  in two (workload in one container, control in another, coordination between
  them re-invents this design one level down). A user who *wants* a dedicated
  driver just adds a service and marks it — the mechanism doesn't care.

**Security invariant (must hold):** a non-driver service can reach exactly what
it reaches today — the events FIFO. The FIFO can *fail* a run
(`always:false`) but never control it; that is pre-existing and accepted. Two
new guards make the boundary real:

1. **Reserved event kinds.** `driver_exit`, `ctl_cmd`, `ctl_result` are
   agent-originated only. `bridge.rs::parse_event` (which today passes any
   kind through, `bridge.rs:169-183`) maps reserved kinds arriving *from the
   FIFO* to `invalid` — so no service can forge a driver death or a phantom
   fault mirror. Belt-and-braces: the host engine also refuses verdict
   transitions from non-reserved-path events.
2. **No network path to control.** The UDS is mount-scoped; the gateway
   exposes nothing new; nft partitions can never sever the driver's control
   path (a UDS is not IP — worth stating: a driver that partitions itself
   from a peer keeps full harness control while its *network* probes fail,
   which is exactly the observability you want during a partition test).

## 6. Decision 4 — driver death ends the test

**Recommendation: the agent watches the driver container with a `podman wait`
child under its poll; exit becomes a `driver_exit` event; the host folds
`(exit code, ledger, final census)` into the 0/1/2 contract.**

Mechanics:

- After the census first shows the driver's container **existing** (any
  state), the host sends `watch_driver{container}`. The agent resolves the
  service (`resolve_by_service`, `agent.rs:555`) and spawns
  `podman wait --interval 10s <id>`, adding the child's stdout fd to the poll
  set. `podman wait` on an already-exited container returns immediately, so
  the arm is race-free against a fast driver. On readability the agent reads
  the exit code and emits `driver_exit{code}` (agent-originated, reserved).
  Why a blocked child and not agent-side state polling: the agent must stay
  timer-free in its own loop; `podman wait` internally polls at the chosen
  interval, whose FF cost is bounded and priced in §7.3.
- Host engine (a new `DriverEngine` beside `ScenarioEngine`, same interface
  `main.rs` already consumes — `next_deadline`/`on_due`/`on_reply`/
  `on_horizon`/`record_abort`/`finalize`): phases
  `AwaitAgent(120s backstop)` → `AwaitDriverStart(census poll every 10 virtual
  s, default 300s timeout → infra 2; this is where compose-up failures
  surface)` → `AwaitDriverExit(no deadline — the AwaitDone idiom,
  `engine.rs:199-206`)` → `FinalCensus` → `Done`.
- **Verdict fold**, in order:
  1. driver exit ≠ 0 → **fail (1)**, failure = `driver exited <code>`;
  2. else ledger verdict (`ledger.rs:56-77`) — a recorded `always:false` or
     unsatisfied `sometimes` still fails a clean exit, mirroring how a
     passing census cannot launder a failed assertion today
     (`engine.rs` test `final_census_pass_folds_failed_assertion`);
  3. else final census with the accrued expect_death (+ the driver service
     auto-exempt — its exit is the mechanism) — an unexpected dead peer →
     **fail (1)**;
  4. else **pass (0)**.
  Infra (2) stays host-classified: agent dead, watch arm failed, horizon
  reached before `driver_exit`, ctl transport wedged.
- **Contrast with today:** `until: done` is a one-bit, forgeable-by-any-
  container completion signal with no failure channel except the ledger;
  `expect_death` is a static allowlist. Driver exit is (a) tied to the one
  authorized container, (b) carries pass/fail intent in-band, (c) cannot be
  spoofed via the FIFO (reserved-kind guard), and (d) composes with dynamic
  expect_death. The horizon keeps its role as the backstop for a driver that
  never exits — unchanged semantics: horizon mid-run = infra error 2
  (`engine.rs:212-237`), and driver runs should carry an explicit
  `max_virtual_time` (default proposed: 24h — decision point #4).

## 7. THE HARD PART — virtual time

This is where the design lives or dies, and the answer is unusually clean —
with honest costs.

### 7.1 No gate is needed: jump legality is structural

Egress pins real rate because sessions terminate **on the host**: real TCP
peers keep real clocks, so a jump would teleport virtual time out from under
live external state — hence quiescence `E == 0` and the phase gate
(`egress.rs:19-62`, `main.rs:1745-1772`). The ctl channel has **no host-side
term to put in any quiescence sum**: driver, socket, agent, podman, nft are all
guest-internal. The FF jump fires only in the HLT park (`main.rs:1698`), and
the guest only HLTs when *no guest process is runnable* — so "a ctl command is
in flight between driver and agent" and "the guest is jumping" are mutually
exclusive by the scheduler itself, not by a gate we must maintain. The one
host-visible artifact — mirror events on ttyS1 — is produced by guest PIO
writes, i.e. VM exits, and the main loop drains COM2 lines at the boundary
after every exit (`main.rs:1011-1038`) before any park can be reached; the same
already-proven property that makes `until: done` events safe today.

One deliberate consequence: an `exec` whose payload sleeps *does* fast-forward
mid-command (all guest processes timer-blocked → HLT → jump). That is correct
and is exactly how scenario execs behave today.

### 7.2 `sleep` IS the virtual-time API

`sleep 24h` in the driver → nanosleep → guest hrtimer → guest idles → the
LAPIC deadline is the next queue event → one jump. The partition after the
sleep fires at virtual ~24h having cost microseconds of wall time. This is not
new machinery — it is the `tdvmm.interval` idiom every workload already uses
(`service-loop.sh`: "between inserts this process genuinely blocks in `sleep`,
so the guest HLTs"). The design blesses it as the primary primitive:

- **relative time:** `sleep <dur>` (or `tdvmm-ctl sleep <dur>`, sugar).
- **absolute virtual time:** `tdvmm-ctl sleep-until <T>` — reads guest
  `CLOCK_MONOTONIC` (which *is* virtual time: the guest clock derives from the
  virtual TSC), computes the remainder, nanosleeps. This bounds cumulative
  slew to per-wakeup slack instead of a sum over the run, for drivers that
  want `at:`-like anchors. Accuracy = guest timer slack (~ms), documented, not
  promised as exact.
- **explicitly rejected:** an "advance to T" *host* op. The driver cannot be
  allowed to move the clock — the clock moves only when the guest is idle, and
  the guest is the authority on its own idleness. A driver that wants T to
  arrive sleeps until T; there is nothing else to want.

### 7.3 The watcher's cost, priced

`podman wait --interval 10s` arms a guest timer every 10 virtual seconds while
the driver runs. Each fires as one cheap FF hop (per-hop mean is tens of µs,
`telemetry.rs` `mean_hop_ns`): a virtual day of driver sleep ≈ 8,640 extra
hops ≈ well under a second of added wall time. Acceptable. If it ever matters,
the upgrade path is an inotify watch (raw syscalls, `sys.rs` precedent) on
conmon's exit file — zero timers, exact — at the cost of depending on podman's
internal `/run/libpod/exits` layout. Recommendation: ship `podman wait`,
keep inotify in the back pocket (decision point #5).

### 7.4 Determinism — the honest ledger

What the declarative engine guarantees that driver mode **cannot**:

- *Exact schedule anchors.* An `at: 2h` step is a queue event fired at an
  exact vtsc under an always-on discipline assert (`main.rs:1543-1548`). A
  driver's `sleep`s accumulate: each busy period consumes virtual time **at
  real rate**, so its virtual duration varies with host speed and load, and
  every subsequent timestamp inherits the slew. Two runs of the same driver
  produce the same *sequence* at different *timestamps*.

What driver mode keeps:

- *Program order.* Single vCPU, sequential driver, one agent loop, one serial
  channel: the order of commands, mirrors, events, and the exit is total and
  repeatable. The JSONL remains a totally-ordered, vtsc-stamped evidence
  trail — auditable and diffable in structure, not in timestamps.
- *Verdict meaningfulness.* The discipline the ecosystem already teaches
  (tigerbeetle's probe-robustness notes: assert observed effects, not
  schedules) becomes the *only* discipline: a correct driver waits for the
  effect it caused before asserting (`until ! pg_isready…`), so its verdict is
  a function of causal structure, which IS reproducible. A driver that
  asserts on wall-adjacent timing was flaky in YAML too; driver mode just
  removes the false comfort.

Consequences we accept and state:

- Driver runs are **excluded from byte-identity gates**. The declarative mode
  remains the golden-run basis (this repo's whole verification culture —
  `2b86ab69` and friends — stays on it). The report gains `"mode":"driver"`
  (additive field, schema 1 stays, following the `assertions` precedent) so
  consumers can tell.
- A failing driver run reproduces the same program, not the same schedule.
  Flake ownership moves to the test author's synchronization discipline —
  same as any concurrent test, said out loud in the docs.
- Unsolved, flagged honestly: there is no replay. A future
  `tdvmm-ctl at <T> <fault>` (driver *submits* absolute-vtsc fault events into
  the host queue, getting exact scheduling for the fault while keeping the
  driver for assertions) would recover exact anchors for the fault half — a
  plausible later hybrid, out of scope here.

### 7.5 Rejected virtual-time alternatives

- *Pin real rate while a ctl connection is open* (egress-style): kills the
  crown jewel (`sleep 24h` would take 24 hours) and defends against nothing —
  there is no external state to protect. Rejected outright.
- *Host-side scheduling API* ("driver uploads a timeline"): that is the YAML
  engine with extra steps; the driver already has `sleep`. Rejected as core
  (see the `at` hybrid above as a possible later extension).

## 8. Coexistence and migration

**Recommendation: a new mode beside the declarative engine; no deprecation.**
The declarative system is the reproducibility anchor and the regression basis;
driver mode is the expressiveness play. Concretely:

- Scenario YAML gains a mutually-exclusive top-level `driver:` block
  (`steps:` XOR `driver:`), e.g.:

  ```yaml
  name: faultlab-driver
  run:
    cmdline: "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1"
    max_virtual_time: 6h
  driver:
    start_timeout: 5m        # AwaitDriverStart bound (default 5m)
  ```

  The artifact must carry a baked driver service (validated against
  `stack.lock`); `run:` keeps the existing `baked < scenario < flag`
  precedence (`cli.rs:390-392`). `tdvmm test app.tdvmm --driver` with no
  scenario file runs on defaults.
- `main.rs` holds `enum Engine { Scenario(ScenarioEngine), Driver(DriverEngine) }`
  behind the existing call surface; `ledger`/`report`/`log`/`eval` are shared.
- Proto `SCHEMA` 3 → 4: ops `event` + `watch_driver`, reserved event kinds,
  golden fixtures regenerated in the same commit (the locked rule,
  `tdvmm-proto/src/lib.rs:38-44`). Old agents reject the new ops with
  `unknown_op` — the free-string `op` design absorbing exactly this.
- Migration is opt-in per stack. Nothing in an existing artifact or scenario
  changes byte-one; the ctl dir bind appears only in re-baked stacks that mark
  a driver.

### faultlab `kill-recover.yml`, re-expressed

Today: 8 host-side steps + static `expect_death: [db]` (§1 table; the YAML in
`testdata/stacks/faultlab/kill-recover.yml`). In the new model, `client` (the
probe container that exists *solely to be exec'd into*) becomes the driver, and
the exec round-trips vanish — the probes run natively:

```yaml
# compose.yml — one added mark
  client:
    x-tdvmm-driver: true
    entrypoint: ["/bin/sh", "/driver/kill-recover.sh"]
```

```sh
#!/bin/sh -e
CTL=/run/tdvmm/ctl/tdvmm-ctl
until pg_isready -h db -t 3; do sleep 5; done   # 1-2: ready + baseline
sleep 3600                                      # healthy for 1 virtual hour (FF)
$CTL kill db                                    # 3: fault (expect_death accrues)
until ! pg_isready -h db -t 3; do sleep 5; done # 4: effect observed
sleep 3600                                      # db stays dead 1 virtual hour (FF)
$CTL start db                                   # 5: recover
until pg_isready -h db -t 3; do sleep 5; done   # 6-7: recovery observed
$CTL event sometimes recovered true             # optional ledger evidence
exit 0                                          # 8 (census) runs host-side on exit
```

Notably better: connectivity checks are direct (no `exec` hop), `expect_death`
is implied by the driver's own `kill`, and the end-of-run census still runs
host-side. Notably lost: the `at: 1h/2h` anchors become `sleep 3600`s — the
kill fires at ~1h, not exactly 1h (§7.4).

## 9. User-facing shape

- **Marking:** `x-tdvmm-driver: true` on one service (§5).
- **Client:** `tdvmm-ctl` — the agent binary in a third multi-call mode
  (precedent: `tdvmm-agent forward`, `tdvmm-agent/src/main.rs:53-55`), copied
  by init into `/run/tdvmm/ctl/` at boot. Static musl → runs in any container,
  any base image, zero image modification, no version skew (same artifact as
  the agent it talks to). Raw `printf JSON | socat` works too — the protocol is
  the contract, the CLI is sugar.
- **CLI sketch** (exit 0 on `ok:true`, nonzero + stderr detail otherwise):

  ```
  tdvmm-ctl kill <svc> | stop <svc> | start <svc>
  tdvmm-ctl partition <a> <b> | heal <a> <b> | heal --all
  tdvmm-ctl exec <svc> -- <argv…>       tdvmm-ctl containers [--json]
  tdvmm-ctl event <always|sometimes|done> <name> <true|false>
  tdvmm-ctl sleep <dur> | sleep-until <virtual-T>
  ```

- **Run:** `tdvmm test shop.tdvmm --scenario driver.yml` (or `--driver`).
  Human summary gains a driver line; report gains
  `"mode":"driver"`, `"driver":{"service":…,"exit":…}`; JSONL gains
  `ctl_cmd`/`ctl_result`/`driver_exit` event types.

## 10. Change inventory

| What | Where |
|---|---|
| UDS listener + connection in the poll set; ctl dispatch; mirrors; reserved-kind FIFO filter; `watch_driver` + wait child | `tdvmm-agent/src/bridge.rs` (poll2→pollN, run_loop), `agent.rs` (dispatch reuse), `main.rs` (ctl gate + bind) |
| `SCHEMA` 4: `event`/`watch_driver` ops, reserved kinds, goldens | `tdvmm-proto/src/lib.rs`, `goldens/` |
| `tdvmm-ctl` multi-call mode | `tdvmm-agent/src/` (new `ctl.rs`) |
| `x-tdvmm-driver` validate + strip; driver-only ctl bind; `restart` guard; `stack.lock` `driver` line | `src/compose/{validate,emit}.rs`, `src/build/stack_lock.rs`, `src/artifact/` |
| init: mkdir ctl dir, copy tdvmm-ctl, gate agent ctl on `tdvmm.driver=1` | `testdata/initramfs-alpine/overlay/init` |
| `DriverEngine` (phases, dynamic expect_death, verdict fold); `Engine` enum; `driver:` schema block; report/JSONL additions | `src/scenario/` (new `driver.rs`; `schema.rs`, `report.rs`), `src/main.rs`, `src/cli.rs` |
| Cmdline append `tdvmm.driver=1` (egress pattern) | `src/main.rs` (~`main.rs:753-757`) |

## 11. Risks and open questions

| Risk / question | Position |
|---|---|
| A hostile driver is root-of-the-stack powerful | By design — the driver is the *user's own* trusted test code, like the YAML was. The boundary defended is non-driver services, and it holds (§5). |
| FIFO forgery of reserved kinds | Closed by the parse_event filter + host-side refusal (§5); needs a regression test in the same commit as the feature. |
| Driver exits before `watch_driver` arms | Closed: `podman wait` returns immediately on an exited container; the census only needs *existence* (§6). |
| Driver container restarts (compose restart policy) | Bake-time reject for the driver service; runtime: first `driver_exit` wins, later ones ignored. |
| `podman wait` poll hops during long sleeps | Priced (§7.3): sub-second wall per virtual day at `--interval 10s`; inotify upgrade path reserved. |
| Timestamps not reproducible run-to-run | Accepted and documented (§7.4); declarative mode remains the golden basis; `"mode":"driver"` marks the reports. |
| Two live channels (FIFO + ctl) for events | Deliberate: FIFO = every service, fire-and-forget; ctl `event` = driver, ordered + confirmed. One writer loop in the agent keeps host-side order total. |
| Hybrid runs (`steps:` preamble + driver) | Deferred — the driver can do its own readiness cheaper in-guest. Revisit if real drivers keep reimplementing `wait_for`. |
| Exact-vtsc fault anchors in driver mode | Unsolved by choice; the `tdvmm-ctl at <T>` queue-submission hybrid is the sketched future (§7.4). |
| Agent complexity creep | The agent gains a listener, a dispatcher branch, and a wait child — but no timers, no threads in the serve path, and no policy. The "transport + executor, host judges" doctrine is the line to hold in review. |

## 12. Owner decision points

1. **Transport (§3)** — recommendation: agent-served UDS in a driver-only
   directory bind, proto reuse verbatim. The load-bearing choice; sign-off
   requested.
2. **Driver designation (§5)** — recommendation: bake-time `x-tdvmm-driver` on
   one of the user's services (no sidecar), privilege = the mount.
3. **Verdict fold (§6)** — recommendation: driver exit ≠ 0 → fail(1); then
   ledger; then census with dynamic expect_death; horizon/agent-death → 2.
4. **Horizon default for driver runs (§6)** — recommendation: default
   `max_virtual_time: 24h` when unspecified (vs. requiring it explicitly).
   Cheap to change; pick one.
5. **Driver-exit watcher (§7.3)** — recommendation: `podman wait --interval
   10s` now; inotify only if hop cost ever shows up in practice.
6. **Coexistence (§8)** — recommendation: add driver mode, keep YAML as the
   reproducibility anchor, no deprecation path implied or scheduled.
