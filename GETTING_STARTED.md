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
| `tdvmm test`  | drive a `.tdvmm` through a scenario, get a pass/fail verdict |

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
`--logs-dir` works on `tdvmm test` too — it's how you get a post-mortem of a test run.

## 4. Test a service against a scenario

Running is watching. **Testing** is asserting. A *scenario* is a small YAML timeline: at
each virtual time, do something — wait for readiness, run a command and check its output,
or inject a fault. Run one against the demo:

```sh
tdvmm test demo \
  --scenario testdata/stacks/demo/demo.yml --logs-dir ./logs
```

Without `--jsonl`/`--report`, the run log and report default to `./demo.jsonl` and
`./demo.report.json` in the current directory (named after the stack).

The exit code is the verdict:

- **0** — every assertion passed.
- **1** — an assertion failed (your stack is wrong).
- **2** — something broke (bad scenario, boot/agent failure).

That split lets CI tell "the code is wrong" from "the tool broke."

### Write your own

The smallest useful scenario waits for a service to be ready, then checks something:

```yaml
name: my-first-test

steps:
  # Wait (in virtual time) until Postgres accepts connections.
  - name: wait-ready
    at: 0s
    wait_for:
      probe:
        exec: { container: postgres, cmd: "pg_isready -U postgres -d appdb" }
      until: exit_zero
      every: 5s
      timeout: 2m

  # Fast-forward an hour, then assert rows have been written.
  - name: rows-exist
    at: 1h
    exec:
      container: postgres
      cmd: "psql -U postgres -d appdb -tAc 'select count(*) from summaries;'"
    expect:
      exit: 0
      output_matches: '^[1-9][0-9]*$'   # at least one row
```

The pieces:

- **`at:`** — when, in *virtual* time, the step runs. `at: 1h` fast-forwards there; a step
  at `at: 24h` costs seconds, not a day.
- **`wait_for`** — poll `probe` every `every:` until it is `until: exit_zero`
  (or `exit_nonzero`), giving up at `timeout:`. This is readiness.
- **`exec` + `expect`** — run a command in a container and require an `exit:` code and/or
  an `output_matches:` regex.

Save it and `tdvmm test <artifact> --scenario my-first-test.yml`.

## 5. Design a fault

Faults are just more scenario steps, scheduled at an `at:` time like everything else:

- **`kill: <svc>`** / **`stop: <svc>`** / **`start: <svc>`** — container lifecycle.
- **`partition: [A, B]`** / **`heal: [A, B]`** — drop, then restore, all traffic between
  two services.

The useful shape is **fault → prove it broke → recover → prove it healed**:

```yaml
name: survives-db-restart

# A SIGKILLed container exits nonzero on purpose. Declare it, so that death
# doesn't count as an unexpected crash in the end-of-run check.
expect_death: [postgres]

steps:
  - name: wait-ready
    at: 0s
    wait_for:
      probe: { exec: { container: postgres, cmd: "pg_isready -U postgres" } }
      until: exit_zero
      every: 5s
      timeout: 2m

  - name: kill-db
    at: 1h
    kill: postgres

  # Prove the outage: the api can no longer reach the db.
  - name: db-is-down
    at: 1h
    wait_for:
      probe: { exec: { container: api, cmd: "pg_isready -h postgres" } }
      until: exit_nonzero
      every: 5s
      timeout: 2m

  - name: restart-db
    at: 2h
    start: postgres

  # Prove recovery: reachable again, and nothing crashed for good.
  - name: db-recovered
    at: 2h
    wait_for:
      probe: { exec: { container: api, cmd: "pg_isready -h postgres" } }
      until: exit_zero
      every: 5s
      timeout: 2m

  - name: all-up
    at: 2.5h
    containers: all_running
```

A network partition is the same idea without killing anything:

```yaml
  - name: cut-the-link
    at: 1h
    partition: [api, postgres]
  # ... assert the api can't reach postgres, then ...
  - name: restore-the-link
    at: 2h
    heal: [api, postgres]
```

Because every fault fires at a scheduled virtual time, `at: 24h` fast-forwards straight
to it — you can test "what happens after a day of running, then a crash" without waiting
a day. `testdata/stacks/faultlab/` has complete kill/recover and partition/heal scenarios
to crib from.

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
