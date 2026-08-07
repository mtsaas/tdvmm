// tdvmm tigerbeetle driver — the test, running beside the cluster it tests.
//
// This container is an ordinary member of the compose stack. It drives real
// double-entry accounting against the 3-replica TigerBeetle cluster with the
// image's own REPL, and it reaches the tdvmm harness over the control socket, so
// a fault can land while a batch of transfers is in flight.
//
// The test proves the point of a consensus cluster — it survives a minority
// failure and recovers:
//  1. bring the cluster up; open two accounts on a ledger;
//  2. move money A->B in batches and read the balances back — double-entry holds
//     (A.debits_posted == B.credits_posted);
//  3. with a transfer batch IN FLIGHT, KILL one replica (a minority of 3);
//  4. the in-flight batch must still commit, and further batches must keep
//     committing on the surviving quorum — availability is preserved (f=1);
//  5. the killed replica must actually be down (observed via the harness census);
//  6. restart it, and it must rejoin; a final batch commits and the invariant holds.
//
// Steps 4 and 6 are the assertions that matter. Finish(1) at any point fails the run.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

const (
	// The replicas' static addresses on the `tb` network (see compose.yml). The
	// REPL takes IP literals only, in fixed replica order, matching the cluster.
	addresses = "10.240.0.10:3000,10.240.0.11:3000,10.240.0.12:3000"
	victim    = "replica2" // the minority replica we kill and restart
	ledger    = "700"
	perBatch  = 5   // transfers per batch
	amount    = 100 // each transfer moves this much A->B

	// commitWindow bounds how long one batch may take to commit. Generous, and
	// virtual: an idle guest fast-forwards the waits, so it costs ~no real time.
	commitWindow = 90 * time.Second
)

var (
	replicas    = []string{"replica0", "replica1", "replica2"}
	debitsRe    = regexp.MustCompile(`"debits_posted":\s*"(\d+)"`)
	creditsRe   = regexp.MustCompile(`"credits_posted":\s*"(\d+)"`)
	transferSeq = 0 // monotonic transfer-id source (ids must be unique cluster-wide)
)

func log(msg string) { fmt.Printf("[driver] %s\n", msg) }

// repl runs one REPL command against the cluster and returns combined output.
func repl(command string) (string, error) {
	cmd := exec.Command("/tigerbeetle", "repl", "--cluster=0", "--addresses="+addresses, "--command="+command)
	out, err := cmd.CombinedOutput()
	return string(out), err
}

// transferBatch commits one batch of perBatch transfers (A=1 -> B=2). Each id is
// globally unique. It returns an error unless every transfer was created.
func transferBatch() error {
	var parts []string
	for i := 0; i < perBatch; i++ {
		transferSeq++
		parts = append(parts, fmt.Sprintf(
			"id=%d debit_account_id=1 credit_account_id=2 amount=%d code=10 ledger=%s",
			transferSeq, amount, ledger))
	}
	out, err := repl("create_transfers " + strings.Join(parts, ", ") + ";")
	if err != nil {
		return fmt.Errorf("%v: %s", err, strings.TrimSpace(out))
	}
	if !strings.Contains(out, "created") {
		return fmt.Errorf("no 'created' in reply: %s", strings.TrimSpace(out))
	}
	return nil
}

// commitBatch drives one batch, retrying briefly so a primary re-election (after a
// kill) does not flake the assertion. finished within commitWindow == success.
func commitBatch(h *tdvmm.Client, what string) error {
	var last error
	if err := h.WaitUntil(func() bool {
		last = transferBatch()
		return last == nil
	}, commitWindow, 2*time.Second, what); err != nil {
		return fmt.Errorf("%s: %v", err, last)
	}
	return nil
}

// balances reads (A.debits_posted, B.credits_posted) — the double-entry pair.
func balances() (int, int, error) {
	out, err := repl("lookup_accounts id=1, id=2;")
	if err != nil {
		return 0, 0, fmt.Errorf("%v: %s", err, strings.TrimSpace(out))
	}
	dm := debitsRe.FindAllStringSubmatch(out, -1)
	cm := creditsRe.FindAllStringSubmatch(out, -1)
	if len(dm) < 1 || len(cm) < 2 {
		return 0, 0, fmt.Errorf("could not parse balances from: %s", strings.TrimSpace(out))
	}
	a, _ := strconv.Atoi(dm[0][1])  // A (id=1) is the debit side
	b, _ := strconv.Atoi(cm[1][1])  // B (id=2) is the credit side
	return a, b, nil
}

// assertInvariant reads the balances and checks double-entry plus the expected
// running total.
func assertInvariant(h *tdvmm.Client, wantMoved int) error {
	a, b, err := balances()
	if err != nil {
		return err
	}
	if a != b {
		return fmt.Errorf("double-entry broken: A.debits=%d != B.credits=%d", a, b)
	}
	if a != wantMoved {
		return fmt.Errorf("balance %d does not match the %d moved so far", a, wantMoved)
	}
	log(fmt.Sprintf("double-entry holds: A.debits == B.credits == %d", a))
	return nil
}

