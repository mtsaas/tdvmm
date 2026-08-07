package tdvmm_test

import (
	"fmt"
	"log"
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

// commitOrders stands in for a real synchronous-replication COMMIT against the
// cluster (e.g. via database/sql). It talks to Postgres, NOT to the harness, and
// blocks until the commit returns. Replace it with your own client call.
func commitOrders() error { return nil }

// Example shows the pattern the whole SDK exists for: cut the network WHILE a
// write is in flight, prove the synchronous commit blocks, heal, and prove the
// same commit then completes — the shape of testdata/stacks/pgcluster's driver,
// kept self-contained here. It connects to a real in-guest harness, so it is a
// compile-checked illustration rather than a unit test.
func Example() {
	h, err := tdvmm.Connect()
	if err != nil {
		log.Fatal(err)
	}
	defer h.Close()

	// 1. Wait (in virtual time) until both nodes have running containers.
	if err := h.WaitForServices([]string{"pg-primary", "pg-standby"}, 5*time.Minute); err != nil {
		h.Fail(fmt.Sprintf("cluster never came up: %v", err))
		return
	}

	// 2. A transaction is open with an uncommitted INSERT; fire the COMMIT on its
	//    own goroutine so it is genuinely IN FLIGHT while we cut the network.
	committed := make(chan error, 1)
	go func() { committed <- commitOrders() }()

	// 3. Cut the cluster in half. Partition returns only once the nftables rule is
	//    installed, so the commit above is guaranteed to meet the partition.
	if err := h.Partition("pg-primary", "pg-standby"); err != nil {
		h.Fail(fmt.Sprintf("could not partition: %v", err))
		return
	}

	// 4. With the synchronous standby unreachable, the commit must NOT return.
	//    The wait is virtual seconds: the guest is idle, so it costs ~no real time.
	select {
	case err := <-committed:
		h.Heal()
		h.Fail(fmt.Sprintf("commit completed while the standby was unreachable: %v", err))
		return
	case <-time.After(30 * time.Second):
		// Still blocked after 30 virtual seconds — correct.
	}

	// 5. Heal, and the same commit must now complete.
	if err := h.Heal(); err != nil {
		h.Fail(fmt.Sprintf("could not heal: %v", err))
		return
	}
	select {
	case err := <-committed:
		if err != nil {
			h.Fail(fmt.Sprintf("commit failed after healing: %v", err))
			return
		}
	case <-time.After(60 * time.Second):
		h.Fail("commit never completed within 60s of healing")
		return
	}

	// 6. Verdict: synchronous commit honored its durability contract.
	h.Finish(0, "synchronous commit blocked under partition and completed after heal")
}
