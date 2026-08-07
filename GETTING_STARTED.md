# Getting started with tdvmm

This takes you from zero to testing a real service stack: build it, run it, read its
logs, and write a test that drives it through time and injects faults. It assumes a
terminal; it does not assume you know how tdvmm works inside.

## The idea, in three sentences

tdvmm runs a Docker Compose stack inside one small Linux VM. When the guest goes idle
(a service sleeping until its next timer), tdvmm **fast-forwards** virtual time straight
to the next thing that happens — so a job that runs "every hour" plays out in seconds.
A whole stack — kernel, container images, and all — bakes into a single self-contained
file (a `.tdvmm`) that runs offline.

Three commands do everything:

| command      | what it does                                              |
|--------------|----------------------------------------------------------|
| `tdvmm build` | bake a `compose.yml` into a `.tdvmm`                       |
| `tdvmm run`   | boot a `.tdvmm` and watch it                               |

## Requirements

- Linux on x86_64 with `/dev/kvm` — that's all you need to *run* a `.tdvmm`.
- `podman` to *build* one (it pulls the images and bakes the guest in pinned containers).
- tdvmm itself: `cargo build --release` (or `mise run install`) → the `tdvmm` binary.

## 1. Build a stack

Give `tdvmm build` a name for the stack and a supported `compose.yml` — your own:

```sh
tdvmm build myapp ./compose.yml
# -> ~/.tdvmm/artifacts/myapp.tdvmm
```

The worked examples in this guide (`demo`, `insert-trim`, `faultlab`, …) live in a repo
checkout under `testdata/stacks/`, so clone the repo to follow along. We'll use the
bundled demo (Postgres + Redis + a small gRPC api/worker/client):

```sh
tdvmm build demo testdata/stacks/demo/compose.yml
# -> ~/.tdvmm/artifacts/demo.tdvmm
```

That resolves each image to a digest, pulls and packs it, bakes a kernel and an in-RAM
root filesystem, and writes one file under `~/.tdvmm/artifacts/`. Same inputs always
produce the same bytes. The very first build also compiles the guest kernel and agent
from pinned sources inside pinned containers (the kernel takes a few minutes — watch
the live build output); both are cached, so every later build skips the compiles. The first argument is the stack name (`demo` here) — the store
key you'll later `tdvmm run <name>` and see in `tdvmm ls`; it must be a single path
component. (`-o path.tdvmm` changes only where the file is written, not the stored name.)

See everything you've built with `tdvmm ls`:

```sh
tdvmm ls            # name, size, and when each artifact was built (times are UTC)
tdvmm ls --digest   # also print each one's sha256 identity (reads the files)
```

If your compose file needs something the closed world can't do — host networking, an
absolute bind mount, an unpinned `build:` base — the build stops and names the line. You
find out now, not at runtime.

### Building on macOS

`tdvmm build` runs only on Linux (it bakes the guest inside Linux containers). But if you
develop on a Mac, `scripts/macos-build.sh` runs the bake for you inside the Linux VM that
`podman machine` already provides, and drops the finished `.tdvmm` straight into your Mac's
`~/.tdvmm/artifacts/`:

```sh
scripts/macos-build.sh testdata/stacks/demo/compose.yml
```

The boundary: **macOS can bake, but only Linux with `/dev/kvm` can run.** Because the
build is byte-reproducible, a Mac-baked artifact is identical to a Linux-baked one — run
`tdvmm verify` to confirm.

## 2. Run it and watch

```sh
tdvmm run demo --max-virtual-time 1h \
  --cmdline "console=ttyS0 tdvmm.stack=1 tdvmm.interval=180 tdvmm.hc_tick=30"
```

`demo` is the stack **name** — `tdvmm run`/`test`/`inspect`/`verify` resolve a bare name
from `~/.tdvmm/artifacts/`. To run a `.tdvmm` file on disk instead, give a path with a `/`
(`./demo.tdvmm` or an absolute path); a bare name is always a store name.

It boots, brings the stack up, and streams every container's output to your terminal,
prefixed by service name. Idle time fast-forwards, so an hour of the workload runs in
tens of seconds. Two flags you'll reach for constantly:

- **`--max-virtual-time <dur>`** bounds the run in *virtual* time (`30s`, `5m`, `24h`).
  Always set it — a fast-forwarding idle guest would otherwise race to the end of time
  in an instant.
- **`--ff off`** turns fast-forward off (real time) — handy for an interactive
  poke-around; leave it on (the default) otherwise.

