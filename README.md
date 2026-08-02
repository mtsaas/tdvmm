# deterministic-vmm — Step 4

A tiny KVM-based hypervisor (VMM) written in Rust. It boots **one** Linux guest,
with a single virtual CPU, to an interactive **serial shell**. Still no virtio,
no PCI, and **no guest-runtime networking** — but the guest is now
container-capable, runs a real, closed-world **Postgres service stack**, and
**fast-forwards its own idle time**: when the guest HLTs waiting for a future
timer, the VMM jumps virtual time straight to that deadline, so the workload's
hourly sleeps collapse and the guest lives through **hours in seconds** of wall
clock.

This completes Phase 1. The end goal is a VMM with deterministic execution and
fast-forwardable virtual time, for running reproducible service stacks. Each
step was built so the later ones (userspace clock ownership, then TSC
fast-forward) dropped in without rework.

- **Step 1** booted a minimal busybox initramfs to a serial shell (still here,
  as the minimal guest for later clock work).
- **Step 2a** added an **Alpine container guest**: a large initramfs whose whole
  rootfs lives in RAM, with **podman + crun + conmon + netavark** baked in. It
  boots, creates a podman **bridge network**, and runs a **digest-pinned**,
  pre-baked image — all **fully offline** (a closed world: no image is pulled
  and nothing talks to the outside at runtime).
- **Step 2b** runs the real Phase-1 workload on that same closed world:
  **Postgres** plus a tiny **service** container that, every interval, inserts a
  row and trims the table to a maximum size. The service reaches Postgres **by
  name** over the bridge (netavark DNS). Both images are baked; nothing is
  pulled at runtime. This is the workload Step 4's fast-forward will demo
  against. Still no virtio-blk / virtio-net.
- **Step 3a** lays the virtual-clock groundwork without changing any
  guest-visible behavior. It moves **TSC calibration off the PIT** by passing
  the host CPUID frequency leaves (`0x15`/`0x16`) through, so the guest derives
  `tsc_khz` directly from CPUID; builds the **virtual clock** (`vtsc_now()` =
  the guest's own TSC, read once from KVM's TSC offset) plus a **cycles<->ns**
  module and a vtsc-ordered **event queue** that Step 3b's userspace interrupt
  controller will drive; and adds a **userspace PIT counter stub** (a pure
  function of vtsc, no interrupts) that is ready to take over once the in-kernel
  PIT is retired. The in-kernel PIT and irqchip both **stay for now**: this
  guest boots in APIC "virtual wire mode" and uses the i8253 PIT (IRQ0) as its
  sole tick source, so the in-kernel PIT is removed only in Step 3b, together
  with the userspace LAPIC that replaces the tick (and an MP-table fix).
- **Step 3b** gives the guest a **userspace interrupt controller we own**, so
  its tick is a timer we drive off virtual time (ready for Step 4's
  fast-forward). Two parts: **(i)** the guest is moved off APIC "virtual wire
  mode". The pinned kernel gains `CONFIG_X86_MPPARSE=y` (it was silently off, so
  no MP table could ever be parsed) and the MP table is fixed to route the timer
  correctly (**ISA IRQ0 -> IO-APIC pin 2**, serial **IRQ4 -> pin 4**); with
  x2APIC masked in CPUID, the guest performs IO-APIC setup and enters
  **symmetric-I/O mode**. **(ii)** the in-kernel irqchip and in-kernel PIT are
  dropped in favor of a **userspace LAPIC + IO-APIC** (xAPIC MMIO at
  `0xFEE0_0000` / `0xFEC0_0000`), a **masked 8259 stub**, and the LAPIC's
  **one-shot / periodic timer** as the tick — every timer deadline living only
  as a `(vtsc, event)` entry in the event queue. A halted guest **parks in
  `ppoll`** on a `timerfd` + stdin (~0% host CPU when idle); that park is the one
  place Step 4 swaps "wait real time" for a "jump the TSC offset". See
  **"Step 3b notes"** below for the two determinism caveats.
- **Step 4** makes idle time **free**: it replaces the park's real-time *wait*
  with a *jump*. When the guest is HLTed waiting for a timer, the VMM computes
  `Δ = next_event_vtsc − vtsc_now()`, adds `Δ` to the cached **TSC offset**
  (write-through to `KVM_VCPU_TSC_OFFSET`), fires everything now due, and loops —
  so virtual time lands *exactly* on the next deadline and the guest experiences
  the elapsed hours instantly. Nothing else changes: the wake path (IRR →
  injection window → RUNNABLE) is unchanged Step-3b machinery; only the wait is
  replaced. Fast-forward is a runtime flag, **`--ff on|off` (default on)**; with
  it off the Step-3b real-wait park is used instead (the A/B for timing bugs and
  the right mode for an interactive console). The in-kernel `--irqchip kernel`
  A/B backend — which cannot fast-forward, its timer runs on the host clock — is
  **removed** in this step. See **"Step 4 notes"** below.

## Phase 2a — arbitrary compose stacks (a supported subset)

Phase 1's single hard-coded workload (`workload.sh`, now **deleted**) is replaced
by a general harness that runs **docker-compose stacks** — closed-world, and still
fast-forwardable. The deliverable is **not** "any compose file": it is a
well-defined **supported subset**, **loud bake-time rejection** of everything
outside it, and the old Postgres workload passing through the generic path.

- **Engine.** The genuine **Docker Compose v2 CLI** (a pinned, sha256-verified
  static Go binary — v5.3.1) is baked into the guest and driven against
  **`podman system service`**'s Docker-compatible API socket
  (`DOCKER_HOST=unix:///run/podman/podman.sock`). The API service runs with an
  **idle timeout**, so once `compose up -d` has created the containers it
  **exits** — the guest is then left holding only **conmon + the workload
  containers**, no resident engine (neither dockerd nor podman-compose is used).
- **The bake pipeline** (`guest/bake-stack.sh`, host-side, build time). Given a
  `compose.yml` it: validates it against the subset; for each `image:` resolves a
  digest and pulls **by digest**, squashing large images to one vfs layer
  reproducibly (`--timestamp`, config-equivalence gate); emits **`compose.lock.yml`**
  — the only compose file the guest sees, with images pinned `@sha256`, published
  `ports:` stripped (warned), relative **read-only** binds materialized into the
  guest and rewritten to in-guest absolute paths, and `COMPOSE_PROJECT_NAME`
  pinned; estimates guest RAM and warns if it looks short; and assembles a
  **per-stack initramfs** plus a `stack.lock` ledger (image digests + artifact
  hashes).
