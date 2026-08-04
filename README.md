# dvmm

dvmm is a small KVM hypervisor (a VMM) written in Rust. It bakes a Docker
Compose–style service stack into a single self-contained file and runs it in one
Linux virtual machine, with idle time fast-forwarded, for testing service stacks.

Two things make it useful:

- **It fast-forwards idle time.** When the guest goes to sleep waiting for a
  timer, dvmm jumps its clock straight to the next thing that happens. A service
  that inserts a row "every hour" runs its whole day in seconds.
- **A whole stack is one reproducible file.** `dvmm build` bakes your compose
  stack — kernel, container images, and all — into a single, self-contained
  `.dvmm`. The same inputs always produce the **byte-identical** file (same
  sha256), so you can pin, share, and re-test the exact same bits.

The guest is a **closed world**: everything it needs is baked in, and it has no
network access to the outside at run time — so a test isn't at the mercy of what
the network or host happens to have.

**"Reproducible" here means the build, not the run.** Identical inputs produce a
byte-identical `.dvmm` (verified). A *run* is **not** deterministic — it's a real
VM on real hardware, so execution timing varies and two runs of the same artifact
are not byte-identical. Deterministic, replayable execution is the project's
long-term goal, not a property it has today (see `ARCHITECTURE.md`). What the
closed world plus fast-forward give you now is testing against a fixed, pinned
stack with no outside flakiness — not instruction-for-instruction repeatability.

**New here?** `GETTING_STARTED.md` is a practical, step-by-step walkthrough —
build a stack, run it, read its logs, and write tests with fault injection.

## Requirements

- Linux on x86_64 with hardware virtualization and `/dev/kvm` (read + write).
- **To run** a `.dvmm`: nothing else — just `/dev/kvm`.
- **To build** a `.dvmm`: `podman` (plus network access the first time, to
  download the pinned images and kernel). No compiler and no kernel toolchain —
  the build runs those in pinned containers.

## Install

Build from source:

```sh
cargo build --release        # produces ./target/release/dvmm
```

dvmm is a single self-contained binary. (A fully static release build is wired up
in `.github/workflows/release.yml`, triggered by a version tag.)

## How it works (the short version)

- **`dvmm build <compose.yml>`** turns your stack into a `.dvmm`. It resolves
  every image to a digest, pulls and packs it, bakes a Linux kernel and an in-RAM
  Alpine root filesystem (with podman and the real Docker Compose CLI inside), and
  writes one file. Anything the closed world can't support is rejected loudly at
  build time, not at run time.
- **`dvmm run <stack>`** boots that stack in a VM: one virtual CPU, a serial
  console, and a clock and interrupt controller that dvmm owns in userspace —
  which is what lets it fast-forward idle time. Fully offline.
- **`dvmm test <stack> --scenario s.yml`** drives a timeline against the
  stack: wait for a service to be ready, run a command and check its output,
  inject a fault (kill a container, partition the network), and print a pass/fail
  verdict with a clear exit code. This is the point of the tool.

`dvmm ls` lists what you've built; `dvmm inspect` prints what's inside a stack;
`dvmm verify` checks it hasn't changed. A `.dvmm` file's identity is just its
sha256.

## Usage

```sh
# Bake a stack into one file (needs podman):
dvmm build guest/stacks/insert-trim/compose.yml   # -> ~/.dvmm/artifacts/insert-trim.dvmm

# List what you've built:
dvmm ls                # names, sizes, when built
dvmm ls --digest       # also compute each artifact's sha256 identity

# Run it by name (offline, only needs /dev/kvm). Idle time fast-forwards automatically:
dvmm run insert-trim --max-virtual-time 24h

# Test it against a scenario (assertions + fault injection):
dvmm test insert-trim --scenario guest/stacks/insert-trim/insert-trim.yml
#   -> writes ./insert-trim.jsonl and ./insert-trim.report.json in the current directory

# Look at an artifact:
dvmm inspect insert-trim    # its manifest
dvmm verify  insert-trim    # check every piece matches, and print its sha256
```

