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
- **`dvmm run <stack.dvmm>`** boots that file in a VM: one virtual CPU, a serial
  console, and a clock and interrupt controller that dvmm owns in userspace —
  which is what lets it fast-forward idle time. Fully offline.
- **`dvmm test <stack.dvmm> --scenario s.yml`** drives a timeline against the
  stack: wait for a service to be ready, run a command and check its output,
  inject a fault (kill a container, partition the network), and print a pass/fail
  verdict with a clear exit code. This is the point of the tool.

A `.dvmm` file's identity is just its sha256. `dvmm inspect` prints what's
inside; `dvmm verify` checks it hasn't changed.

## Usage

```sh
# Bake a stack into one file (needs podman):
dvmm build guest/stacks/dogfood/compose.yml     # -> guest/initramfs-alpine/dogfood.dvmm

# Run it (offline, only needs /dev/kvm). Idle time fast-forwards automatically:
dvmm run guest/initramfs-alpine/dogfood.dvmm --max-virtual-time 24h

# Test it against a scenario (assertions + fault injection):
dvmm test guest/initramfs-alpine/dogfood.dvmm \
  --scenario guest/stacks/dogfood/dogfood.yml

# Look at an artifact:
dvmm inspect dogfood.dvmm    # its manifest
dvmm verify  dogfood.dvmm    # check every piece matches, and print its sha256
```

Fast-forward is on by default; pass `--ff off` for real time (e.g. an interactive
console). `--max-virtual-time <dur>` bounds a run in virtual time (`30s`, `5m`,
`24h`) — important, because a fast-forwarding idle guest would otherwise reach
the end of time in an instant.

`dvmm test` exit codes: **0** all assertions passed, **1** an assertion or
readiness check failed, **2** something broke (bad scenario, boot or agent
failure). That split lets CI tell "your stack is wrong" from "the tool broke."

## Example stacks

`guest/stacks/` has worked examples, each a normal `compose.yml` plus a scenario:
`dogfood` (Postgres + a service that inserts a row and trims the table),
`faultlab` (kill / network-partition fault tests), `webstack`, `svcchain`, and
more.

## What's supported

dvmm runs a **subset** of Compose — the part that fits a closed, single-machine
world: `image:` and `build:` services, service-name networking, healthchecks and
`depends_on`, relative bind mounts, and named volumes. Things that break the
closed world (host networking, absolute host binds, external networks,
always-pull) are rejected at build time with a clear message.

## Learn more

- **`CONTRIBUTING.md`** — how the code is laid out and where to look to add a
  feature or fix a bug.
- **`ARCHITECTURE.md`** — the full design, from first principles.
