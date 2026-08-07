// tdvmm pgcluster driver — the test, running inside the cluster it tests.
//
// This container is an ordinary member of the compose stack. It talks to
// Postgres with psql and to the tdvmm harness over the control socket, so a
// fault can land in the middle of an operation this program has in flight.
//
// The test:
//  1. bring the cluster up and confirm the standby is streaming;
//  2. turn on synchronous replication, so a COMMIT needs the standby;
//  3. open a transaction and INSERT a row, but do not commit;
//  4. partition the primary from the standby while that transaction is open;
//  5. COMMIT — it must block, because the synchronous standby is unreachable;
//  6. heal — the same commit must complete, and the row must appear on both nodes.
//
// Step 5 is the assertion that matters. Finish(1) at any point fails the run.
package main

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

const (
	primary = "pg-primary"
	standby = "pg-standby"

	// blockedProof is how long the partitioned commit must stay blocked. Virtual
	// seconds: the guest is idle while we wait, so fast-forward makes it ~free.
	blockedProof = 30 * time.Second
	// healComplete is how long the healed commit is allowed to finish. Also virtual.
	healComplete = 60 * time.Second
)

func log(msg string) { fmt.Printf("[driver] %s\n", msg) }

// psql runs one SQL statement against host and returns trimmed stdout. -tA gives
// a bare scalar for a single-value SELECT.
func psql(host, sql string) (string, error) {
	cmd := exec.Command("psql", "-h", host, "-U", "postgres", "-d", "appdb",
		"-X", "-q", "-t", "-A", "-w", "-v", "ON_ERROR_STOP=1", "-c", sql)
	cmd.Env = append(os.Environ(), "PGCONNECT_TIMEOUT=10")
	var stderr strings.Builder
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("psql %s: %v: %s", host, err, strings.TrimSpace(stderr.String()))
	}
	return strings.TrimSpace(string(out)), nil
}

// scalarInt runs sql and parses its scalar result as an int.
func scalarInt(host, sql string) (int, error) {
	s, err := psql(host, sql)
	if err != nil {
		return 0, err
	}
	return strconv.Atoi(strings.TrimSpace(s))
}

// writer is a long-lived psql session that holds a transaction open across the
// partition. Its COMMIT is fired by writing to stdin; completion is detected by
// the process exiting (stdin is closed after COMMIT, so psql exits once it
// returns). done carries the exit result.
type writer struct {
	cmd    *exec.Cmd
	stdin  io.WriteCloser
	done   chan error
	stderr *strings.Builder
}

// startWriter opens the session and runs BEGIN + INSERT (uncommitted).
func startWriter(host string) (*writer, error) {
	cmd := exec.Command("psql", "-h", host, "-U", "postgres", "-d", "appdb",
		"-X", "-q", "-w", "-v", "ON_ERROR_STOP=1")
	cmd.Env = append(os.Environ(), "PGCONNECT_TIMEOUT=10")
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	var stderr strings.Builder
	cmd.Stderr = &stderr
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	w := &writer{cmd: cmd, stdin: stdin, done: make(chan error, 1), stderr: &stderr}
	go func() { w.done <- w.cmd.Wait() }()
	if _, err := io.WriteString(stdin,
		"BEGIN;\nINSERT INTO orders (item) VALUES ('widget-during-partition');\n"); err != nil {
		return nil, err
	}
	return w, nil
}

// commit sends COMMIT and closes stdin, so psql exits once COMMIT returns.
func (w *writer) commit() error {
	if _, err := io.WriteString(w.stdin, "COMMIT;\n"); err != nil {
		return err
	}
	return w.stdin.Close()
}

func (w *writer) err() string { return strings.TrimSpace(w.stderr.String()) }

