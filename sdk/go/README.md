# tdvmm Go SDK

Drive the tdvmm test harness from inside one of your own containers.

This is the Go sibling of the [Python SDK](../python/README.md); both speak the
exact same wire protocol (`tdvmm-proto`) over the same control socket, so a Go
driver has precisely the fault vocabulary a Python one does.

## The idea

A tdvmm guest runs your compose stack in a single-vCPU VM whose idle time is
fast-forwarded. One of your containers can reach back into the harness through a
unix socket and **inject faults into its own cluster** — partition the network,
kill a node, heal it — and then **end the run with a verdict**.

That means the workload and the faults are one program, so you can cut the
network *while a request is in flight*:

```go
import tdvmm "github.com/mtsaas/tdvmm/sdk/go"

h, err := tdvmm.Connect()
if err != nil {
	log.Fatal(err)
}
defer h.Close()

committed := make(chan error, 1)
go func() { committed <- db.Write("orders", row) }() // in flight to the cluster
h.Partition("pg-primary", "pg-standby")               // cut it mid-write
// ... assert the commit stays blocked ...
h.Heal()
h.Finish(0, "")
```

There is no `tdvmm test` verb and no scenario file. **A test is just a run with
a driver**: `tdvmm run mystack` boots the stack, and if some container calls
`Finish`, that verdict becomes the run's exit code.

## Install

The SDK is a dependency-free module (standard library only). Add it to your
driver image's build:

```go
import tdvmm "github.com/mtsaas/tdvmm/sdk/go"
```

```
go get github.com/mtsaas/tdvmm/sdk/go
```

The control socket is bind-mounted into every service automatically at bake time,
so there is nothing else to wire up.

## Running it

```
tdvmm build mystack ./compose.yml
tdvmm run mystack --wall-timeout 900
echo $?     # 0 = PASS, 1 = FAIL, 2 = infrastructure, 3 = virtual-time horizon
```

Set `--wall-timeout`: it is the safety net that ends a run whose driver died
without calling `Finish`. Nothing else watches your driver.

## API

```go
h, err := tdvmm.Connect()          // retries while the agent binds the socket
// tdvmm.Dial(path) targets a non-default socket; TDVMM_CONTROL_SOCKET overrides
// the default path used by Connect.

// network faults — both directions, applied before the call returns
h.Partition("a", "b")
h.Heal("a", "b")                   // or h.Heal() for every partition

// container faults — return once the container has really reached its new state
h.Kill("db"); h.Stop("db"); h.Start("db")

// observation
h.Containers()                     // census: service, state, exit_code, health
h.Running()                        // map[string]bool of services that are up
h.Exec("db", "pg_isready", "-q")   // run a command in ANOTHER container (argv)
h.ExecShell("db", "pg_isready -q") // ... or a shell script via sh -c
h.Logs("db")                       // that container's whole log
h.WaitForServices([]string{"a", "b"}, 3*time.Minute) // block (virtual time) until up
h.WaitUntil(pred, 60*time.Second, time.Second, "…")  // generic virtual-time poll

// ending the run — the first call decides the verdict
h.Finish(0, "")                    // PASS
h.Finish(1, "quorum was not lost") // FAIL, with a reason in the run summary
h.Fail("replica never rejoined")   // shorthand for Finish(1, …)

h.Do(&tdvmm.Request{Op: "some_new_op", Container: ptr("x")}) // escape hatch to the raw protocol
```

Every method returns `error`. A command the agent refused returns a
`*tdvmm.CommandError`, whose `.Code` is the stable prefix (`no_container`, `nft`,
`podman_op`, `unknown_op`, …) so you can branch on the kind:

```go
var ce *tdvmm.CommandError
if errors.As(h.Kill("ghost"), &ce) && ce.Code == "no_container" {
	// ...
}
```

## Two things worth understanding

**Faults are applied before the call returns.** `Partition` comes back only
after the nftables rule is installed; `Kill` only after the container is actually
dead. So `h.Partition("a","b"); fireRequest()` is deterministic — there is no
"did it land yet?" window. Order the fault and the workload however you need; you
get program order. Each fault method **blocks** until the agent acknowledges.

**`time.Sleep` is the virtual-time API.** A sleeping guest is an idle guest, and
the VMM fast-forwards its clock, so `time.Sleep(24 * time.Hour)` returns in
microseconds of real time having "taken" a day. That is how you write "run for a
day, then partition" without waiting a day:

```go
h.WaitForServices([]string{"pg-primary", "pg-standby"}, 3*time.Minute)
time.Sleep(24 * time.Hour)         // a virtual day, ~instant
h.Partition("pg-primary", "pg-standby")
```

`WaitUntil` sleeps between probes, so its timeouts are virtual too: a 60-second
poll costs no real time if nothing else is running. Write drivers that wait for
the effect they caused rather than for a duration:

```go
h.Kill("db")
h.WaitUntil(func() bool { r, _ := h.Running(); return !r["db"] },
	60*time.Second, time.Second, "db to be down") // good
time.Sleep(5 * time.Second)                        // fragile
```

## Concurrency

A `Client` is safe to share across goroutines: each command is one serialized
request/reply round-trip on the single socket. In the pattern above the in-flight
work (the `db.Write` goroutine) talks to your cluster, not to the harness, so it
never contends with the driver's fault calls.

## A complete example

See `example_test.go` in this directory for the partition-during-in-flight-write
pattern end to end, and `testdata/stacks/pgcluster/` in this repo for the same
experiment as a real (Python) driver against a two-node Postgres cluster.
