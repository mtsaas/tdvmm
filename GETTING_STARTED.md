# Getting started with tdvmm

This guide builds a real service stack, runs it, reads its logs, and writes a
test that drives it through time and injects faults. It assumes a terminal, not
knowledge of tdvmm's internals.

## The idea

tdvmm runs a Docker Compose stack inside one small Linux VM. When the guest goes
idle — a service sleeping until its next timer — tdvmm fast-forwards virtual time
to the next event, so a job that runs "every hour" plays out in seconds. A whole
stack — kernel, container images, and all — bakes into one self-contained file (a
`.tdvmm`) that runs offline.

Two commands do most of the work:

| command       | what it does                          |
|---------------|---------------------------------------|
| `tdvmm build` | bake a `compose.yml` into a `.tdvmm`  |
| `tdvmm run`   | boot a `.tdvmm` and watch it          |

## Requirements

- Linux on x86_64 with `/dev/kvm` to run a `.tdvmm`.
- `podman` to build one (it pulls the images and bakes the guest in pinned
  containers).
- tdvmm itself: `cargo build --release` (or `mise run install`) → the `tdvmm`
  binary.

## 1. Build a stack

Give `tdvmm build` a stack name and a supported `compose.yml`:

```sh
tdvmm build myapp ./compose.yml
# -> ~/.tdvmm/artifacts/myapp.tdvmm
```

The worked examples in this guide (`demo`, `insert-trim`, `faultlab`, …) live
under `testdata/stacks/` in a repo checkout, so clone the repo to follow along.
This guide uses the bundled demo (Postgres + Redis + a small gRPC api/worker/client):

```sh
tdvmm build demo testdata/stacks/demo/compose.yml
# -> ~/.tdvmm/artifacts/demo.tdvmm
```

That resolves each image to a digest, pulls and packs it, bakes a kernel and an
in-RAM root filesystem, and writes one file under `~/.tdvmm/artifacts/`. The same
inputs produce the same bytes. The first build also compiles the guest kernel and
agent from pinned sources inside pinned containers (the kernel takes a few
minutes); both are cached, so later builds skip the compiles. The first argument
is the stack name (`demo`) — the store key for `tdvmm run <name>` and `tdvmm ls`;
it must be a single path component. `-o path.tdvmm` changes only where the file is
written, not the stored name.

List what you have built with `tdvmm ls`:

```sh
tdvmm ls            # name, size, and build time (UTC)
tdvmm ls --digest   # also print each one's sha256 identity (reads the files)
```

If a compose file needs something the closed world cannot do — host networking,
an absolute bind mount, an unpinned `build:` base — the build stops and names the
line.

### Building on macOS

`tdvmm build` runs only on Linux (it bakes the guest inside Linux containers). On
a Mac, `scripts/macos-build.sh` runs the bake inside the Linux VM that `podman
machine` provides and drops the finished `.tdvmm` into your Mac's
`~/.tdvmm/artifacts/`:

```sh
scripts/macos-build.sh testdata/stacks/demo/compose.yml
```

macOS can bake, but only Linux with `/dev/kvm` can run. The build is
byte-reproducible, so a Mac-baked artifact is identical to a Linux-baked one; run
`tdvmm verify` to confirm.

## 2. Run it and watch

```sh
tdvmm run demo --max-virtual-time 1h \
  --cmdline "console=ttyS0 tdvmm.stack=1 tdvmm.interval=180 tdvmm.hc_tick=30"
```

`demo` is the stack name — `tdvmm run`/`inspect`/`verify` resolve a bare name
from `~/.tdvmm/artifacts/`. To run a `.tdvmm` file on disk, give a path with a `/`
(`./demo.tdvmm` or an absolute path).

It boots, brings the stack up, and streams every container's output to the
terminal, prefixed by service name. Idle time fast-forwards, so an hour of the
workload runs in tens of seconds. Two flags you will use often:

- **`--max-virtual-time <dur>`** bounds the run in virtual time (`30s`, `5m`,
  `24h`). Set it — a fast-forwarding idle guest otherwise races to the end of time
  at once.
- **`--ff off`** turns fast-forward off (real time), for an interactive
  poke-around. Leave it on (the default) otherwise.

The `--cmdline` above tunes the demo's cadence — a cycle every 180 virtual
seconds instead of its hourly default — so it iterates several times. Most stacks
do not need it.

The run ends at `--max-virtual-time`. To stop sooner, `pkill tdvmm` from another
terminal (`Ctrl-C` goes to the guest, not to tdvmm).

## 3. See each service's logs

The terminal stream interleaves everything. For a clean per-service log, add
`--logs-dir`:

```sh
tdvmm run demo --max-virtual-time 1h --logs-dir ./logs \
  --cmdline "console=ttyS0 tdvmm.stack=1 tdvmm.interval=180 tdvmm.hc_tick=30"
# -> ./logs/postgres.log, ./logs/redis.log, ./logs/api.log, ./logs/worker.log, ...
```

Each file is one service's output with RFC3339 timestamps and `stdout`/`stderr`
tags.

## 4. Test a stack from inside it

A test is not a separate verb or file. It is one of your own containers — a
driver — that talks to the harness while it drives the workload. The driver does
two things an ordinary container cannot:

- inject faults into its own cluster — partition the network, kill a node, heal
  it;
- end the run with a verdict — `Finish(0, "")` passes, `Finish(1, "why")` fails.

Because the workload and the faults are one program, a fault can land in the
middle of an operation the driver has in flight.

The driver is a normal service. Its image is a small program built with the Go
SDK. Add it to your compose file:

```yaml
  driver:
    build: ./driver               # a multi-stage Go build; see testdata/stacks/pgcluster
    depends_on: [postgres, api]
```

Write the test with the Go SDK (`sdk/go`, package `tdvmm`):

```go
package main

import (
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

func main() {
	h, err := tdvmm.Connect()
	if err != nil {
		return
	}
	defer h.Close()
	h.WaitForServices([]string{"postgres", "api"}, 3*time.Minute)

	// Cut the api off from its database while it is serving.
	h.Partition("api", "postgres")
	if apiStillClaimsHealthy() {
		h.Finish(1, "the api reported healthy with its database unreachable")
		return
	}
	h.Heal()

	h.Finish(0, "")
}
```

Then run it:

```sh
tdvmm run demo --wall-timeout 900 --logs-dir ./logs
```

The exit code is the verdict:

- **0** — the driver called `Finish(0, …)`, or there was no driver and the guest
  stopped on its own.
- **1** — the driver called `Finish` with a nonzero code. Its raw code is in the
  summary line and `--metrics-out`.
- **2** — something broke: a bad artifact, an unreachable agent, or the
  `--wall-timeout` safety net firing because the driver died without finishing.
- **3** — `--max-virtual-time` ran out first.

That split lets CI tell "the code is wrong" from "the tool broke." Set
`--wall-timeout` on any driven run: it is the only thing that ends a test whose
driver crashed.

### Virtual time is sleep

There is no "advance the clock" call. Virtual time moves only while the guest is
idle, and the guest is the authority on its own idleness. So you sleep:

```go
h.WaitForServices([]string{"postgres"}, 3*time.Minute)
time.Sleep(24 * time.Hour)     // a virtual day, ~instantly
h.Kill("postgres")             // ...then the fault
```

Prefer waiting for an observed state over a duration — it keeps a test
reproducible:

```go
h.Kill("postgres")
h.WaitUntil(func() bool { r, _ := h.Running(); return !r["postgres"] },
	time.Minute, time.Second, "postgres to be down")
```

## 5. Design a fault

Every fault is one SDK call, and each returns only once the fault is applied —
the nftables rule installed, or the container stopped. There is no "did it land
yet?" window between a fault and the next request.

```go
h.Kill("postgres"); h.Stop("postgres"); h.Start("postgres")
h.Partition("api", "postgres")
h.Heal("api", "postgres")      // or h.Heal() for every partition at once
```

The useful shape is fault → prove it broke → recover → prove it healed:

```go
h.WaitForServices([]string{"api", "postgres"}, 3*time.Minute)

h.Partition("api", "postgres")
h.WaitUntil(func() bool { return !apiCanReachDB() },
	time.Minute, time.Second, "the api to lose its database")

h.Heal()
h.WaitUntil(apiCanReachDB, time.Minute, time.Second, "the api to recover")

h.Finish(0, "")
```

`testdata/stacks/pgcluster/` is a complete worked example: a Postgres pair with
synchronous replication, whose driver opens a transaction, partitions the two
nodes while the write is in flight, proves the commit blocks, then heals and
proves it completes on both nodes. `sdk/go/README.md` is the full API.

## Looking at an artifact

```sh
tdvmm inspect demo   # what is inside: images, sizes, the manifest
tdvmm verify  demo   # check nothing changed; print its sha256
```

## Good to know

- **The closed world.** tdvmm runs a subset of Compose: `image:` and `build:`
  services, service-name networking, healthchecks and `depends_on`, relative bind
  mounts, and named volumes. Anything needing the outside world is rejected at
  build time.
- **Every run starts fresh.** Writes inside the guest are ephemeral. Each run
  boots from the same baked state.
- **Virtual time is the point.** `at:` times, timeouts, and intervals are all
  virtual. Fast-forward collapses only genuine idle (a sleeping/HLTed guest), so a
  service that busy-loops does not speed up.
- **Opening the network.** By default the guest cannot reach anything outside. If
  a service must call out, add `--allow-egress` to `run`: it opens one proxy the
  guest reaches at its bridge gateway on port 1080, and a container opts in with
  `ALL_PROXY=socks5h://<gateway>:1080` in its own compose. It is never baked into
  an artifact. Trade-offs: the run slows to real speed while a connection is open;
  the guest's wall clock is fake and drifts per jump, so TLS/token flows may fail;
  and that run is no longer exactly repeatable (the report records `egress=on`).

## Where to go next

- `testdata/stacks/` — worked examples: `demo` (the full gRPC stack),
  `insert-trim` (minimal), `faultlab` (faults), `webstack`, `svcchain`,
  `pgcluster` (the driver-controlled test).
- `sdk/go/README.md` — the driver SDK API.
- `CONTRIBUTING.md` — code layout and where to change things.
- `ARCHITECTURE.md` — how tdvmm works inside.