func run(h *tdvmm.Client) int {
	if ping, _ := h.Ping(); ping != nil && ping.Schema != nil {
		log(fmt.Sprintf("harness ready: schema %d", *ping.Schema))
	}

	// 1. the whole cluster is up.
	if err := h.WaitForServices(replicas, 5*time.Minute); err != nil {
		return fail(h, fmt.Sprintf("cluster never came up: %v", err))
	}
	log("all three replicas have running containers")

	// 2. open the two accounts (retry until the cluster elects a primary).
	if err := h.WaitUntil(func() bool {
		out, _ := repl("create_accounts id=1 code=10 ledger=" + ledger +
			" flags=history, id=2 code=10 ledger=" + ledger + " flags=history;")
		return strings.Contains(out, "created")
	}, 5*time.Minute, 3*time.Second, "the cluster to open the two accounts"); err != nil {
		return fail(h, "accounts never opened: "+err.Error())
	}
	log("opened accounts A=1 B=2 on ledger " + ledger)

	moved := 0
	// baseline: a couple of healthy batches, invariant holds.
	for i := 0; i < 2; i++ {
		if err := commitBatch(h, "a baseline batch to commit"); err != nil {
			return fail(h, "baseline transfers failed: "+err.Error())
		}
		moved += perBatch * amount
	}
	if err := assertInvariant(h, moved); err != nil {
		return fail(h, err.Error())
	}
	log("baseline accounting works")

	// 3. a batch IN FLIGHT when the fault lands: fire it on its own goroutine, then
	//    KILL a minority replica. Kill returns only once the container is dead.
	inflight := make(chan error, 1)
	go func() { inflight <- transferBatch() }()
	if err := h.Kill(victim); err != nil {
		return fail(h, fmt.Sprintf("could not kill %s: %v", victim, err))
	}
	moved += perBatch * amount
	log("KILLED " + victim + " with a transfer batch in flight")

	// 4. the in-flight batch must still commit — the surviving quorum serves it.
	select {
	case err := <-inflight:
		if err != nil {
			// A primary re-election can drop the very first attempt; recommit it.
			log("in-flight batch hit the failover; recommitting on the quorum")
			if e := commitBatch(h, "the in-flight batch to commit on the quorum"); e != nil {
				return fail(h, "cluster lost availability under a minority kill: "+e.Error())
			}
		}
	case <-time.After(commitWindow):
		return fail(h, "the in-flight batch never committed after a minority kill")
	}
	log("the in-flight batch committed despite the killed replica")

	// 5. the fault is real: the victim is down, the quorum is up. And the cluster
	//    keeps committing while degraded.
	if err := h.WaitUntil(func() bool { r, _ := h.Running(); return !r[victim] },
		time.Minute, 2*time.Second, victim+" to be down"); err != nil {
		return fail(h, err.Error())
	}
	if running, _ := h.Running(); !running["replica0"] || !running["replica1"] {
		return fail(h, "the surviving quorum is not both up")
	}
	log(victim + " is down; replica0 and replica1 form the quorum")
	for i := 0; i < 2; i++ {
		if err := commitBatch(h, "a degraded-cluster batch to commit"); err != nil {
			return fail(h, "cluster stopped committing while degraded: "+err.Error())
		}
		moved += perBatch * amount
	}
	if err := assertInvariant(h, moved); err != nil {
		return fail(h, err.Error())
	}
	log("the cluster kept committing on the quorum while a replica was down")

	// 6. heal: restart the replica; it must rejoin, and accounting must continue.
	if err := h.Start(victim); err != nil {
		return fail(h, fmt.Sprintf("could not restart %s: %v", victim, err))
	}
	if err := h.WaitUntil(func() bool { r, _ := h.Running(); return r[victim] },
		3*time.Minute, 2*time.Second, victim+" to restart"); err != nil {
		return fail(h, err.Error())
	}
	log(victim + " restarted and rejoined")

	if err := commitBatch(h, "a post-heal batch to commit"); err != nil {
		return fail(h, "cluster did not recover after the replica rejoined: "+err.Error())
	}
	moved += perBatch * amount
	if err := assertInvariant(h, moved); err != nil {
		return fail(h, err.Error())
	}
	log("the healed cluster is committing again")

	_ = h.Finish(0, "cluster kept committing through a replica kill and recovered after restart")
	return 0
}

// fail ends the run as a failure and returns exit code 1.
func fail(h *tdvmm.Client, reason string) int {
	_ = h.Finish(1, reason)
	return 1
}

func main() {
	h, err := tdvmm.Connect()
	if err != nil {
		fmt.Printf("[driver] cannot reach the harness: %v\n", err)
		os.Exit(1)
	}
	defer h.Close()
	os.Exit(run(h))
}
