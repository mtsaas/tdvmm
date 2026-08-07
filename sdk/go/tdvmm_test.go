package tdvmm_test

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"net"
	"path/filepath"
	"testing"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

// mockAgent is a stand-in for the in-guest tdvmm-agent: it listens on a temp unix
// socket, reads request lines, records each raw line for wire assertions, and
// answers with whatever the handler returns. It serves exactly one connection —
// the single connection a Client holds open.
type mockAgent struct {
	ln       net.Listener
	path     string
	handler  func(req map[string]any) map[string]any
	received [][]byte
	recvCh   chan struct{}
}

func newMockAgent(t *testing.T, handler func(map[string]any) map[string]any) *mockAgent {
	t.Helper()
	path := filepath.Join(t.TempDir(), "sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen on %s: %v", path, err)
	}
	m := &mockAgent{ln: ln, path: path, handler: handler}
	go m.serve()
	t.Cleanup(func() { ln.Close() })
	return m
}

func (m *mockAgent) serve() {
	conn, err := m.ln.Accept()
	if err != nil {
		return
	}
	defer conn.Close()
	r := bufio.NewReader(conn)
	for {
		line, err := r.ReadBytes('\n')
		if trimmed := bytes.TrimSpace(line); len(trimmed) > 0 {
			// Record the exact request bytes BEFORE replying, so a caller that
			// has its reply in hand can read them without a race.
			m.received = append(m.received, append([]byte(nil), trimmed...))
			var req map[string]any
			if json.Unmarshal(trimmed, &req) == nil {
				out, _ := json.Marshal(m.handler(req))
				conn.Write(append(out, '\n'))
			}
		}
		if err != nil {
			return
		}
	}
}

// okStdout echoes a successful reply for the request's id/op.
func okStdout(stdout string) func(map[string]any) map[string]any {
	return func(req map[string]any) map[string]any {
		return map[string]any{"id": req["id"], "ok": true, "op": req["op"], "stdout": stdout}
	}
}

func TestPartitionRoundTrip(t *testing.T) {
	m := newMockAgent(t, okStdout("partition pg-primary <-x-> pg-standby"))
	c, err := tdvmm.Dial(m.path)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer c.Close()

	if err := c.Partition("pg-primary", "pg-standby"); err != nil {
		t.Fatalf("partition: %v", err)
	}

	want := `{"id":1,"op":"partition","container":"pg-primary","peer":"pg-standby"}`
	if got := string(m.received[len(m.received)-1]); got != want {
		t.Fatalf("wire request mismatch\n got: %s\nwant: %s", got, want)
	}
}

func TestNotOKBecomesCommandError(t *testing.T) {
	m := newMockAgent(t, func(req map[string]any) map[string]any {
		return map[string]any{
			"id": req["id"], "ok": false, "op": req["op"],
			"error": "no_container: no running container for service ghost",
		}
	})
	c, err := tdvmm.Dial(m.path)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer c.Close()

	err = c.Kill("ghost")
	if err == nil {
		t.Fatal("expected an error from an ok=false reply")
	}
	var ce *tdvmm.CommandError
	if !errors.As(err, &ce) {
		t.Fatalf("want *tdvmm.CommandError, got %T: %v", err, err)
	}
	if ce.Op != "kill" {
		t.Errorf("Op = %q, want kill", ce.Op)
	}
	if ce.Code != "no_container" {
		t.Errorf("Code = %q, want no_container", ce.Code)
	}
	if ce.Reason != "no_container: no running container for service ghost" {
		t.Errorf("Reason = %q", ce.Reason)
	}
}

func TestNotOKWithNoErrorString(t *testing.T) {
	// ok=false with no error field still becomes a CommandError with a default reason.
	m := newMockAgent(t, func(req map[string]any) map[string]any {
		return map[string]any{"id": req["id"], "ok": false, "op": req["op"]}
	})
	c, _ := tdvmm.Dial(m.path)
	defer c.Close()

	var ce *tdvmm.CommandError
	if err := c.Stop("db"); !errors.As(err, &ce) {
		t.Fatalf("want *tdvmm.CommandError, got %T: %v", err, err)
	} else if ce.Reason != "the agent reported no reason" {
		t.Errorf("Reason = %q, want the default", ce.Reason)
	}
}

func TestFinish(t *testing.T) {
	m := newMockAgent(t, okStdout("finish 0"))
	c, err := tdvmm.Dial(m.path)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer c.Close()

	if err := c.Finish(0, ""); err != nil {
		t.Fatalf("finish: %v", err)
	}
	// exit must be present even when 0 (a real result, not a default); an empty
	// message is omitted.
	want := `{"id":1,"op":"finish","exit":0}`
	if got := string(m.received[len(m.received)-1]); got != want {
		t.Fatalf("finish wire mismatch\n got: %s\nwant: %s", got, want)
	}
}

