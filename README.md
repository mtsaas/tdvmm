# tdvmm

tdvmm is a small KVM hypervisor, written in Rust, for **testing Docker
Compose–style stacks faster than real time.** It runs your whole stack —
services, container images, and a Linux kernel — inside one virtual machine, and
fast-forwards the time the guest spends idle.

The whole stack is one file. `tdvmm build` bakes your compose stack, a kernel,
images, and root filesystem into a single self-contained `.tdvmm` you can pin,
share, and re-test offline.

## Why you'd want it

- **Fast-forward idle time.** When the guest sleeps waiting on a timer, tdvmm
  jumps its clock straight to the next thing that happens. You can watch a day of a
  scheduled job play out in seconds.
- **A whole stack is one file.** One `.tdvmm` holds the kernel, the images, and
  the root filesystem.
- **Real fault injection.** Kill a container, or partition the network between
  two services, then assert your stack recovers on a scheduled
  virtual-time timeline.
- **Runs a practical subset of Compose.** `image:` and `build:` services,
  service-name networking, healthchecks and `depends_on`, bind mounts, and named
  volumes. Anything else that needs the outside world is rejected at build time.
- **Open the network when you must.** `--allow-egress` opens one proxy door for a
  service that genuinely needs to call out — opt-in per container, and never
  baked into an artifact.

## A taste

```sh
# Bake a compose stack into one file (needs podman). You name it:
tdvmm build demo ./demo/compose.yml   # -> ~/.tdvmm/artifacts/demo.tdvmm

# Run it offline; idle time fast-forwards automatically:
tdvmm run demo --max-virtual-time 24h

# Drive it through a scenario and get a pass/fail verdict:
tdvmm test demo --scenario ./demo/scenario.yml
```

## Requirements

- **To run** a `.tdvmm`: Linux on x86_64 with `/dev/kvm` (read + write). Nothing
  else.
- **To build** one: `podman` (plus network access the first time, to fetch the
  pinned images and sources). Nothing precompiled is downloaded: the first build
  compiles the guest kernel (a few minutes) and the guest agent from pinned
  sources inside pinned containers, then caches them — later builds reuse the
  cache. No compiler and no kernel toolchain needed on your machine.

Build tdvmm itself from source with `cargo build --release`
(→ `./target/release/tdvmm`), a single self-contained binary.

## Docs

- **[GETTING_STARTED.md](GETTING_STARTED.md)** — start here. Build a stack, run
  it, read its logs, and write your own tests with fault injection against your
  own compose file.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — how tdvmm works inside, from first
  principles.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — how the code is laid out and where to
  change things.