- **Loud rejections** (each a clear `DVMM_BAKE_REJECT` at bake time, tested by
  `scripts/bake_reject_test.sh`): absolute host bind-mounts, `external:` networks,
  `pull_policy: always`, `network_mode: host`, and a `build:` context whose base
  image is **not digest-pinned** (`build:` itself is supported — see "Phase 2b
  (item 1)"). Published `ports:` are the one exception: **warn + strip**, not
  reject. (In 2a, healthchecks / `depends_on: {condition: service_healthy}` were
  also rejected; **Phase 2b items 3 & 4** un-reject them — see below.)
- **Closed world at runtime.** No default route on the guest (egress can't leave);
  service-name DNS only (netavark + aardvark); external lookups fail. A stack that
  needs the internet gets a crisp error, never a silent hang.
- **Dogfood.** The Phase-1 Postgres + insert/trim service is re-expressed as
  `guest/stacks/dogfood/compose.yml` and run through the full pipeline. Because 2a
  is image-based only, the insert/trim service reuses the pinned Postgres image
  (it already ships `sh`/`psql`/`pg_isready`) with its entrypoint overridden to a
  bind-mounted loop script — so **every** image is one real upstream digest. It
  passes the complete Step-4 fast-forward gate set (`scripts/ff_demo.sh`).
- **RAM.** The 2a spec default is 4 GiB, but the current VMM maps guest RAM only
  **below the 32-bit MMIO gap** (`arch.rs` `MMIO_MEM_START = 0xC000_0000` = 3 GiB),
  so the effective default is **3072 MiB** (dogfood peaks ~1.2 GiB). 4 GiB needs a
  VMM high-memory region — VMM-core work, out of 2a scope; flagged, not improvised.

Build and run a stack:

```sh
guest/initramfs-alpine/build_rootfs.sh        # base 2a guest (busybox selftest)
guest/bake-stack.sh guest/stacks/dogfood/compose.yml   # -> initramfs-alpine-dogfood.cpio.gz
scripts/ff_demo.sh 24                          # dogfood through the FF gate set
scripts/bake_reject_test.sh                    # the loud-rejection tests
scripts/bake_repeat_test.sh                    # same input -> identical lock + digests
```

## Phase 2b (item 1) — a Go A/B stack, `build:` contexts, a comparison harness

Phase 2a ran only pre-built `image:` services. This slice adds three things, all
to answer one question: **how does a chatty language runtime behave under
fast-forward?**

- **A Go insert/trim service** (`guest/stacks/go-ab/`), functionally IDENTICAL to
  the shell dogfood service — same SQL, same schema, same env knobs, same
  `DVMM_ROWCOUNT` markers. It even shells out to the same `psql`/`pg_isready`, so
  the ONLY difference between the two stacks is the language runtime (a Go binary
  vs a POSIX shell). One is the control, the other the treatment.
- **`build:` contexts** (new). A service may build from a Containerfile/Dockerfile
  instead of pulling an image. The build runs **host-side at bake time only**
  (never in the guest); its base images must be **digest-pinned**; and the built
  image flows through the same squash / seed-store / `compose.lock` pipeline as any
  pulled image. The Go service is the first `build:` exercise.
- **Built-image reproducibility.** Built images are judged by **content identity**
  (the squashed layer's DiffID), not image ID (which isn't reproducible). A
  reproducible Go build (`-trimpath -buildvcs=false -buildid=`, `CGO_ENABLED=0`,
  pinned toolchain) plus `--timestamp` mtime normalization makes that DiffID stable
  bake-to-bake, recorded in `stack.lock` as `content_id=`. `bake_repeat_test.sh`
  gates it: same source -> identical content identity + identical `compose.lock.yml`.
- **A permanent comparison harness** (`scripts/compare_stacks.sh`) runs any two
  stacks under fast-forward and prints a side-by-side report: hops per
  virtual-hour, speedup, per-hop cost mean/p99/max, the Δvtsc histogram, and a
  real-vs-virtual time accounting that doubles as a **busy-wait tripwire** (a
  runtime that spins instead of parking burns real execution time during nominal
  idle). It consumes the VMM's new `--metrics-out <path>` per-run metrics file
  (which reuses the existing Δvtsc histogram + jump stats).

```sh
guest/bake-stack.sh guest/stacks/go-ab/compose.yml          # builds the Go service host-side
scripts/bake_repeat_test.sh guest/stacks/go-ab/compose.yml  # reproducible content-identity
scripts/compare_stacks.sh dogfood go-ab                     # shell (control) vs Go (treatment)
```

**What the numbers say.** With Postgres in the loop, the shell and Go stacks are
nearly identical in chattiness (~390k hops per virtual-hour each, ~530× speedup,
per-hop ~0.3 µs). Postgres's background processes wake the guest every ~0.1–1 ms
and **dominate** the profile; the Go runtime — with `GOMAXPROCS=1` (the single
vCPU) and a faithful sleep-the-interval loop — backs its `sysmon` off to deep idle
and adds only a negligible number of wakeups on top. So the expected "Go is far
chattier" result does **not** show here: Postgres is the bottleneck, not the
runtime, and neither runtime busy-waits (the tripwire is clean — both fast-forward
at ~530×). The measured chattiness is a **floor** (single core, no native driver);
a multi-core guest or an app-level ticker would be strictly chattier.

## Phase 2b (items 3 & 4) — healthchecks + rw binds / named volumes

Two more of the compose subset's rejections become supported features, so more
real stacks bake unchanged. Still closed-world, single-writer, deterministic-bake.

- **Healthcheck ticker + `depends_on: {condition: service_healthy}`.** podman has
  no healthcheck auto-runner without a systemd timer, and this guest runs busybox
  init — so a container's health would sit at "starting" forever and a
  service-healthy gate would hang `compose up`. The guest now runs a small
  **healthcheck ticker** (`overlay/usr/local/bin/healthcheck-ticker.sh`, started
  by `compose-up.sh` **only** when the baked lock declares a healthcheck): a plain
  `sleep` loop that runs `podman healthcheck run <c>` for every running container
  with a healthcheck, so health flips to **healthy** and compose's gate resolves
  and starts the dependents. It is FF-friendly (the guest HLTs between ticks;
  fast-forward collapses the waits). `bake_compose.py` no longer rejects
  healthchecks / `service_healthy`. Test stack: `guest/stacks/health-gate/`
  (`scripts/smoke_test_healthgate.sh`) — the dependent starts only after the gate
  is healthy; ran at ~20× under FF.
- **Relative read-write binds.** 2a materialized only relative **read-only** binds.
  2b also materializes relative **read-write** binds (copied into the guest image,
  rewritten to in-guest absolute paths, mounted `rw`); writes land on the guest
  tmpfs and are **ephemeral** (identical baked initial state every boot — fine in
  the closed world). **Absolute** host binds stay **rejected**, ro or rw (the
  closed-world boundary — `guest/stacks/rejects/absbind{,_rw}.yml`).
- **Named volumes** get full coverage: podman creates them in the tmpfs-backed
  store, so they are ephemeral and empty + identical every run. Test stack:
  `guest/stacks/rwbind/` (`scripts/smoke_test_binds.sh`) — a materialized rw bind
  and a named volume, both written + read back; ran at ~800× under FF.

```sh
guest/bake-stack.sh guest/stacks/health-gate/compose.yml   # healthcheck-gating demo
scripts/smoke_test_healthgate.sh                           # gate resolves; dependent waits
guest/bake-stack.sh guest/stacks/rwbind/compose.yml        # rw bind + named volume demo
scripts/smoke_test_binds.sh                                # writes land; runs under FF
```

## Phase 2b (corpus) — real-world-shaped stacks that exercise features together

The earlier 2b stacks each prove ONE feature in isolation. The **corpus** proves the
supported subset holds up on **realistic, multi-feature compose files** — the kind a
real project would write. Three stacks (`guest/stacks/`), each closed-world (images
digest-pinned or built host-side and baked) and fast-forwardable, run through a single
runner: **`scripts/corpus_test.sh`** (bakes each if needed, boots it under FF with a
virtual-time horizon + `--metrics-out`, then asserts functional correctness + health
gating order + the per-hop ≤500 µs mean gate).

- **`webstack`** — a web/api + **redis** + **postgres** shape. The `api` (a `build:`
  context) starts only after **BOTH** `postgres` and `redis` report healthy (two
  `depends_on: {condition: service_healthy}` gates, resolved by the guest healthcheck
  ticker), then reaches both backends **by name** (inserts to postgres, bumps a redis
  counter). Exercises: multi-service, dual health gates, a build context, service-name
  DNS to two backends.
- **`configpipeline`** — a `build:` context worker + a busybox sidecar. Exercises a
  relative **read-write** config bind (`./config` materialized + written back), a
  **named volume SHARED across both services** (the worker publishes, the sidecar reads
  it back), and a service-started `depends_on`.
- **`svcchain`** — a 3-tier **health-gated chain** `db → backend → frontend`. The
  `backend` (postgres image, entrypoint overridden via a relative **read-only** bind)
  waits for `db` healthy, connects, then flips its **own** healthcheck healthy, which
  unblocks the `frontend`. Proves gated ordering propagates across **two hops**.

```sh
scripts/corpus_test.sh                 # bake (if needed) + run all three under FF
BAKE=1 scripts/corpus_test.sh webstack # force a re-bake of one stack
```

All three fast-forward at hundreds of × with a per-hop mean well under the 500 µs gate
(the same VMM property the dogfood/go-ab comparison measures); the numbers are a stack
property, the ≤500 µs mean is the hard VMM gate.

## OP-1a — the `.dvmm` single-file artifact + the run/inspect/verify/boot verbs

Everything a baked stack needs to run is now packed into **one self-contained
file**, `<stack>.dvmm`, and the binary grew developer verbs to run, inspect, and
verify it. The scripts still do the baking; the bake now ends by emitting a
`.dvmm`.

- **The format (v1).** A plain, **uncompressed outer tar** (`tar tvf` reads it)
  with four members in a fixed canonical order: `manifest.json`,
  `compose.lock.yml`, `kernel` (the vmlinux), `initramfs` (the per-stack guest,
  already gzip'd). The encoding is **deterministic** — `mtime=0`, `uid=0`,
  `gid=0`, fixed mode, fixed order, no volatile fields — so identical inputs
  produce a **byte-identical** `.dvmm`. Identity = **sha256 of the whole file**
  (there is no embedded self-hash — chicken-and-egg); `manifest.json` records a
  sha256 for every *other* member and `verify` closes the loop. `manifest.json`
  also carries the full anchor set (member hashes, the effective-CPUID snapshot,
  the compose-engine version+hash, image digests + squash/build provenance, the
  bake toolchain versions, the RAM estimate) and the **baked run-defaults** (mem,
  cmdline, ff, horizon). Member-name prefixes `scenario/`, `record.log`, and
  `snapshot/` are **reserved** for later phases (the reader ignores unknown
  members). The Rust side (`src/artifact.rs`) is a hand-rolled deterministic
  USTAR writer/reader — full control over the byte layout for the reproducible
  guarantee, and cheap single-member reads so `inspect` never touches the big
  payloads.

- **`dvmm run <stack.dvmm> [overrides]`** — load the artifact **into memory** (no
  temp-dir extraction: the kernel is parsed from a byte buffer, the initramfs
  written straight into guest RAM), apply the baked run-defaults, and boot. Fully
  **offline** (only `/dev/kvm`; no network, no host deps). Member-hash
  verification on load is **default-ON** (`--no-verify` to skip) — a corrupt or
  tampered artifact is refused before boot.
- **`dvmm inspect <stack.dvmm>`** — print `manifest.json` (reads **only** the
  manifest member; milliseconds even for a 200 MiB artifact).
- **`dvmm verify <stack.dvmm>`** — recompute every member hash against the
  manifest and print the file's sha256 identity; **nonzero exit** on any mismatch.
- **`dvmm boot --kernel <..> --initrd <..> [flags]`** — the original raw
  invocation, preserved as the low-level dev verb (smoke tests + VMM development
  use `boot`; artifact users use `run`).
- **Override precedence is LOCKED: baked run-defaults < CLI flags.** Every run
  prints an **effective-config** line with per-knob provenance (the future
  record-log preamble), e.g.
  `[dvmm] effective-config: mem=3072 (baked) ff=off (flag) max-virtual-time=36h (baked) ...`.

```sh
guest/bake-stack.sh guest/stacks/dogfood/compose.yml -o dogfood.dvmm  # bake -> one .dvmm
dvmm inspect dogfood.dvmm          # the manifest (fast; manifest member only)
dvmm verify  dogfood.dvmm          # member hashes vs manifest + the sha256 identity
dvmm run     dogfood.dvmm --max-virtual-time 24h   # boot it, baked defaults + overrides
scripts/artifact_test.sh dogfood svcchain          # the OP-1a acceptance gate set
```

## Interactive-console polish

Two cosmetic fixes make an interactive `run.sh` session read cleanly, **without**
touching the guest byte stream or any time behavior (both keyed only off the existing
isatty signal, never off wall-clock):

- **Log lines align to column 0.** At an interactive console the tty is in raw mode
  (the guest owns the byte stream, so the terminal's `\n`→CRLF translation is off). dvmm
  now terminates its **own** log lines (startup, telemetry, WARN, horizon diagnostic)
  with `\r\n` and a leading `\r` when raw mode is active, so they no longer staircase.
  In non-tty/harness runs nothing changes (plain `\n`).
- **Quiet interactive telemetry.** The periodic HLT-rate / fast-forward rollup (a
  perf metric) is suppressed in an interactive session (tty + no `--metrics-out` and no
  `--max-virtual-time`), so it stops interrupting the prompt every ~15 s. It is still
  emitted for every harness path (non-tty, `--metrics-out`, or a horizon set) and the
  on-stop summary + metrics file are unaffected.

## TEST-1a — test a stack against a scenario (`dvmm test`)

You can now *test* a stack: drive virtual time, wait for readiness, probe guest
state, assert, and get a verdict — `dvmm test <stack.dvmm> --scenario s.yml`.
This is the developer-testing foundation (fault injection is the next slice,
TEST-1b; the schema and agent protocol already leave room for it).

- **Control channel.** A **second 16550** (COM2 / ttyS1) is added to the VMM,
  reusing the serial model. It is the modeled control channel between the host
  and a tiny guest-side **`dvmm-agent`** (a static, reproducible Go binary baked
  into every guest, running *outside* the containers). The agent **blocks reading
  ttyS1** — a blocked read arms no timer, so it produces no wakes and an idle
  guest with the agent baked in still fast-forwards normally. Protocol:
  line-delimited JSON. The guest kernel gains `CONFIG_SERIAL_8250_NR_UARTS=2` so
  ttyS1 exists (see `guest/kernel/test1a-com2.config`).
- **The LAW for commands.** Every command is delivered by the VMM **at its
  scheduled vtsc as a queue entry** — a `(vtsc, ScenarioStep)` event that
  GENERALIZES the existing `(vtsc, StopRun)` horizon — never an ad-hoc side
  channel. `at: 24h` therefore fast-forwards through idle exactly like any other
  deadline. IRQ3 (COM2) was already routed identity to IO-APIC pin 3 by the MP
  table, so no interrupt-routing change was needed.
- **Scenario (host YAML).** A timeline of steps. Each has an `at:` virtual
  duration and one kind: `exec` (run a command in a container; assert exit code
  and/or a stdout regex — covers SQL via psql, HTTP via curl), `containers`
  (`all_running` / `none_exited_nonzero`), or `wait_for` (poll a probe every
  `every:` until a predicate holds or `timeout:` passes — readiness). **Static
  validation** happens before boot (sub-second): unknown keys, bad durations, a
  bad regex, or an unknown service (checked against the artifact's
  `compose.lock.yml`) are rejected loudly.
- **Verdict.** A structured **JSONL run log** (one line per event: scenario
  steps, commands + results, probe outcomes, assertions, container census, FF
  stats) plus a **JSON report** file and a human summary table. The artifact
  sha256 + the scenario (+ its sha256) + the JSONL = a reproduction package. This
  schema is a documented, versioned contract (`schema: 1`), shared with the
  future e2e runner. **Exit codes:** `0` all assertions passed; `1` an assertion
  / readiness failure (or a container exited nonzero); `2` an infrastructure
  error (bad scenario, boot/bake/agent failure, or the agent couldn't reach a
  container). CI can tell "your stack is wrong" (1) from "the tool broke" (2).

```sh
guest/bake-stack.sh guest/stacks/dogfood/compose.yml -o dogfood.dvmm  # agent baked in
dvmm test dogfood.dvmm --scenario guest/stacks/dogfood/dogfood.yml    # -> PASS, exit 0
scripts/test_scenario.sh          # the TEST-1a gate set (pass / wrong->1 / infra->2)
```

The **dogfood-as-scenario** acceptance (`guest/stacks/dogfood/dogfood.yml`) is the
platform testing itself: `wait_for` Postgres ready, fast-forward through virtual
hours, then `exec` a `psql` probe asserting the events-table row count **accrues**
and then **caps** at MAX_ROWS.

## TEST-1b — fault injection (the core testing value)

You can now inject **failures** and check how a stack behaves under them and
recovers. Faults are scenario **action steps** at an `at:` time, delivered the
exact same way assertions are — as scheduled `(vtsc, ScenarioStep)` queue entries
(THE LAW: no side channels, replayable) — so a fault at `at: 24h` fast-forwards
through idle like any other deadline and lands on time.

- **Action step kinds** (extend the scenario schema, same static pre-boot
  validation as assertions — unknown service ⇒ exit 2 before boot):
  - **`kill <service>`** / **`stop <service>`** / **`start <service>`** —
    container lifecycle (`podman kill`/`stop`/`start` on the resolved
    compose-service container; kill/stop then WAIT for the container to actually
    stop, so a following census is deterministic).
  - **`partition [A, B]`** / **`heal [A, B]`** (or **`heal: all`**) — a network
    fault: drop **all** traffic between the two services' container IPs, both
    directions. The agent resolves each service→container→IP and installs
    **nftables drop rules** in the guest root netns, in its OWN table
    (`inet dvmm_faults`), rebuilt atomically from the active-partition set on every
    change; `heal` removes them. (The guest kernel has no nft `bridge` family, so
    this uses the `inet` FORWARD hook — `bridge-nf-call-iptables`, already set by
    netavark, makes bridged intra-network packets traverse it.)
- **Expected-death policy.** A scenario declares deliberately-killed services with
  a top-level `expect_death: [<service>...]`. Any **other** container that exits
  nonzero is an **unexpected death → verdict fail (exit 1)** — enforced by an
  implicit end-of-run container census (and honored by the `containers:
  none_exited_nonzero` assertion). A SIGKILLed Postgres exits 137; declaring it in
  `expect_death` is what stops that from failing the run.
- **Effect + recovery are ordinary assertions.** Use the existing `wait_for` /
  `exec` / `containers` steps to prove the fault took hold (e.g. a `pg_isready`
  probe now returns nonzero) and that recovery worked (it returns zero again).
- **Replayable log.** Every fault is one JSONL line with its `ts_vtsc`, op, and
  service(+peer), plus its `command_result` — the reproduction package a replay
  needs.

The fault set lives in `guest/stacks/faultlab/` — a two-service stack (`db` +
an idle `client` that probes `db` by name) built for exactly this:

```sh
guest/bake-stack.sh guest/stacks/faultlab/compose.yml -o faultlab.dvmm
dvmm test faultlab.dvmm --scenario guest/stacks/faultlab/kill-recover.yml    # -> PASS, exit 0
dvmm test faultlab.dvmm --scenario guest/stacks/faultlab/partition-heal.yml  # -> PASS, exit 0
dvmm test faultlab.dvmm --scenario guest/stacks/faultlab/unexpected-death.yml# -> FAIL, exit 1
scripts/test_faults.sh    # the TEST-1b gate set (kill+recover / partition+heal / unexpected-death / scheduled-at-vtsc / unknown-service)
```

> **netem delay/loss is TEST-2, not here.** It needs `sch_netem` in the pinned
> kernel (deliberately not built in), so a scenario that asks for it is a loud
> reject — do not rebuild the kernel for it in this slice.

## What it does

- 1 vCPU, 64-bit long mode, direct kernel boot (loads an ELF `vmlinux`).
- Builds the Linux boot data by hand: the zero page (`boot_params`), the
  **E820** memory map, and an **MPTable** (so the guest can discover its CPU
  without ACPI).
- A 16550 serial port (via `vm-superio`) is the console. Your keystrokes go in,
  the guest's output comes out.
- The interrupt controller is a **userspace LAPIC + IO-APIC we own** (no
  in-kernel irqchip): every LAPIC/IO-APIC access is an MMIO exit we service, and
  the LAPIC timer — driven by virtual time — is the guest's tick. This is what
  lets Step 4 fast-forward the tick by moving virtual time.
- A **CPUID filter** hides KVM's paravirtual clock from the guest, hides
  MWAIT (so an idle guest traps as HLT), and advertises an invariant TSC.
  This is what keeps later virtual-time work from being silently undone. It
  masks the **entire** `0x4000_0000`–`0x4000_00FF` hypervisor-leaf range, so
  the guest sees no hypervisor leaves at all and cannot load `cpuidle-haltpoll`.

## The Alpine container guest (Step 2a)

The primary guest is now an **Alpine** initramfs (`guest/initramfs-alpine/`).
Everything runs in RAM; there is no disk and no network at runtime.

- **`podman run` works offline.** Images are pinned **by digest** and pre-baked
  into a podman store on the host at build time (`podman pull --root ...`);
  the guest copies that store into a tmpfs and runs from it with `--pull=never`.
- **A bridge network is created** with `podman network create appnet`
  (netavark + the guest kernel's nftables NAT). The container gets a bridge IP;
  there is no uplink (closed world).
- **The clock is fixed.** There is no RTC (that is Step 3). `init` sets a fixed
  baked-in epoch (`date -s @<BUILD_EPOCH>`, currently 2026-08-01T00:00:00Z), so
  every boot starts at an identical wall time. No `chronyd`/`ntpd` runs.
- **Root is a tmpfs, not ramfs.** `init` first `switch_root`s from the
  initramfs onto a tmpfs (still entirely in RAM). This is required for
  containers: an OCI runtime `pivot_root`s into the container rootfs, and the
  kernel refuses `pivot_root` when the mount-namespace root has no parent mount
  — which is exactly the initramfs `rootfs`. A tmpfs root has the initramfs as
  its parent, so container `pivot_root` works.
- **Storage driver is `vfs`.** Podman's store sits on a size-capped tmpfs at
  `/var/lib/containers`. `vfs` (full-copy layers) is used instead of `overlay`:
  overlayfs-on-tmpfs needs features tmpfs does not reliably provide, and the
  baked image is tiny, so the extra RAM is negligible.

## The Postgres workload (Step 2b)

On top of the 2a guest, Step 2b runs a real closed-world service stack. When the
guest is booted in **workload mode** (`dvmm.workload=1` on the kernel cmdline),
`init` runs `workload.sh`, which uses plain `podman run` (no compose tool) to:

1. create the `appnet` bridge network (netavark),
2. start **Postgres** (`postgres:16-alpine`, pinned by digest) detached on
   `appnet` as the fixed name `postgres`. Its data dir is on tmpfs and fresh
   every boot (desired). `POSTGRES_HOST_AUTH_METHOD=trust` is fine for this
   closed test; a baked SQL file creates the `events` table on first start
   (`id bigserial primary key, ts timestamptz default now(), value text`),
3. wait until Postgres accepts connections, then
4. start the **service** (a tiny Alpine + `psql` image we build) detached on
   `appnet`. It reaches Postgres **by name** via netavark DNS.

The service is a **POSIX-shell sleep-loop** (deliberately not Go/JVM, whose
runtimes wake the CPU too often and would wreck the later idle profile). Each
cycle it inserts one row, trims the table to at most `MAX_ROWS` newest rows, and
`sleep`s `INTERVAL_SECONDS` — so the guest genuinely **HLTs** when idle. It
prints one marker per cycle so the workload can be checked over serial:

```
DVMM_ROWCOUNT=<n> iter=<i> max=<MAX_ROWS> ts=<time>
```

**Knobs** (env / cmdline `dvmm.interval=` `dvmm.maxrows=`): `INTERVAL_SECONDS`
defaults to **3600**, `MAX_ROWS` to **1000** (what Step 4 demos). The acceptance
test overrides both to small values to prove cadence and trimming fast.

Postgres's own background timers (bgwriter/walwriter ~200 ms, autovacuum ~60 s)
are left at their defaults — they periodically wake the guest, which is expected
realism for the later fast-forward work, not a bug to fix. Only the **service**
is made to genuinely sleep between inserts.

### Baking the images: the single-layer (squash) policy

The graph driver is **vfs**, which stores every layer as a full cumulative copy.
That is free for tiny images but catastrophic for Postgres: `postgres:16-alpine`
as-pulled is **~1.8 GiB** of duplicated layers in a vfs store — it would not fit
the guest, and could not even boot (the initramfs would decompress to several
GiB). So the heavy images are baked as a **single vfs layer**:

- **Postgres** is pulled by its pinned digest, then repackaged to one layer via
  a **pure `FROM <digest>` Containerfile** (nothing else). The single-layer store
  is **~290 MiB**. A build-time **config-equivalence gate** fails the build if
  the squashed image's `Entrypoint` / `Cmd` / `Env` (incl. `PGDATA`) / `Volumes`
  drift from the upstream image, so the repackage can never smuggle in a config
  change. The image the guest runs is filesystem-identical to the pinned one.
- **The service** image is ours, so it is **built single-layer from the start**
  (`podman build --squash-all`). One policy, no special cases.

`images.lock` records, per image, the upstream digest, whether it was squashed,
the podman version used, and a content identity of the **result** (the squashed
layer's DiffID) — image IDs are not reproducible, filesystem content is.

## The single-writer invariant

> All effects on guest state (guest memory, interrupt raises, device register
> state) commit on the vCPU thread at loop boundaries. Host I/O may happen
> off-thread; its effects may not.

There are **no** exceptions now. The early-Step-1 background stdin thread (the
one temporary exception) is gone: with the userspace LAPIC, the vCPU thread owns
console input directly — it reads stdin while parked at a HLT exit and raises the
serial IRQ itself. The Step-4 TSC-offset bump is likewise written only on the
vCPU thread, between `KVM_RUN`s.

The vCPU loop keeps its shape from day one:

```
loop { service_timers(); run(); handle_exit(); }
```

`service_timers()` fires every guest timer due at the loop boundary (or, at a HLT
park, waits-or-jumps to the next one).

## Layout

```
Cargo.toml
src/
  main.rs      vCPU loop + orchestration (+ the single-writer invariant doc)
  arch.rs      guest-physical layout constants
  memory.rs    guest RAM + KVM_SET_USER_MEMORY_REGION
  lapic.rs     userspace xAPIC (MMIO): IRR/ISR/PPR/EOI/LVT + one-shot/periodic
               timer; counts->TSC cycles = the exact CPUID-0x15 EBX/EAX integer
               ratio (no float), all a pure function of vtsc
  ioapic.rs    userspace IO-APIC (24 edge-triggered RTEs, index/data window)
  pic.rs       masked 8259 stub (init-and-mask, delivers nothing)
  park.rs      idle park: FF on = JUMP the TSC offset to the next deadline
               (Step 4); FF off = timerfd + ppoll real-wait (Step 3b). The one
               place virtual-time deadlines become a wait-or-jump.
  cpuid.rs     CPUID filter (mask kvmclock/MWAIT/x2APIC/TSC-deadline, expose
               invariant TSC, pass through host frequency leaves 0x15/0x16)
  vtsc.rs      virtual-clock authority: vtsc_now() = host TSC + CACHED TSC offset
               (shared cell; no ioctl in the hot loop), the cycles<->ns module,
               and bump_offset() = Step 4's write-through TSC-offset JUMP
  pit.rs       userspace 8254 PIT counter stub (ports 0x40-0x43 + ELCR);
               counter is a pure function of vtsc, no interrupts
  events.rs    the one vtsc-ordered event queue (built + tested; driven in 3b)
  msrs.rs      boot-time MSR values
  regs.rs      GDT / segments / page tables / registers / LAPIC LINT setup
  mptable.rs   Intel MP table (CPU discovery without ACPI)
  boot.rs      load vmlinux, cmdline, E820, zero page
  serial.rs    16550 UART wrapper + the temporary stdin input thread
guest/
  kernel/
    microvm-kernel-x86_64-6.1.config   pinned guest kernel config (HPET off,
                                       + Step 2a container/nftables options)
    step2a-container.config            human-readable record of the 2a config delta
    build_kernel.sh                    builds the pinned ELF vmlinux
    vmlinux-6.1.128                    (gitignored) bootstrap/built kernel
  initramfs/                           the minimal busybox guest (Step 1)
    init                 PID 1 setup (boot marker); then execs busybox `init`
    inittab              busybox init table: respawn the serial shell; relay
                         poweroff/reboot (so `exit` gives a fresh prompt)
    busybox              static busybox (the permanent test-guest rootfs)
    gen_init_cpio.c      vendored kernel tool (build initramfs without root)
    build_initramfs.sh   builds initramfs.cpio.gz
    initramfs.cpio.gz    the built initramfs
  bake-stack.sh          Phase-2a pipeline: compose.yml -> per-stack initramfs
    bake_compose.py      compose subset validator + compose.lock.yml emitter
  stacks/
    dogfood/             the Phase-1 Postgres workload re-expressed as compose
                         (compose.yml + service-loop.sh + schema.sql); bake output
                         compose.lock.yml + stack.lock live here after a bake
    rejects/             one compose per rejected feature (the reject-test corpus)
  initramfs-alpine/                    the Alpine container guest (2a base + stacks)
    build_rootfs.sh      build the base 2a guest, or (stack mode) assemble a stack
    prebake_images.sh    bake the busybox self-test image into a base seed (host)
    compose-engine.lock  pinned Docker Compose v2 version + sha256
    images.lock          base-guest pin (busybox) + generated provenance
    packages.lock        exact Alpine package versions installed (generated)
    service/             the 2b service image (Containerfile + loop) -- kept for 2b
    overlay/             files baked into the rootfs: init, self-test,
                         compose-up.sh (stack launcher), podman conf, and
                         etc/inittab (busybox init: respawn shell + relay power)
    initramfs-alpine.cpio.gz          (gitignored) base 2a guest
    initramfs-alpine-<stack>.cpio.gz  (gitignored) per-stack baked guest
  manifest.txt             the artifact manifest: vmlinux + initramfs hashes AND
                           the effective guest clock/timer CPUID profile (so a
                           host/CPU change is a detected deviation, not silent)
scripts/
  smoke_test.sh            boot the busybox guest; require its marker within N s
  smoke_test_container.sh  boot the base 2a guest; require `podman run` to work
  smoke_test_workload.sh   boot the dogfood stack; assert rows grow then cap
                           (real-time / FF-off park path)
  smoke_test_interactive_exit.sh  regression: boot interactively; `exit` respawns
                           the shell (fresh prompt), `reboot` (triple fault) and
                           `poweroff` (IF=0 HLT) self-exit 0, horizon bounds a wedge
  ff_demo.sh               Step 4 fast-forward demo + acceptance-gate assertions
  ff_repeat.sh             repeatability: run ff_demo twice, require identical rows
  bake_reject_test.sh      assert every out-of-subset compose rejects loudly
  bake_repeat_test.sh      assert same compose input -> identical lock + digests
  gen_manifest.sh          write/verify (--check) guest/manifest.txt
run.sh                     build + boot interactively (base Alpine guest, 2 GiB,
                           fast-forward OFF — the human real-time entry point)
```

## Build and run

```sh
# 1. Build the VMM
cargo build --release

# 2. Build a guest kernel — build the pinned one (needs bc, flex, bison, ...):
guest/kernel/build_kernel.sh
#    ...or bootstrap with Firecracker's published microvm kernel (but this does
#    NOT have the Step 2a container options, so containers will not run):
curl -sSL -o guest/kernel/vmlinux-6.1.128 \
  https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/x86_64/vmlinux-6.1.128

# 3. Build the Alpine container guest rootfs (needs host podman + network for the
#    one-time package + image downloads; everything is pinned). This also bakes
#    the Postgres + service images into the seed store:
guest/initramfs-alpine/build_rootfs.sh

# 4a. Boot to a shell (Alpine container guest, 2 GiB, by default; runs the 2a
#     self-test then drops to a serial shell). run.sh boots fast-forward OFF
#     (real time — the human entry point) and prints a one-line mode statement
#     at startup. Leave the guest with `poweroff` or `reboot`; `exit` just gives
#     a fresh prompt. Use `FF=on ./run.sh` to fast-forward instead:
./run.sh

# 4b. Boot the Step 2b Postgres workload (needs ~3 GiB; committed defaults are
#     INTERVAL_SECONDS=3600, MAX_ROWS=1000). With fast-forward ON (the default),
#     the hourly sleeps collapse — the guest runs ~1000x virtual-time. Add
#     `--ff off` for the Step-3b real-time park (interactive console / A/B):
./target/release/dvmm boot \
  --kernel guest/kernel/vmlinux-6.1.128 \
  --initrd guest/initramfs-alpine/initramfs-alpine.cpio.gz \
  --mem 3072 --ff on \
  --cmdline "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.workload=1 dvmm.interval=3600 dvmm.maxrows=1000"

# 4c. The Step 4 fast-forward demo (asserts the acceptance gates: cadence,
#     >=100x speedup, per-hop cost, max jump, timekeeping cleanliness):
scripts/ff_demo.sh 24            # >=24 virtual hours in seconds-to-minutes
scripts/ff_repeat.sh 24          # repeatability: run it twice, identical rows
```

Fast-forward flags: `--ff on|off` toggles the jump. The **binary default is on**
(an explicit, documented choice — it does *not* vary with the environment, so our
own pty-based tests are stable). The human entry point `run.sh` instead passes
`--ff off`, because fast-forward at an interactive console races the guest clock
and pins a host core. The VMM **always prints a one-line mode statement at
startup** (stderr) — the FF state and how it was chosen, e.g.
`[dvmm] fast-forward: OFF (--ff off)` or `ON (default)`; at a tty it appends a
quit hint, and if FF is on it adds an advisory to pass `--ff off` for real time
(that advisory uses the isatty signal for the *warning only*, never for the FF
decision). `--max-jump-secs <n>` (default **300**) is a safety bound — a single
jump larger than this aborts the run and reports it (expected never to trip:
Postgres's sub-second background timers keep the largest observed Δ ~0.1 s).

`--max-virtual-time <dur>` sets a **virtual-time horizon**: when the guest's
virtual clock reaches `<dur>` of elapsed virtual time, the run stops with a
distinct exit status (**3**) and a diagnostic dump (jump count, jump rate, and a
Δvtsc histogram). `<dur>` is a duration — a bare number is seconds, or use a
suffix (`500ms`, `30s`, `5m`, `2h`). It is implemented as a `(vtsc, StopRun)`
entry in the one event queue, so it is deterministic and replayable — not a
wall-clock timeout. A wedged or deeply-idle guest hits any sane horizon in
seconds of real time (fast-forward converts idle into throughput), so this is
what bounds a run whose guest would otherwise fast-forward forever. Every harness
run with a known virtual budget sets it (`ff_demo.sh`, `smoke_test_workload.sh`,
`smoke_test_container.sh`); interactive `run.sh` leaves it unset (unlimited).
Independently, whenever the jump rate stays very high (>10k/s for >5 s) the VMM
emits a single rate-limited `WARN` with the same histogram — the wedge signature
made visible — but that telemetry **never** stops the run.

Leaving the guest — each path stops the VMM cleanly:

- **`exit`** at the shell — the guest init (busybox `init`, per `/etc/inittab`)
  **respawns** the shell, so `exit` just gives a fresh prompt and the VMM keeps
  running. (A bare shell as PID 1 would panic the kernel here.)
- **`reboot`** — with the default `reboot=t` this is a **triple fault** →
  `KVM_EXIT_SHUTDOWN`, and the VMM stops (status 0). Not `reboot=k`: the VMM does
  not emulate an i8042 keyboard controller, so `reboot=k` never completes and the
  guest would fast-forward forever (only `--max-virtual-time` bounds it).
- **`poweroff`** — with no ACPI the kernel finishes in a **HLT with interrupts
  disabled (IF=0)**, "System halted". That halt can never wake (no interrupt is
  deliverable), so the VMM recognizes it as a clean **"guest halted (power off)"**
  terminal stop (status 0) — a distinct `StopReason` alongside guest shutdown and
  the horizon.

`scripts/smoke_test_interactive_exit.sh` is the regression test for all of this:
it boots interactively and asserts `exit` respawns (fresh prompt), `reboot` and
`poweroff` each self-exit 0, `--max-virtual-time` still bounds a reboot=k wedge
(status 3), and the startup mode line is present.

The minimal **busybox** guest is still here for later clock work. Build and
boot it with:

```sh
guest/initramfs/build_initramfs.sh
INITRD=guest/initramfs/initramfs.cpio.gz MEM=256 ./run.sh
```

The kernel command line is
`console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable` (the
container smoke test adds `dvmm.autotest=1` so the guest runs its self-test and
stops on its own). The `no_timer_check tsc=reliable` pair arrived in Step 3a:
`tsc=reliable` trusts the invariant TSC and skips the clocksource watchdog, and
`no_timer_check` skips the boot-time timer-IRQ probe (harmless here, and needed
once the in-kernel PIT is removed in Step 3b). To leave an interactive VMM, run
`poweroff` or `reboot` inside the guest (see "Leaving the guest" above); or
`pkill dvmm` from another terminal (Ctrl-A/Ctrl-C are delivered to the guest).

When the Alpine guest boots it runs a self-test: it creates the `appnet` bridge
network and `podman run`s the baked image, printing over serial:

```
DVMM_ALPINE_BOOT_OK reached userspace
...
DVMM_NET_CREATE_OK
DVMM_CONTAINER_HELLO
DVMM_PODMAN_RUN_OK
DVMM_SELFTEST_PASS
```

## Smoke test

```sh
# Step 2b workload: boot with a small interval + max_rows and assert rows
# accumulate at the interval AND cap at MAX_ROWS (never exceeding). Also reports
# the measured peak guest RAM. Exits 0 only if both properties hold.
scripts/smoke_test_workload.sh 240

# Alpine container guest (Step 2a): boot, create the bridge network, `podman run`
# a digest-pinned image offline. Exits 0 only if all of that works.
scripts/smoke_test_container.sh 120

# Minimal busybox guest: boot; require the "DVMM_BOOT_OK" marker within 30s.
scripts/smoke_test.sh 30 \
  guest/kernel/vmlinux-6.1.128 guest/initramfs/initramfs.cpio.gz
```

All exit 0 on success, non-zero otherwise. A passing 2b run (interval 2 s,
max_rows 5) shows the row count climb then cap, at the interval:

```
DVMM_ROWCOUNT=1 iter=1 max=5 ts=2026-08-01T00:00:01Z
DVMM_ROWCOUNT=2 iter=2 max=5 ts=2026-08-01T00:00:03Z
DVMM_ROWCOUNT=3 iter=3 max=5 ts=2026-08-01T00:00:05Z
DVMM_ROWCOUNT=4 iter=4 max=5 ts=2026-08-01T00:00:07Z
DVMM_ROWCOUNT=5 iter=5 max=5 ts=2026-08-01T00:00:09Z
DVMM_ROWCOUNT=5 iter=6 max=5 ts=2026-08-01T00:00:11Z   <- capped, holds as
DVMM_ROWCOUNT=5 iter=7 max=5 ts=2026-08-01T00:00:13Z      inserts continue
```

## Pinned versions (Steps 2a/2b)

- **Alpine** 3.22.5 minirootfs (sha256 pinned in `build_rootfs.sh`).
- **podman** 5.6.2, **crun** 1.23.1, **conmon** 2.1.13, **netavark** 1.16.1,
  **aardvark-dns** 1.16.0, **nftables** 1.1.3 (full set in `packages.lock`).
- Baked images (`images.lock` + generated provenance):
  - busybox `@sha256:dc2d74b2…` (2a self-test), pulled as-is.
  - **postgres** `postgres:16-alpine @sha256:57c72fd2…` (= 16.14), pulled by
    digest then squashed to one vfs layer (~290 MiB); config-gated.
  - **service** `localhost/dvmm-service:2b`, built single-layer from
    `alpine @sha256:14358309…` + `postgresql16-client=16.14-r0`.
- Built on **host podman 5.8.3**. Repro anchor = the initramfs artifact hash
  (`initramfs-alpine.cpio.gz.sha256`); bit-reproducible rebuilds are out of
  Phase-1 scope (see `images.lock` NOTES).
- **Measured peak guest RAM** for the workload (interval 2 s, max_rows 5):
  **~1.2 GiB used** of a 3 GiB guest (min MemAvailable ~1.8 GiB). The guest is
  run at **3 GiB** for the workload for headroom over vfs per-container copies;
  2a still boots at 2 GiB.
- Guest wall clock: 2026-08-01T00:00:00Z (epoch 1785542400), baked into `init`.

## Step 3b notes

**Determinism ledger.** Guest timer = LAPIC one-shot/periodic via MMIO
(`TMICT`/`TMCCT`/`TDCR`); the count decrements at the **core-crystal frequency
the guest reads from CPUID leaf `0x15`** (38.4 MHz on this host — *not* an
arbitrary declared value), because we pass `0x15`/`0x16` through and the guest
sets `lapic_timer_period = crystal_khz*1000/HZ` from it rather than measuring our
rate. Our timer counts at exactly that crystal frequency as a pure function of
`vtsc`, so ticks land at the correct real time, identically every run. All timer
programming reaches the VMM as MMIO exits; deadlines exist only as `(vtsc,event)`
queue entries. **TSC-deadline is left unadvertised on the userspace backend** due
to a KVM fastpath limitation (below).

**The in-kernel A/B backend was removed in Step 4.** Through Step 3b an
`--irqchip kernel` backend (in-kernel LAPIC/IO-APIC/PIT, advertising TSC-deadline
and running a `lapic-deadline` clockevent) was kept as a sanity reference. It
could not fast-forward — its timer runs on the host clock — so Step 4 retired it.
The **FF on/off flag** is now the A/B: `--ff off` runs the same userspace guest
on the Step-3b real-time park, `--ff on` fast-forwards it.

**KVM finding (Phase-3 ledger item, host kernel unchanged in Phase 1).** Routing
`IA32_TSC_DEADLINE` (MSR `0x6E0`) to the userspace LAPIC via
`KVM_X86_SET_MSR_FILTER` would be the natural design, but on this host (Linux
7.1.4) that never fires: KVM's
WRMSR **fastpath** (`__handle_fastpath_wrmsr`) handles `MSR_IA32_TSC_DEADLINE`
*before* the MSR filter and, unlike the x2APIC-ICR case right beside it, **omits
the `lapic_in_kernel()` check** — so without an in-kernel LAPIC it silently
no-ops the write and re-enters the guest, and the filter/userspace never sees it.
A one-line `lapic_in_kernel()` guard in that fastpath case would fix it; that is
the kind of tiny host-kernel patch budgeted for Phase 3. For now we sidestep it
by using the LAPIC one-shot/periodic timer (MMIO, no fastpath) instead.

**Reproducibility unit.** From Step 3b the anchor pair is **(vmlinux sha256 +
initramfs sha256)** — see `guest/kernel/vmlinux-6.1.128.sha256` and
`guest/initramfs-alpine/initramfs-alpine.cpio.gz.sha256`. The kernel gained one
config line, `CONFIG_X86_MPPARSE=y` (recorded in `guest/kernel/step3b-mpparse.config`).
Step 4 folds both hashes plus the effective guest CPUID into a single manifest
(`guest/manifest.txt`; see "Step 4 notes").

## Step 4 notes

**The jump.** A HLTed guest is waiting for an interrupt; with the userspace
irqchip that means a timer deadline living as a `(vtsc, event)` queue entry.
Instead of sleeping `(deadline − now)` real nanoseconds, the parker adds that
gap to the **TSC offset** and returns immediately, so `vtsc_now()` lands exactly
on the deadline. Then it fires everything now due, lets the guest reprogram its
timer, and loops. Because *every* guest-visible clock (RDTSC, the LAPIC timer,
the PIT counter, `CLOCK_MONOTONIC`, `CLOCK_REALTIME`) is a pure function of that
one offset, moving it moves the whole guest's sense of time atomically.

**The offset is cached + write-through.** `vtsc_now() = host RDTSC + cached
offset`; the cache is a shared cell every `VirtualClock` clone reads, so the hot
loop issues **no ioctl**. A jump writes `KVM_VCPU_TSC_OFFSET` (so the guest's own
RDTSC moves in lockstep) and then the cache, in that order. The offset is written
**only** while parked at a HLT exit, on the vCPU thread, between `KVM_RUN`s —
never concurrent with a running vCPU — and is **monotonically non-decreasing**.
TSC *scaling* (`KVM_SET_TSC_KHZ`) is never touched: the rate stays 1:1 with the
host, only the offset jumps. A post-bump assert checks `vtsc_now() == deadline`
exactly (same host-TSC sample), and the queue never fires an event before its
vtsc.

**No idle-CPU gate here (by design).** Step 3b's "~0% CPU when idle" does not
carry over: fast-forward deliberately converts idle into host CPU *throughput*.
The replacement metric is **virtual-seconds / real-second**; on the 2b workload
(`INTERVAL=3600`) it runs **~950x** — 24 virtual hours in ~90 s of wall clock —
at a per-jump cost of **~0.3 µs mean**. Postgres's sub-second background timers
(bgwriter/walwriter) keep the guest waking often, so the largest single jump is
**~0.1 s** — nowhere near the `--max-jump-secs 300` safety bound (which aborts
the run if some real deadline ever blows past it).

**Bit-exact timer conversion (3b-closure).** counts→TSC-cycles uses the CPUID
`0x15` integers as a pure ratio `count × divisor × EBX/EAX` in 128-bit integer
math — **no floating point anywhere** in the timer/vtsc conversions. On this host
`0x15` is `EAX=2, EBX=160` → **80 TSC cycles per APIC count** (38.4 MHz crystal,
3.072 GHz TSC), independent of the kHz-rounded `KVM_GET_TSC_KHZ`. Determinism
needs the conversion bit-identical every run; this makes it so.

**The manifest (3b-closure).** The declared timer frequency now hangs off the
passed-through `0x15`/`0x16` leaves, so a host/CPU change must surface as a
*detected deviation*, not a silent difference. `dvmm --dump-cpuid` prints the
**effective guest clock/timer CPUID profile** (leaves `0x15`/`0x16`, the leaf-1
policy masks, and invariant-TSC), and `scripts/gen_manifest.sh` records it into
`guest/manifest.txt` alongside the vmlinux + initramfs hashes.
`gen_manifest.sh --check` fails on any deviation. (Per-core-volatile fields — the
leaf-1 initial-APIC-ID byte and the topology x2APIC IDs — are excluded so the
profile is byte-stable run-to-run on one host.)

**Ledger — "don't jump while host I/O is in flight" (untested gap).** A correct
fast-forward must never advance virtual time while a host operation the guest is
waiting on (e.g. a virtio-blk request) is still outstanding, or the guest would
see the completion "in the past". In this closed, tmpfs-only world there is **no
host I/O** (no NIC, no disk — storage is RAM), so the rule has nothing to
violate and is **untested**. It is validated when **virtio-blk lands post-Step 4**
(the device model must fence a jump against in-flight requests). Phase 1 accepts
with this gap explicitly noted.

## Requirements

- Linux with `/dev/kvm` accessible (read+write), x86_64, hardware virt on.
- Rust (built and tested with 1.97).
- To build the Alpine guest / pre-bake images: **host podman** with
  `/etc/subuid` + `/etc/subgid` configured (rootless user namespaces), plus
  host network for the one-time downloads. `build_rootfs.sh` uses its own clean
  `CONTAINERS_CONF`, so a custom host podman runtime config does not interfere.

## Crate versions

`kvm-ioctls 0.25`, `kvm-bindings 0.14` (`fam-wrappers`), `vm-memory 0.18`
(`backend-mmap`, `backend-atomic`), `linux-loader 0.14`, `vmm-sys-util 0.15`,
`vm-superio 0.8`, `libc 0.2`. Per the spec we deliberately do **not** use
`event-manager`; the single stdin input source is hand-rolled.

## Notes / provenance

- The static `busybox` binary is BusyBox 1.35.0 (x86_64, musl static), from
  the upstream prebuilt binaries. It is a permanent in-repo fixture — the
  minimal test guest for later clock work — not a throwaway.
- `guest/initramfs/gen_init_cpio.c` is vendored from the Linux kernel tree
  (`usr/gen_init_cpio.c`, v6.1). It lets us bake device nodes
  (`/dev/console`, ...) into the initramfs without root.
- The arch code (E820, MPTable, zero page, GDT/page tables) is ported from
  Firecracker's `arch/x86_64`; it was studied and adapted, not vendored.
- The Alpine guest is built **without root**: `build_rootfs.sh` runs `apk` in a
  chroot inside `podman unshare` (a user namespace), and the initramfs is packed
  with `gen_init_cpio` (device nodes) + `cpio` (the tree, forced to uid/gid 0).
- The pre-baked image store is made relocatable by stripping podman's `libpod`
  state (which records an absolute path); the guest recreates it fresh at
  `/var/lib/containers/storage`. The digest-pinned image itself lives in the
  path-agnostic containers/storage layers.
- Kernel config delta for containers is recorded in
  `guest/kernel/step2a-container.config`: the base Firecracker microvm config
  already had namespaces, cgroup v2 + MEMCG, veth, bridge, overlayfs, tmpfs
  xattr/ACL and seccomp; Step 2a adds the nftables `inet` family plus the
  masquerade/fib/reject expressions netavark emits.