(The `--cmdline` above just tunes the demo's cadence — a cycle every 180 virtual
seconds instead of its baked hourly default — so you see it iterate several times.
Most stacks don't need it.)

The run ends when it reaches `--max-virtual-time`. To stop sooner, `pkill tdvmm` from
another terminal (`Ctrl-C` goes to the guest, not to tdvmm).

## 3. See each service's logs

The terminal stream interleaves everything. For a clean per-service log, add `--logs-dir`:

```sh
tdvmm run demo --max-virtual-time 1h --logs-dir ./logs \
  --cmdline "console=ttyS0 tdvmm.stack=1 tdvmm.interval=180 tdvmm.hc_tick=30"
# -> ./logs/postgres.log, ./logs/redis.log, ./logs/api.log, ./logs/worker.log, ...
```

Each file is one service's output with RFC3339 timestamps and `stdout`/`stderr` tags.
`--logs-dir` is how you get a post-mortem of a failed test run.

## 4. Test a stack — from inside it

Running is watching. **Testing** is asserting. In tdvmm a test is not a separate
verb or a separate file: it is one of your own containers — a **driver** — that
talks to the harness while it drives the workload.

The driver does two things an ordinary container cannot:

- **inject faults into its own cluster** — partition the network, kill a node,
  heal it;
- **end the run with a verdict** — `finish(0)` passes, `finish(1, "why")` fails.

Because the workload and the faults are the same program, a fault can land in the
middle of an operation the driver has in flight — the thing you actually want to
test about a distributed system, and the thing an external timeline cannot express.

Add a driver service to your compose file:

```yaml
  driver:
    build: ./driver               # any image with Python 3
    depends_on: [postgres, api]
    volumes:
      - ./tdvmm.py:/app/tdvmm.py:ro       # the SDK (sdk/python/tdvmm.py)
      - ./driver.py:/app/driver.py:ro
    command: ["/app/driver.py"]
```

...and write the test:

```python
import tdvmm

h = tdvmm.connect()
h.wait_for_services(["postgres", "api"])

# Cut the api off from its database while it is serving.
h.partition("api", "postgres")
if api_still_claims_healthy():
    h.finish(1, "the api reported healthy with its database unreachable")
h.heal()

h.finish(0)
```

Then just run it:

```sh
tdvmm run demo --wall-timeout 900 --logs-dir ./logs
```

The exit code is the verdict:

- **0** — the driver called `finish(0)`, or there was no driver and the guest
  stopped on its own.
- **1** — the driver called `finish` with a nonzero code (your stack is wrong).
  Its raw code is in the summary line and `--metrics-out`.
- **2** — something broke: a bad artifact, an unreachable agent, or the
  `--wall-timeout` safety net firing because the driver died without finishing.
- **3** — `--max-virtual-time` ran out first.

That split lets CI tell "the code is wrong" from "the tool broke." Set
`--wall-timeout` on any driven run: it is the only thing that ends a test whose
driver crashed.

### Virtual time is `sleep`

There is no "advance the clock" call, and there could not be one — virtual time
moves only while the guest is idle, and the guest is the authority on its own
idleness. So you just sleep:

```python
h.wait_for_services(["postgres"])
time.sleep(86400)              # a virtual day, ~instantly
h.kill("postgres")             # ...then the fault
```

Prefer waiting for an **observed state** over a duration, though — it is what
keeps a test reproducible:

```python
h.kill("postgres")
h.wait_until(lambda: "postgres" not in h.running(), what="postgres to be down")
```

## 5. Design a fault

Every fault is one SDK call, and each returns only once the fault is really
applied — the nftables rule installed, or the container actually stopped. So
there is no "did it land yet?" window between a fault and the request you fire
next.

```python
h.kill("postgres"); h.stop("postgres"); h.start("postgres")
h.partition("api", "postgres")
h.heal("api", "postgres")      # or h.heal() for every partition at once
```

The useful shape is **fault → prove it broke → recover → prove it healed**:

```python
h.wait_for_services(["api", "postgres"])

h.partition("api", "postgres")
h.wait_until(lambda: not api_can_reach_db(), what="the api to lose its database")

h.heal()
h.wait_until(api_can_reach_db, what="the api to recover")

h.finish(0)
```

`testdata/stacks/pgcluster/` is a complete worked example: a Postgres pair with
synchronous replication, whose driver opens a transaction, partitions the two
nodes **while the write is in flight**, proves the commit blocks, then heals and
proves it completes on both nodes. `sdk/python/README.md` is the full API.

## Looking at an artifact

```sh
tdvmm inspect demo   # what's inside: images, sizes, the manifest
tdvmm verify  demo   # check nothing changed; print its sha256
```

## Good to know

- **The closed world.** tdvmm runs a *subset* of Compose — the part that fits one offline
  machine: `image:` and `build:` services, service-name networking, healthchecks and
  `depends_on`, relative bind mounts, and named volumes. Anything needing the outside
  world is rejected at build time.
- **Every run starts fresh.** Writes inside the guest are ephemeral — each run boots from
  the same baked state, so every run starts from an identical point.
- **Virtual time is the point.** `at:` times, timeouts, and intervals are all *virtual*.
  Fast-forward only collapses genuine idle (a sleeping/HLTed guest), so a service that
  busy-loops won't speed up — and that shows up as a slow run.
- **Opening the network.** By default the guest can't reach anything outside — that's
  what makes fast-forward safe. If a service must call out, add `--allow-egress` to
  `run`/`test`: it opens one proxy the guest reaches at its bridge gateway on port 1080,
  and a container opts in with `ALL_PROXY=socks5h://<gateway>:1080` in its own compose.
  It's never baked into an artifact — you pass the flag each time. Trade-offs: the run
  slows to real speed while a connection is open (so prefer short requests and
  `Connection: close`); the guest's wall clock is fake and drifts per jump, so TLS/token
  flows may fail; and that run is no longer exactly repeatable (the report records
  `egress=on`).

## Where to go next

- `testdata/stacks/` — worked examples: `demo` (the full gRPC stack), `insert-trim`
  (minimal), `faultlab` (faults), `webstack`, `svcchain`.
- `CONTRIBUTING.md` — how the code is laid out and where to change things.
- `ARCHITECTURE.md` — how tdvmm works inside.
