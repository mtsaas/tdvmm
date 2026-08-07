# tdvmm Python SDK

Drive the tdvmm test harness from inside one of your own containers.

> Prefer Go? The [Go SDK](../go/README.md) speaks the same wire protocol.

## The idea

A tdvmm guest runs your compose stack in a single-vCPU VM whose idle time is
fast-forwarded. One of your containers can reach back into the harness through a
unix socket and **inject faults into its own cluster** — partition the network,
kill a node, heal it — and then **end the run with a verdict**.

That means the workload and the faults are one program, so you can cut the
network *while a request is in flight*:

```python
import tdvmm

h = tdvmm.connect()
fut = pool.submit(db.write, "orders", row)   # in flight to the cluster
h.partition("pg-primary", "pg-standby")      # cut it mid-write
assert "no quorum" in str(fut.exception())
h.heal()
h.finish(0)
```

There is no `tdvmm test` verb and no scenario file. **A test is just a run with
a driver**: `tdvmm run mystack` boots the stack, and if some container calls
`finish()`, that verdict becomes the run's exit code.

## Install

The SDK is a single dependency-free module (Python 3.9+, stdlib only). Copy it
into your driver image and bind-mount your driver script:

```yaml
services:
  driver:
    image: docker.io/library/python@sha256:...
    volumes:
      - ./tdvmm.py:/app/tdvmm.py:ro
      - ./driver.py:/app/driver.py:ro
    entrypoint: ["python3", "/app/driver.py"]
```

Nothing else is needed — the control socket is bind-mounted into every service
automatically at bake time.

## Running it

```
tdvmm build mystack ./compose.yml
tdvmm run mystack --wall-timeout 900
echo $?     # 0 = PASS, 1 = FAIL, 2 = infrastructure, 3 = virtual-time horizon
```

Set `--wall-timeout`: it is the safety net that ends a run whose driver died
without calling `finish()`. Nothing else watches your driver.

## API

```python
h = tdvmm.connect()               # retries while the agent binds the socket

# network faults — both directions, applied before the call returns
h.partition("a", "b")
h.heal("a", "b")                  # or h.heal() for every partition

# container faults — return once the container has really reached its new state
h.kill("db"); h.stop("db"); h.start("db")

# observation
h.containers()                    # census: service, state, exit_code, health
h.running()                       # {"api", "db", ...}
h.exec("db", "pg_isready -q")     # run a command in ANOTHER container
h.logs("db")                      # that container's whole log
h.wait_for_services(["a", "b"])   # block (in virtual time) until they are up
h.wait_until(pred, timeout_s=60)  # generic virtual-time poll

# ending the run — the first call decides the verdict
h.finish(0)                       # PASS
h.finish(1, "quorum was not lost") # FAIL, with a reason in the run summary
h.fail("replica never rejoined")   # shorthand for finish(1, ...)

h.request("some_new_op", container="x")   # escape hatch to the raw protocol
```

Every failure raises `TdvmmError`; a command the agent refused raises
`CommandError`, whose `.code` is the stable prefix (`no_container`, `nft`,
`podman_op`, `unknown_op`, …) so you can branch on the kind.

## Two things worth understanding

**Faults are applied before the call returns.** `partition()` comes back only
after the nftables rule is installed; `kill()` only after the container is
actually dead. So `h.partition("a","b"); fire_request()` is deterministic —
there is no "did it land yet?" window. Order the fault and the workload however
you need; you get program order.

**`time.sleep()` is the virtual-time API.** A sleeping guest is an idle guest,
and the VMM fast-forwards its clock, so `time.sleep(86400)` returns in
microseconds of real time having "taken" a day. That is how you write "run for a
day, then partition" without waiting a day:

```python
h.wait_for_services(["pg-primary", "pg-standby"])
time.sleep(86400)                  # a virtual day, ~instant
h.partition("pg-primary", "pg-standby")
```

Sleep is the whole API. There is no "advance the clock" call, and there could not
be one: virtual time moves only when the guest is idle, and the guest is the
authority on its own idleness.

## What this costs you in determinism

The old declarative scenarios fired faults at exact virtual timestamps. A driver's
sleeps accumulate instead: busy periods run at real rate, so timestamps drift
between runs even though the *sequence* does not. Write drivers that wait for the
effect they caused rather than for a duration:

```python
h.kill("db")
h.wait_until(lambda: "db" not in h.running(), what="db to be down")   # good
time.sleep(5)                                                        # fragile
```

Verdicts stay meaningful because they are a function of causal structure, which
is reproducible; wall-adjacent timing was never reproducible in the first place.

## A complete example

See `testdata/stacks/pgcluster/` in this repo: a two-node Postgres cluster whose
driver fires an insert, partitions the pair mid-write, checks what the replica
saw, heals, and finishes.
