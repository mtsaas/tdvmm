# tdvmm

tdvmm is a small KVM hypervisor, written in Rust, for testing Docker
Compose–style stacks faster than real time. It runs a whole stack — services,
container images, and a Linux kernel — inside one virtual machine, and
fast-forwards the time the guest spends idle.

`tdvmm build` bakes a compose stack, a kernel, images, and root filesystem into
one self-contained `.tdvmm` file. Pin it, share it, and re-test it offline.

## Features

- **Fast-forward idle time.** When the guest sleeps on a timer, tdvmm jumps its
  clock to the next event. A day of a scheduled job runs in seconds.
- **One file per stack.** A `.tdvmm` holds the kernel, the images, and the root
  filesystem.
- **Fault injection.** Kill a container, or partition the network between two
  services, then assert the stack recovers.
- **A subset of Compose.** `image:` and `build:` services, service-name
  networking, healthchecks and `depends_on`, bind mounts, and named volumes.
  Anything that needs the outside world is rejected at build time.
- **Opt-in network.** `--allow-egress` opens one proxy for a service that must
  call out. It is per-container and never baked into an artifact.

## Example

```sh
# Bake a compose stack into one file (needs podman). You name it:
tdvmm build demo ./demo/compose.yml   # -> ~/.tdvmm/artifacts/demo.tdvmm

# Run it offline; idle time fast-forwards automatically:
tdvmm run demo --max-virtual-time 24h

# A stack with a driver container tests itself from the inside; the driver's
# verdict becomes the exit code (0 = pass, 1 = fail). A test is a run with a driver:
tdvmm run demo --wall-timeout 900
```

## Requirements

- **To run** a `.tdvmm`: Linux on x86_64 with `/dev/kvm` (read + write).
- **To build** one: `podman`, plus network access on the first build to fetch the
  pinned images and sources. The first build compiles the guest kernel (a few
  minutes) and the guest agent from pinned sources inside pinned containers, then
  caches them. Later builds reuse the cache. No compiler or kernel toolchain is
  needed on the host.

Build tdvmm itself with `cargo build --release` (→ `./target/release/tdvmm`).

## Docs

- **[GETTING_STARTED.md](GETTING_STARTED.md)** — build a stack, run it, read its
  logs, and write a test with fault injection.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — how tdvmm works inside.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — code layout and where to change things.
