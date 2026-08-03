# Getting started with dvmm

This takes you from zero to testing a real service stack: build it, run it, read its
logs, and write a test that drives it through time and injects faults. It assumes a
terminal; it does not assume you know how dvmm works inside.

## The idea, in three sentences

dvmm runs a Docker Compose stack inside one small Linux VM. When the guest goes idle
(a service sleeping until its next timer), dvmm **fast-forwards** virtual time straight
to the next thing that happens — so a job that runs "every hour" plays out in seconds.
A whole stack — kernel, container images, and all — bakes into a single self-contained
file (a `.dvmm`) that runs offline.

Three commands do everything:

| command      | what it does                                              |
|--------------|----------------------------------------------------------|
| `dvmm build` | bake a `compose.yml` into a `.dvmm`                       |
| `dvmm run`   | boot a `.dvmm` and watch it                               |
| `dvmm test`  | drive a `.dvmm` through a scenario, get a pass/fail verdict |

## Requirements

- Linux on x86_64 with `/dev/kvm` — that's all you need to *run* a `.dvmm`.
- `podman` to *build* one (it pulls the images and bakes the guest in pinned containers).
- dvmm itself: `cargo build --release` (or `mise run install`) → the `dvmm` binary.

## 1. Build a stack

Point `dvmm build` at any supported `compose.yml`. We'll use the bundled demo (Postgres
+ Redis + a small gRPC api/worker/client):

```sh
dvmm build guest/stacks/demo/compose.yml
# -> ~/.dvmm/artifacts/demo.dvmm
```

That resolves each image to a digest, pulls and packs it, bakes a kernel and an in-RAM
root filesystem, and writes one file under `~/.dvmm/artifacts/`. Same inputs always
produce the same bytes. (`-o path.dvmm` writes it wherever you want.)

If your compose file needs something the closed world can't do — host networking, an
absolute bind mount, an unpinned `build:` base — the build stops and names the line. You
find out now, not at runtime.

## 2. Run it and watch

```sh
dvmm run ~/.dvmm/artifacts/demo.dvmm --max-virtual-time 1h \
  --cmdline "console=ttyS0 dvmm.stack=1 dvmm.interval=180 dvmm.hc_tick=30"
```

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

The run ends when it reaches `--max-virtual-time`. To stop sooner, `pkill dvmm` from
another terminal (`Ctrl-C` goes to the guest, not to dvmm).

## 3. See each service's logs

The terminal stream interleaves everything. For a clean per-service log, add `--logs-dir`:

```sh
dvmm run ~/.dvmm/artifacts/demo.dvmm --max-virtual-time 1h --logs-dir ./logs \
  --cmdline "console=ttyS0 dvmm.stack=1 dvmm.interval=180 dvmm.hc_tick=30"
# -> ./logs/postgres.log, ./logs/redis.log, ./logs/api.log, ./logs/worker.log, ...
```

Each file is one service's output with RFC3339 timestamps and `stdout`/`stderr` tags.
`--logs-dir` works on `dvmm test` too — it's how you get a post-mortem of a test run.

## 4. Test a service against a scenario

Running is watching. **Testing** is asserting. A *scenario* is a small YAML timeline: at
each virtual time, do something — wait for readiness, run a command and check its output,
or inject a fault. Run one against the demo:

```sh
dvmm test ~/.dvmm/artifacts/demo.dvmm \
  --scenario guest/stacks/demo/demo.yml --logs-dir ./logs
```

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

Save it and `dvmm test <artifact> --scenario my-first-test.yml`.

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
a day. `guest/stacks/faultlab/` has complete kill/recover and partition/heal scenarios
to crib from.

## Looking at an artifact

```sh
dvmm inspect ~/.dvmm/artifacts/demo.dvmm   # what's inside: images, sizes, the manifest
dvmm verify  ~/.dvmm/artifacts/demo.dvmm   # check nothing changed; print its sha256
```

## Good to know

- **The closed world.** dvmm runs a *subset* of Compose — the part that fits one offline
  machine: `image:` and `build:` services, service-name networking, healthchecks and
  `depends_on`, relative bind mounts, and named volumes. Anything needing the outside
  world is rejected at build time.
- **Every run starts fresh.** Writes inside the guest are ephemeral — each run boots from
  the same baked state. That is what makes a run repeatable. (Note: the *build* is
  byte-reproducible; a *run* is not deterministic — see `ARCHITECTURE.md`.)
- **Virtual time is the point.** `at:` times, timeouts, and intervals are all *virtual*.
  Fast-forward only collapses genuine idle (a sleeping/HLTed guest), so a service that
  busy-loops won't speed up — and that shows up as a slow run.

## Where to go next

- `guest/stacks/` — worked examples: `demo` (the full gRPC stack), `insert-trim`
  (minimal), `faultlab` (faults), `webstack`, `svcchain`.
- `CONTRIBUTING.md` — how the code is laid out and where to change things.
- `ARCHITECTURE.md` — how dvmm works inside.
