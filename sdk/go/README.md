# tdvmm Go SDK

Drive the tdvmm test harness from inside one of your own containers.

The client speaks the `tdvmm-proto` wire protocol over the control socket that
the in-guest agent serves.

## What it does

A tdvmm guest runs a compose stack in a single-vCPU VM whose idle time is
fast-forwarded. One container in the stack reaches the harness through a unix
socket and can inject faults into its own cluster — partition the network, kill a
node, heal it — and end the run with a verdict.

Workload and faults are one program, so the network can be cut while a request is
in flight:

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

There is no `tdvmm test` verb and no scenario file. A test is a run with a
driver: `tdvmm run mystack` boots the stack, and if a container calls `Finish`,
that verdict becomes the run's exit code.

## Install

The SDK is standard-library only.

```go
import tdvmm "github.com/mtsaas/tdvmm/sdk/go"
```

```
go get github.com/mtsaas/tdvmm/sdk/go
```

The control socket is bind-mounted into every service automatically at bake time.

## Running it

```
tdvmm build mystack ./compose.yml
tdvmm run mystack --wall-timeout 900
echo $?     # 0 = PASS, 1 = FAIL, 2 = infrastructure, 3 = virtual-time horizon
```

Set `--wall-timeout`. It ends a run whose driver died without calling `Finish`.
Nothing else watches the driver.

## API

```go
h, err := tdvmm.Connect()          // retries while the agent binds the socket
// tdvmm.Dial(path) targets a non-default socket; TDVMM_CONTROL_SOCKET overrides
// the default path used by Connect.

// network faults — both directions, applied before the call returns
h.Partition("a", "b")
h.Heal("a", "b")                   // or h.Heal() for every partition

// container faults — return once the container has reached its new state
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

h.Do(&tdvmm.Request{Op: "some_new_op", Container: ptr("x")}) // raw protocol escape hatch
```

Every method returns `error`. A command the agent refused returns a
`*tdvmm.CommandError`, whose `.Code` is the stable prefix (`no_container`, `nft`,
`podman_op`, `unknown_op`, …):

```go
var ce *tdvmm.CommandError
if errors.As(h.Kill("ghost"), &ce) && ce.Code == "no_container" {
	// ...
}
```

## Two things to know

**Faults are applied before the call returns.** `Partition` returns after the
nftables rule is installed; `Kill` after the container is dead. So
`h.Partition("a","b"); fireRequest()` is deterministic. Each fault method blocks
until the agent acknowledges.

**`time.Sleep` is the virtual-time API.** A sleeping guest is idle, and the VMM
fast-forwards its clock, so `time.Sleep(24 * time.Hour)` returns in microseconds
of real time. `WaitUntil` sleeps between probes, so its timeouts are virtual too.
Prefer waiting for an observed effect over a fixed duration:

```go
h.Kill("db")
h.WaitUntil(func() bool { r, _ := h.Running(); return !r["db"] },
	60*time.Second, time.Second, "db to be down")
```

## Concurrency

A `Client` is safe to share across goroutines: each command is one serialized
request/reply round-trip on the single socket. The in-flight work (the `db.Write`
goroutine above) talks to the cluster, not the harness, so it never contends with
the driver's fault calls.

## Example

Kill Postgres and check the committed row survives the restart.

`compose.yml`:

```yaml
services:
  db:
    image: postgres:16
    environment:
      POSTGRES_HOST_AUTH_METHOD: trust
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 5s
      retries: 10

  driver:
    build: { context: ./driver }
    depends_on:
      db: { condition: service_healthy }
```

`driver/driver.go`:

```go
package main

import (
	"strings"
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

func main() {
	h, _ := tdvmm.Connect() // the harness socket, mounted into every service
	defer h.Close()

	h.WaitForServices([]string{"db"}, time.Minute)
	h.ExecShell("db", `psql -U postgres -c "create table t (x int); insert into t values (1)"`)

	h.Kill("db")  // crash Postgres
	h.Start("db") // restart it
	h.WaitForServices([]string{"db"}, time.Minute)

	r, _ := h.ExecShell("db", `psql -U postgres -tAc "select count(*) from t"`)
	if strings.TrimSpace(*r.Stdout) == "1" {
		h.Finish(0, "") // the row survived
	} else {
		h.Fail("row lost across restart")
	}
}
```

Run:

```
tdvmm build pgtest ./compose.yml
tdvmm run pgtest --wall-timeout 300
echo $?     # 0 = pass
```

Build files (`go.mod`, `Containerfile`) mirror `testdata/stacks/pgcluster/driver/`.
