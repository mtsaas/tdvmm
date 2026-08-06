# Contributing to tdvmm

tdvmm is a from-scratch KVM hypervisor (a VMM) in Rust that runs Docker Compose
stacks in a single VM, with fast-forwarded time, for testing service stacks. This
doc is the orientation:
enough of the mental model to work in the code, where things live, and the rules
you must not break. For the full design, see `ARCHITECTURE.md`.

## The mental model

tdvmm boots **one** Linux guest with **one** virtual CPU and a serial console.
Two ideas drive everything:

1. **tdvmm owns the guest's clock.** There is no in-kernel interrupt controller
   and no paravirtual clock. tdvmm provides a userspace LAPIC + IO-APIC and a
   virtual TSC, so every guest-visible clock is a pure function of one number
   (the TSC offset). When the guest halts waiting for a timer, tdvmm can **jump**
   that number straight to the next scheduled event instead of waiting — that's
   fast-forward. All timed events live in one queue as `(virtual-time, event)`
   entries.

2. **The world is closed and pinned.** A stack is baked into a `.tdvmm` file that
   contains everything: kernel, an in-RAM root filesystem, and the container
   images (pinned by digest). At run time the guest has no outside network. The
   same inputs produce a byte-identical `.tdvmm` — the *build* is reproducible.
   (The *run* is a live VM that fast-forwards idle time — see "What tdvmm is" at
   the end.)

Almost every rule below exists to keep those two things true.

## Repo map

```
src/                 the VMM (the `tdvmm` binary)
  main.rs            the vCPU loop + command dispatch
  cli.rs             the command-line surface (clap)
  build.rs           `tdvmm build`: the bake pipeline (large)
  engine.rs          the ONE place we spawn podman (the container-runtime choke point)
  artifact.rs        the `.tdvmm` file format (deterministic tar) + manifest
  cpio.rs            deterministic initramfs packing
  compose.rs         the supported Compose subset (parser + validator + lock emitter)
  scenario/          `tdvmm test`: the scenario timeline + verdict (schema/engine/eval/ledger/log/report)
  vtsc.rs            the virtual clock (the TSC offset; the fast-forward jump)
  park.rs            what happens at an idle HLT: wait real time, or jump
  lapic.rs ioapic.rs pic.rs pit.rs   the userspace interrupt controller + timer
  cpuid.rs           the CPUID filter (hides the host clock so we own time)
  boot.rs regs.rs msrs.rs mptable.rs memory.rs arch.rs   x86 boot + guest memory
  serial.rs control.rs   the console (ttyS0) and the test control channel (ttyS1)
  events.rs          the one virtual-time event queue
  telemetry.rs       fast-forward metrics / histograms
  ui.rs              the `tdvmm build` progress spinner
  exit.rs util.rs    exit codes; small shared helpers

tdvmm-proto/          host <-> guest-agent wire types (shared crate)
tdvmm-agent/          the tiny in-guest agent that `tdvmm test` drives (static musl)
testdata/
  kernel/            the pinned kernel config + kernel.lock
  initramfs-alpine/  the in-RAM guest rootfs (overlay files, image/package pins)
  stacks/            worked example stacks (each a compose.yml + a scenario)
scripts/             the test suite (see "Building and testing")
```

## Where to look

| You want to…                                | Start in |
|---------------------------------------------|----------|
| Change how a stack is baked                 | `build.rs` (and `engine.rs` for any podman call) |
| Support or reject a Compose feature         | `compose.rs` |
| Change the `.tdvmm` format                   | `artifact.rs` (+ `cpio.rs` for the initramfs) |
| Add a scenario step or a fault              | `scenario/` + `tdvmm-agent/` (and `tdvmm-proto/` for the wire type) |
| Touch the clock or fast-forward             | `vtsc.rs`, `park.rs`, `lapic.rs` |
| Change CPU/timer features the guest sees    | `cpuid.rs` (+ `mptable.rs`) |
| Add a CLI flag or command                   | `cli.rs` (+ `main.rs` dispatch) |
| Change build progress output                | `ui.rs` |

## The invariants (don't break these)

These are the rules the whole design rests on. A change that violates one is
wrong even if it compiles and the tests pass.

1. **Single writer.** Guest state (memory, interrupts, device registers) is
   mutated **only on the vCPU thread, at loop boundaries**. Host I/O may happen
   off-thread, but its effects reach guest state only on that thread.

2. **Bit-reproducibility.** The same inputs must produce a byte-identical
   `.tdvmm`. So: artifact bytes come **only** from tdvmm's own normalizing packers
   (`artifact.rs`, `cpio.rs`) — never from a container's export/copy determinism;
   and **nothing host-probed** (tool versions, paths, timestamps) may enter the
   hashed bytes or a cache key. Declared, pinned inputs only. The `cold == warm`
   gate (a fresh bake and a cached bake must be byte-identical) guards this — keep
   it green.

3. **Fewest host assumptions.** Building a stack needs **only podman**; running
   one needs **only `/dev/kvm`**. Every podman call goes through `engine.rs`. tdvmm
   stays a single, pure-Rust static binary — **do not add a dependency that
   compiles C or asm** (it would break the static build). Tools the build needs
   (apk, wget, tar, gzip) run inside pinned containers or in-process, never as
   host prerequisites.

4. **Clean machine output.** stdout, the `tdvmm test` JSONL/JSON reports, and the
   guest's serial stream are consumed by scripts and must stay byte-clean. Human
   chrome (the progress spinner, log lines) goes to **stderr only**, and only at a
   terminal; piped or CI output must not change.

5. **Own the clock.** Nothing may reintroduce a guest-visible time source tdvmm
   doesn't control (a paravirtual clock, an in-kernel timer, TSC-deadline). That
   is why `cpuid.rs` masks what it masks — fast-forward depends on every clock
   being a function of the one TSC offset.

## Building and testing

```sh
cargo build --release
cargo test --release          # unit + protocol golden tests

scripts/test_scenario.sh      # end-to-end: bake insert-trim, boot it, run a scenario
scripts/test.sh --fast        # the fast tier of the suite
```

If your change touches the bake pipeline, prove reproducibility directly: two
`--no-cache` bakes of the same stack must produce an identical sha256.

Comments follow one rule: explain only what the code can't say for itself
(intent, a non-obvious "why", an invariant) — not what it already says, and not
the history of how it got here.

## What tdvmm is

tdvmm is a **time-dilation VMM**: it bakes a Docker Compose–style stack into one
self-contained file and runs it in a single Linux VM, fast-forwarding virtual
time whenever the guest is idle — so a stack that idles through hours of service
time finishes in seconds to minutes of real time.

The discipline the code follows — single vCPU, single writer, closed world,
integer-only time math, pinned inputs — is there to keep the clock jumps
trustworthy and the system debuggable. What works today: fast-forward and
byte-identical **artifacts**. `ARCHITECTURE.md` has the details and the open
items.