func run(h *tdvmm.Client) int {
	ping, _ := h.Ping()
	agent, schema := "", uint32(0)
	if ping != nil {
		if ping.Agent != nil {
			agent = *ping.Agent
		}
		if ping.Schema != nil {
			schema = *ping.Schema
		}
	}
	log(fmt.Sprintf("harness ready: %s schema %d", agent, schema))

	// 1. the cluster is up.
	if err := h.WaitForServices([]string{primary, standby}, 5*time.Minute); err != nil {
		return fail(h, fmt.Sprintf("cluster never came up: %v", err))
	}
	log("both nodes have running containers")

	if err := h.WaitUntil(func() bool { _, e := psql(primary, "SELECT 1"); return e == nil },
		5*time.Minute, 2*time.Second, "the primary to accept connections"); err != nil {
		return fail(h, err.Error())
	}
	log("primary accepting connections")

	if _, err := psql(primary,
		"CREATE TABLE IF NOT EXISTS orders (id serial PRIMARY KEY, item text NOT NULL, ts timestamptz DEFAULT now())"); err != nil {
		return fail(h, fmt.Sprintf("creating the schema: %v", err))
	}
	log("schema ready")

	// The standby must be a live REPLICA. pg_stat_replication also lists
	// pg_basebackup's own connection while the clone runs, so match the
	// walreceiver by application_name rather than counting rows.
	if err := h.WaitUntil(func() bool {
		n, e := scalarInt(primary, "SELECT count(*) FROM pg_stat_replication "+
			"WHERE state = 'streaming' AND application_name = 'walreceiver'")
		return e == nil && n == 1
	}, 5*time.Minute, 2*time.Second, "the standby's walreceiver to start streaming"); err != nil {
		return fail(h, err.Error())
	}
	if rec, _ := psql(standby, "SELECT pg_is_in_recovery()"); rec != "t" {
		return fail(h, "the standby is not in recovery — it is not a replica of the primary")
	}
	log("standby is streaming and in recovery")

	// 2. synchronous replication on: a COMMIT is now incomplete until the standby
	//    confirms it.
	if _, err := psql(primary, "ALTER SYSTEM SET synchronous_standby_names = '*'"); err != nil {
		return fail(h, fmt.Sprintf("enabling synchronous replication: %v", err))
	}
	if _, err := psql(primary, "SELECT pg_reload_conf()"); err != nil {
		return fail(h, fmt.Sprintf("reloading config: %v", err))
	}
	if err := h.WaitUntil(func() bool {
		s, e := psql(primary, "SELECT sync_state FROM pg_stat_replication LIMIT 1")
		return e == nil && s == "sync"
	}, time.Minute, time.Second, "the standby to become the synchronous replica"); err != nil {
		return fail(h, err.Error())
	}
	log("synchronous replication is ON (commits now require the standby)")

	baseline, err := scalarInt(primary, "SELECT count(*) FROM orders")
	if err != nil {
		return fail(h, fmt.Sprintf("reading the baseline count: %v", err))
	}

	// 3. a write, in flight: open a transaction and INSERT, then wait until the
	//    writer backend is idle-in-transaction, so the row is on the primary and
	//    nowhere else.
	w, err := startWriter(primary)
	if err != nil {
		return fail(h, fmt.Sprintf("opening the writer transaction: %v", err))
	}
	if err := h.WaitUntil(func() bool {
		n, e := scalarInt(primary,
			"SELECT count(*) FROM pg_stat_activity WHERE state = 'idle in transaction'")
		return e == nil && n >= 1
	}, time.Minute, time.Second, "the writer transaction to open"); err != nil {
		return fail(h, "the writer transaction never opened: "+err.Error())
	}
	log("transaction open on the primary with an uncommitted INSERT")

	// 4. cut the cluster in half, mid-transaction. Partition returns only once the
	//    rule is installed, so the commit fired next meets a partitioned network.
	if err := h.Partition(primary, standby); err != nil {
		return fail(h, fmt.Sprintf("could not partition: %v", err))
	}
	log(fmt.Sprintf("PARTITIONED %s <-x-> %s with the write still in flight", primary, standby))

	// 5. the commit must not complete.
	if err := w.commit(); err != nil {
		return fail(h, fmt.Sprintf("issuing the commit: %v", err))
	}
	select {
	case cerr := <-w.done:
		_ = h.Heal()
		why := "it succeeded"
		if cerr != nil {
			why = cerr.Error()
		}
		return fail(h, fmt.Sprintf(
			"the commit completed while the synchronous standby was unreachable (%s)", why))
	case <-time.After(blockedProof):
		// Still blocked — correct.
	}
	log(fmt.Sprintf("commit is still blocked after %s of virtual time — correct", blockedProof))

	// The primary must still serve reads, and the in-flight row must be invisible
	// to other sessions.
	if n, _ := scalarInt(primary, "SELECT count(*) FROM orders"); n != baseline {
		_ = h.Heal()
		return fail(h, "the uncommitted row was visible to another session")
	}
	log("the in-flight row is correctly invisible to other sessions")

	// 6. heal, and the same commit completes.
	if err := h.Heal(); err != nil {
		return fail(h, fmt.Sprintf("could not heal: %v", err))
	}
	log("HEALED; the commit should now be able to finish")

	select {
	case cerr := <-w.done:
		if cerr != nil {
			return fail(h, fmt.Sprintf("the commit failed after healing: %v (%s)", cerr, w.err()))
		}
	case <-time.After(healComplete):
		return fail(h, fmt.Sprintf("the commit never completed within %s of healing", healComplete))
	}
	log("the commit completed after healing")

	// The row must be on BOTH nodes.
	if n, _ := scalarInt(primary, "SELECT count(*) FROM orders"); n != baseline+1 {
		return fail(h, "the committed row is missing from the primary")
	}
	if err := h.WaitUntil(func() bool {
		n, e := scalarInt(standby, "SELECT count(*) FROM orders")
		return e == nil && n == baseline+1
	}, time.Minute, time.Second, "the healed standby to carry the committed row"); err != nil {
		return fail(h, "the standby never received the committed row: "+err.Error())
	}
	log("the row is present on BOTH nodes")

	_ = h.Finish(0, "synchronous commit blocked under partition and completed after heal")
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