`run`, `test`, `inspect`, and `verify` take a bare stack **name**, resolved from
your `~/.dvmm/artifacts/` store the way `dvmm build` names it. To point at a `.dvmm`
file on disk instead, give a path (anything with a `/` — `./foo.dvmm` or an absolute
path); a bare name is always a store name, never a file in the current directory.

Fast-forward is on by default; pass `--ff off` for real time (e.g. an interactive
console). `--max-virtual-time <dur>` bounds a run in virtual time (`30s`, `5m`,
`24h`) — important, because a fast-forwarding idle guest would otherwise reach
the end of time in an instant.

`dvmm test` exit codes: **0** all assertions passed, **1** an assertion or
readiness check failed, **2** something broke (bad scenario, boot or agent
failure). That split lets CI tell "your stack is wrong" from "the tool broke."

## Example stacks

`guest/stacks/` has worked examples, each a real `compose.yml` (most with a
scenario for `dvmm test`). Start with **`demo`** — the capability reference.

### `demo` — a real gRPC microservice stack

Five services, closed-world, exercising the whole supported subset at once:

- **postgres** and **redis** — real, digest-pinned, health-checked backends.
- **api** — a real Python **gRPC** server (the `OrderService` in
  `proto/orders.proto`), backed by both stores over their real wire protocols
  (`psycopg2` → Postgres, `redis-py` → Redis).
- **client** — a real Python gRPC client that submits a batch of orders and reads
  the stats back, once per virtual hour.
- **worker** — rolls each hour's orders up into a `summaries` row.

api, client and worker are one small `build:` image with three entrypoints. The
api starts only after Postgres **and** Redis are `service_healthy`.

Fast-forward the workload's hourly batch cycles and watch them stream by in seconds:

```
$ dvmm run demo --ff on --max-virtual-time 1h \
    --cmdline "console=ttyS0 dvmm.stack=1 dvmm.interval=180 dvmm.hc_tick=30"

hour 1: submitted 9 orders via gRPC -> 9 total orders (cache=9)
hour 2: submitted 10 orders via gRPC -> 19 total orders (cache=19)
hour 3: submitted 11 orders via gRPC -> 30 total orders (cache=30)
...
hour 18: submitted 13 orders via gRPC -> 200 total orders (cache=200)
[dvmm] FAST-FORWARD SUMMARY: virtual 3600s in real ~33s = ~104x speedup
```

That's ~19 of the workload's hourly batch cycles — a live Postgres + Redis + gRPC
stack — in about half a minute of wall clock. Each cycle sleeps a virtual
`dvmm.interval` (180s here) that fast-forward collapses; raise it toward `3600`
for genuine hour-apart spacing over a longer run. (`dvmm.hc_tick` sets how often
the health ticker runs — real work under fast-forward.)

`dvmm test demo --scenario guest/stacks/demo/demo.yml --logs-dir /tmp/demo-logs`
runs those cycles, then SIGKILLs Postgres mid-run: the api, client and worker log
retries, recover when it comes back (its data survives — same container), and
Postgres and Redis stay consistent — verdict **PASS**. The `--logs-dir` output
gives you a clean per-service log of the whole run, fault gap and all.

### The others

- **`insert-trim`** — the minimal case: Postgres + a service that inserts a row
  each hour and trims to a cap. The fastest fast-forward acceptance.
- **`webstack`** — web/api + Postgres + Redis behind two health gates.
- **`svcchain`** — a 3-tier `db → backend → frontend` health-gated chain.
- **`configpipeline`** — a worker + sidecar sharing a named volume and an rw bind.
- **`faultlab`** — kill / network-partition fault scenarios.

## What's supported

dvmm runs a **subset** of Compose — the part that fits a closed, single-machine
world: `image:` and `build:` services, service-name networking, healthchecks and
`depends_on`, relative bind mounts, and named volumes. Things that break the
closed world (host networking, absolute host binds, external networks,
always-pull) are rejected at build time with a clear message.

## Learn more

- **`GETTING_STARTED.md`** — a step-by-step walkthrough: build a stack, run it,
  read its logs, and write tests with fault injection.
- **`CONTRIBUTING.md`** — how the code is laid out and where to look to add a
  feature or fix a bug.
- **`ARCHITECTURE.md`** — the full design, from first principles.
