// deterministic-vmm Phase-2b Go A/B service.
//
// FUNCTIONALLY IDENTICAL to the shell insert/trim service
// (guest/stacks/dogfood/service-loop.sh). Same contract, same env knobs, same
// SQL, same schema, same DVMM_* markers. The ONLY thing that differs versus the
// shell service is the language RUNTIME driving the loop: this is a Go binary, so
// the Go runtime (scheduler, sysmon, GC, scavenger) runs its own background
// wakeups underneath the same workload. That is the whole point — this stack is
// the TREATMENT, the shell dogfood is the CONTROL, and the comparison harness
// measures how a chatty runtime behaves under fast-forward.
//
// To keep the runtime the SOLE variable, this program shells out to the exact
// same `pg_isready` / `psql` invocations the shell service uses (byte-identical
// SQL, byte-identical psql flags). So the Postgres-side load is identical between
// the two stacks; only the parent process's idle behavior differs.
//
// GOGC and GOMAXPROCS are LEFT AT DEFAULTS ON PURPOSE (measure defaults, do not
// tune): GOGC=100, and GOMAXPROCS defaults to the single guest vCPU (=1), which
// already quiets the runtime relative to multi-core — so the measured chattiness
// is a FLOOR, not a ceiling.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"time"
)

func getenv(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// psql runs `psql` with the shared flags and the given SQL, inheriting the
// process environment (PGHOST/PGUSER/PGDATABASE/... just like the shell service).
// Output (stdout) is returned trimmed of a single trailing newline; ON_ERROR_STOP
// makes any SQL error a non-zero exit, exactly as the shell path relies on.
func psql(extraFlags []string, sql string) (string, error) {
	args := append([]string{"-v", "ON_ERROR_STOP=1"}, extraFlags...)
	args = append(args, "-c", sql)
	cmd := exec.Command("psql", args...)
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	s := string(out)
	if n := len(s); n > 0 && s[n-1] == '\n' {
		s = s[:n-1]
	}
	return s, err
}

func fail(msg string) {
	fmt.Printf("DVMM_SVC_FAIL %s\n", msg)
	os.Exit(1)
}

func main() {
	// Default the PG* connection env the same way the shell service does, so the
	// spawned psql/pg_isready children see identical settings.
	pghost := getenv("PGHOST", "postgres")
	pgport := getenv("PGPORT", "5432")
	pguser := getenv("PGUSER", "postgres")
	pgdatabase := getenv("PGDATABASE", "appdb")
	os.Setenv("PGHOST", pghost)
	os.Setenv("PGPORT", pgport)
	os.Setenv("PGUSER", pguser)
	os.Setenv("PGDATABASE", pgdatabase)
	if os.Getenv("PGCONNECT_TIMEOUT") == "" {
		os.Setenv("PGCONNECT_TIMEOUT", "5")
	}

	intervalSeconds, err := strconv.Atoi(getenv("INTERVAL_SECONDS", "3600"))
	if err != nil || intervalSeconds < 0 {
		fail("bad INTERVAL_SECONDS")
	}
	maxRows, err := strconv.Atoi(getenv("MAX_ROWS", "1000"))
	if err != nil || maxRows < 1 {
		fail("bad MAX_ROWS")
	}

	fmt.Printf("DVMM_SVC_START host=%s db=%s interval=%ds max_rows=%d\n",
		pghost, pgdatabase, intervalSeconds, maxRows)

	// 1. Wait for Postgres to accept connections (retry loop over pg_isready).
	for {
		c := exec.Command("pg_isready", "-q", "-h", pghost, "-p", pgport, "-U", pguser)
		if c.Run() == nil {
			break
		}
		fmt.Println("DVMM_SVC_WAIT postgres not ready yet")
		time.Sleep(1 * time.Second)
	}
	fmt.Println("DVMM_SVC_PG_READY")

	// 2. Ensure the table exists (idempotent guard; the baked initdb schema also
	//    creates it on first start). Identical DDL to the shell service.
	if _, err := psql([]string{"-q"},
		"CREATE TABLE IF NOT EXISTS events (id bigserial PRIMARY KEY, ts timestamptz NOT NULL DEFAULT now(), value text);"); err != nil {
		fail("create-table")
	}

	// 3. Insert one row, trim to MAX_ROWS newest rows, report the count — forever.
	i := 0
	for {
		i++

		if _, err := psql([]string{"-q"},
			fmt.Sprintf("INSERT INTO events(value) VALUES ('tick-%d');", i)); err != nil {
			fail(fmt.Sprintf("insert iter=%d", i))
		}

		if _, err := psql([]string{"-q"},
			fmt.Sprintf("DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT %d);", maxRows)); err != nil {
			fail(fmt.Sprintf("trim iter=%d", i))
		}

		n, err := psql([]string{"-tAX"}, "SELECT count(*) FROM events;")
		if err != nil || n == "" {
			fail(fmt.Sprintf("count iter=%d", i))
		}

		// Same marker + timestamp format as the shell service. time.Now() reads the
		// guest CLOCK_REALTIME (TSC-derived), so it rides the fast-forward jumps —
		// timestamps land INTERVAL_SECONDS apart in VIRTUAL time.
		fmt.Printf("DVMM_ROWCOUNT=%s iter=%d max=%d ts=%s\n",
			n, i, maxRows, time.Now().UTC().Format("2006-01-02T15:04:05Z"))

		time.Sleep(time.Duration(intervalSeconds) * time.Second)
	}
}