func TestFinishWithMessage(t *testing.T) {
	m := newMockAgent(t, okStdout("finish 1"))
	c, _ := tdvmm.Dial(m.path)
	defer c.Close()

	if err := c.Fail("quorum was not lost"); err != nil {
		t.Fatalf("fail: %v", err)
	}
	want := `{"id":1,"op":"finish","exit":1,"message":"quorum was not lost"}`
	if got := string(m.received[len(m.received)-1]); got != want {
		t.Fatalf("fail wire mismatch\n got: %s\nwant: %s", got, want)
	}
}

func TestSecondFinishRefused(t *testing.T) {
	// The agent's "first finish wins": a second finish is refused with ok=false.
	first := true
	m := newMockAgent(t, func(req map[string]any) map[string]any {
		if req["op"] == "finish" && !first {
			return map[string]any{"id": req["id"], "ok": false, "op": "finish",
				"error": "finish: run already finished with exit 0"}
		}
		first = false
		return map[string]any{"id": req["id"], "ok": true, "op": req["op"], "stdout": "finish 0"}
	})
	c, _ := tdvmm.Dial(m.path)
	defer c.Close()

	if err := c.Finish(0, ""); err != nil {
		t.Fatalf("first finish: %v", err)
	}
	if err := c.Finish(0, ""); err == nil {
		t.Fatal("second finish should be refused")
	}
}

func TestHealAllVsPair(t *testing.T) {
	m := newMockAgent(t, okStdout("healed"))
	c, err := tdvmm.Dial(m.path)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer c.Close()

	if err := c.Heal(); err != nil { // heal-all
		t.Fatalf("heal-all: %v", err)
	}
	if err := c.Heal("pg-primary", "pg-standby"); err != nil { // heal-pair
		t.Fatalf("heal-pair: %v", err)
	}
	// A wrong argument count is rejected locally and must never hit the wire.
	if err := c.Heal("solo"); err == nil {
		t.Fatal("heal with one service should error")
	}

	if len(m.received) != 2 {
		t.Fatalf("expected exactly 2 requests on the wire, got %d: %q", len(m.received), m.received)
	}
	if got, want := string(m.received[0]), `{"id":1,"op":"heal"}`; got != want {
		t.Errorf("heal-all wire\n got: %s\nwant: %s", got, want)
	}
	if got, want := string(m.received[1]), `{"id":2,"op":"heal","container":"pg-primary","peer":"pg-standby"}`; got != want {
		t.Errorf("heal-pair wire\n got: %s\nwant: %s", got, want)
	}
}

func TestRunningParsesCensus(t *testing.T) {
	m := newMockAgent(t, func(req map[string]any) map[string]any {
		return map[string]any{
			"id": req["id"], "ok": true, "op": "containers",
			"containers": []map[string]any{
				{"name": "pg-primary-1", "service": "pg-primary", "state": "running", "exit_code": 0, "health": "healthy"},
				{"name": "pg-standby-1", "service": "pg-standby", "state": "exited", "exit_code": 1, "health": ""},
			},
		}
	})
	c, _ := tdvmm.Dial(m.path)
	defer c.Close()

	running, err := c.Running()
	if err != nil {
		t.Fatalf("running: %v", err)
	}
	if !running["pg-primary"] {
		t.Error("pg-primary should be running")
	}
	if running["pg-standby"] {
		t.Error("pg-standby is exited, should not be in the running set")
	}
}

func TestPingRoundTrip(t *testing.T) {
	m := newMockAgent(t, func(req map[string]any) map[string]any {
		return map[string]any{
			"id": req["id"], "ok": true, "op": "ping",
			"agent": "tdvmm-agent/1", "schema": 4, "build": "abc123",
		}
	})
	c, _ := tdvmm.Dial(m.path)
	defer c.Close()

	reply, err := c.Ping()
	if err != nil {
		t.Fatalf("ping: %v", err)
	}
	if reply.Agent == nil || *reply.Agent != "tdvmm-agent/1" {
		t.Errorf("agent = %v", reply.Agent)
	}
	if reply.Schema == nil || *reply.Schema != 4 {
		t.Errorf("schema = %v, want 4", reply.Schema)
	}
	if got := string(m.received[len(m.received)-1]); got != `{"id":1,"op":"ping"}` {
		t.Errorf("ping wire = %s", got)
	}
}

func TestDialMissingSocket(t *testing.T) {
	if _, err := tdvmm.Dial(filepath.Join(t.TempDir(), "nope")); err == nil {
		t.Fatal("dialing a nonexistent socket should error")
	}
}
