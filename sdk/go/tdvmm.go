package tdvmm

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"strings"
	"sync"
	"time"
)

// SocketPath is the control socket the in-guest tdvmm-agent serves, bind-mounted
// into every container in the stack. It mirrors tdvmm_proto::CONTROL_SOCKET_PATH
// and is the default target of Connect; override it per-process with the
// TDVMM_CONTROL_SOCKET environment variable, or per-call with Dial.
const SocketPath = "/run/tdvmm/ctl/sock"

// EnvSocketPath is the environment variable that overrides SocketPath for Connect.
const EnvSocketPath = "TDVMM_CONTROL_SOCKET"

// DefaultTimeout bounds how long a single command waits for the agent's reply.
// Generous because a fault call blocks until the fault is really applied (a kill
// waits for the container to actually stop), and guest seconds are cheap.
const DefaultTimeout = 120 * time.Second

// DefaultRetry bounds how long Connect keeps retrying the initial dial, because a
// driver container can start before the agent has bound the socket.
const DefaultRetry = 30 * time.Second

// resolveSocketPath returns the env override if set, else SocketPath.
func resolveSocketPath() string {
	if v := os.Getenv(EnvSocketPath); v != "" {
		return v
	}
	return SocketPath
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

// CommandError is returned when the agent refused or could not perform a command
// (a reply with ok=false). Code is the stable machine-matchable prefix of the
// agent's error string ("no_container", "nft", "podman_op", "unknown_op", ...),
// so a driver can branch on the failure kind without matching the whole message.
type CommandError struct {
	// Op is the op that failed (e.g. "partition").
	Op string
	// Reason is the agent's full error string ("<code>: <detail>").
	Reason string
	// Code is the stable prefix of Reason, before the first ':' (empty if none).
	Code string
}

func (e *CommandError) Error() string { return e.Op + ": " + e.Reason }

// codeOf extracts the stable prefix of an agent error string: everything before
// the first ':', or "" when there is no ':'. Mirrors the Python SDK's .code.
func codeOf(errStr string) string {
	if i := strings.IndexByte(errStr, ':'); i >= 0 {
		return errStr[:i]
	}
	return ""
}

// ---------------------------------------------------------------------------
// Wire messages (mirror tdvmm_proto::{Request,Reply,ContainerInfo,GuestEvent})
// ---------------------------------------------------------------------------

// Request is one command line sent to the agent. Every typed method below is a
// thin wrapper that fills the fields it needs; Do sends a Request directly, so a
// new agent op is usable before this package knows about it. The optional fields
// are pointers/slices so an unset field is omitted from the wire line exactly as
// the agent's serde and the Python SDK expect.
//
// Id is assigned by the client when the request is sent; any value set here is
// overwritten.
type Request struct {
	Id        uint64   `json:"id"`
	Op        string   `json:"op"`
	Container *string  `json:"container,omitempty"`
	Peer      *string  `json:"peer,omitempty"`
	Cmd       []string `json:"cmd,omitempty"`
	TimeoutS  *uint64  `json:"timeout_s,omitempty"`
	Cursor    *uint64  `json:"cursor,omitempty"`
	MaxBytes  *uint64  `json:"max_bytes,omitempty"`
	Exit      *int64   `json:"exit,omitempty"`
	Message   *string  `json:"message,omitempty"`
}

// ContainerInfo is one container in a census (the containers reply).
type ContainerInfo struct {
	Name     string `json:"name"`
	Service  string `json:"service"`
	State    string `json:"state"`
	ExitCode int64  `json:"exit_code"`
	Health   string `json:"health"`
}

// GuestEvent is a guest→host assertion/telemetry event. A control-socket client
// never receives one (they flow host-ward), but it is part of the Reply type.
type GuestEvent struct {
	Kind    string          `json:"kind"`
	Name    string          `json:"name,omitempty"`
	Ok      *bool           `json:"ok,omitempty"`
	Exit    *int64          `json:"exit,omitempty"`
	Details json.RawMessage `json:"details,omitempty"`
}

// Reply is the agent's answer to a command. Every discriminating field is
// optional (a pointer), mirroring the single permissive proto Reply type.
type Reply struct {
	Id         *uint64         `json:"id,omitempty"`
	Ok         *bool           `json:"ok,omitempty"`
	Op         *string         `json:"op,omitempty"`
	Exit       *int64          `json:"exit,omitempty"`
	Stdout     *string         `json:"stdout,omitempty"`
	Stderr     *string         `json:"stderr,omitempty"`
	Error      *string         `json:"error,omitempty"`
	DurMs      *uint64         `json:"dur_ms,omitempty"`
	Containers []ContainerInfo `json:"containers,omitempty"`
	Agent      *string         `json:"agent,omitempty"`
	Schema     *uint32         `json:"schema,omitempty"`
	Build      *string         `json:"build,omitempty"`
	Data       *string         `json:"data,omitempty"`
	NextCursor *uint64         `json:"next_cursor,omitempty"`
	Eof        *bool           `json:"eof,omitempty"`
	Event      *GuestEvent     `json:"event,omitempty"`
	Seq        *uint64         `json:"seq,omitempty"`
}

// encodeLine renders one message as a single framed wire line: compact JSON plus
// a trailing '\n'. HTML escaping is disabled so the bytes match the Python SDK's
// json.dumps(separators=(",",":")) output for the same request.
func encodeLine(v any) ([]byte, error) {
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil { // Encode appends the trailing '\n'.
		return nil, err
	}
	return b.Bytes(), nil
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

// Client is a live connection to the in-guest harness. It injects faults into
// its own cluster (partition, kill, stop, start, heal) and ends the run with a
// verdict (Finish). Create one with Connect or Dial.
//
// A Client is safe for use by multiple goroutines: each command is a serialized
// request/reply round-trip, so concurrent calls take turns on the one socket.
type Client struct {
	mu       sync.Mutex
	conn     net.Conn
	r        *bufio.Reader
	timeout  time.Duration
	nextID   uint64
	finished bool
}

// config holds the tunables Connect/Dial accept via Option.
type config struct {
	timeout time.Duration
	retry   time.Duration
}

// Option configures Connect or Dial.
type Option func(*config)

// WithTimeout sets how long a single command waits for the agent's reply.
func WithTimeout(d time.Duration) Option { return func(c *config) { c.timeout = d } }

// WithRetry sets how long Connect keeps retrying the initial dial. Ignored by Dial.
func WithRetry(d time.Duration) Option { return func(c *config) { c.retry = d } }

func newConfig(opts ...Option) config {
	c := config{timeout: DefaultTimeout, retry: DefaultRetry}
	for _, o := range opts {
		o(&c)
	}
	return c
}

func newClient(conn net.Conn, timeout time.Duration) *Client {
	return &Client{
		conn:    conn,
		r:       bufio.NewReader(conn),
		timeout: timeout,
		nextID:  1,
	}
}

// Connect reaches the harness at the default control socket (SocketPath, or the
// TDVMM_CONTROL_SOCKET override). It retries briefly, because a driver container
// can start before the agent has bound the socket, and returns an error if the
// socket never appears — which usually means you are not running inside a tdvmm
// guest.
func Connect(opts ...Option) (*Client, error) {
	cfg := newConfig(opts...)
	path := resolveSocketPath()
	deadline := time.Now().Add(cfg.retry)
	for {
		conn, err := net.DialTimeout("unix", path, cfg.timeout)
		if err == nil {
			return newClient(conn, cfg.timeout), nil
		}
		if time.Now().After(deadline) {
			return nil, fmt.Errorf(
				"cannot reach the tdvmm control socket at %s: %w; "+
					"is this container running inside a tdvmm guest?", path, err)
		}
		time.Sleep(500 * time.Millisecond)
	}
}

// Dial connects once to a specific control-socket path (no retry). Use it to
// point at a non-default socket; Connect is the usual in-guest entry point.
func Dial(path string, opts ...Option) (*Client, error) {
	cfg := newConfig(opts...)
	conn, err := net.DialTimeout("unix", path, cfg.timeout)
	if err != nil {
		return nil, fmt.Errorf("cannot reach the tdvmm control socket at %s: %w", path, err)
	}
	return newClient(conn, cfg.timeout), nil
}

// Close closes the connection. It does NOT end the run — see Finish.
func (c *Client) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.conn.Close()
}

// Do sends one raw command and returns the agent's reply. Every typed method is
// a thin wrapper over this, so a new agent op is usable before this package
// knows about it. A reply with ok=false is returned as a *CommandError.
func (c *Client) Do(req *Request) (*Reply, error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	req.Id = c.nextID
	c.nextID++

	line, err := encodeLine(req)
	if err != nil {
		return nil, fmt.Errorf("encoding %q request: %w", req.Op, err)
	}
	if c.timeout > 0 {
		_ = c.conn.SetDeadline(time.Now().Add(c.timeout))
	}
	if _, err := c.conn.Write(line); err != nil {
		return nil, fmt.Errorf("control socket failed during %q: %w", req.Op, err)
	}

	raw, err := c.r.ReadBytes('\n')
	if err != nil && len(raw) == 0 {
		if err == io.EOF {
			return nil, fmt.Errorf(
				"control socket closed while waiting for the reply to %q "+
					"(did the run already end?)", req.Op)
		}
		return nil, fmt.Errorf("control socket failed during %q: %w", req.Op, err)
	}

	var reply Reply
	if jerr := json.Unmarshal(bytes.TrimSpace(raw), &reply); jerr != nil {
		return nil, fmt.Errorf("malformed reply to %q: %q: %w", req.Op, raw, jerr)
	}
	if reply.Ok == nil || !*reply.Ok {
		reason := "the agent reported no reason"
		if reply.Error != nil && *reply.Error != "" {
			reason = *reply.Error
		}
		return &reply, &CommandError{Op: req.Op, Reason: reason, Code: codeOf(reason)}
	}
	return &reply, nil
}

func strPtr(s string) *string { return &s }
func u64Ptr(v uint64) *uint64 { return &v }

// ---------------------------------------------------------------------------
// Network faults
// ---------------------------------------------------------------------------

// Partition drops ALL traffic between two services, both directions. It returns
// once the rule is installed, so a request fired after this call is guaranteed
// to meet the partition. Services are compose service names.
func (c *Client) Partition(a, b string) error {
	_, err := c.Do(&Request{Op: "partition", Container: strPtr(a), Peer: strPtr(b)})
	return err
}

// Heal undoes one partition, or ALL of them when called with no arguments.
// Call Heal() to heal everything, or Heal(a, b) to heal a single pair; any other
// argument count is an error, mirroring the Python SDK.
func (c *Client) Heal(pair ...string) error {
	req := &Request{Op: "heal"}
	switch len(pair) {
	case 0:
		// heal all
	case 2:
		req.Container = strPtr(pair[0])
		req.Peer = strPtr(pair[1])
	default:
		return fmt.Errorf("heal takes two services or none (heal-all), got %d", len(pair))
	}
	_, err := c.Do(req)
	return err
}

// ---------------------------------------------------------------------------
// Container faults
// ---------------------------------------------------------------------------

// Kill SIGKILLs a service's container and waits for it to actually be dead.
func (c *Client) Kill(service string) error {
	_, err := c.Do(&Request{Op: "kill", Container: strPtr(service)})
	return err
}

// Stop gracefully stops a service's container (SIGTERM, then SIGKILL).
func (c *Client) Stop(service string) error {
	_, err := c.Do(&Request{Op: "stop", Container: strPtr(service)})
	return err
}

// Start starts a previously stopped/killed container. Idempotent if running.
func (c *Client) Start(service string) error {
	_, err := c.Do(&Request{Op: "start", Container: strPtr(service)})
	return err
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

// Ping round-trips the agent; the reply carries its identity, wire schema, and build.
func (c *Client) Ping() (*Reply, error) {
	return c.Do(&Request{Op: "ping"})
}

// Containers returns the container census: name, service, state, exit_code, health.
func (c *Client) Containers() ([]ContainerInfo, error) {
	reply, err := c.Do(&Request{Op: "containers"})
	if err != nil {
		return nil, err
	}
	return reply.Containers, nil
}

// Running returns the set of services that currently have a running container,
// as a map for O(1) membership tests (running[svc] is true when svc is up).
func (c *Client) Running() (map[string]bool, error) {
	list, err := c.Containers()
	if err != nil {
		return nil, err
	}
	out := make(map[string]bool, len(list))
	for _, ci := range list {
		if ci.State == "running" {
			out[ci.Service] = true
		}
	}
	return out, nil
}

// Exec runs a command inside ANOTHER service's container, exec'd directly (argv).
// A nonzero exit is NOT an error here — it is a result in the returned reply's
// Exit/Stdout/Stderr; only the agent failing to run the command returns an error.
func (c *Client) Exec(service string, argv ...string) (*Reply, error) {
	return c.Do(&Request{Op: "exec", Container: strPtr(service), Cmd: argv})
}

// ExecShell runs a shell script inside ANOTHER service's container via `sh -c`,
// the string form of the Python SDK's exec. See Exec for the exit semantics.
func (c *Client) ExecShell(service, script string) (*Reply, error) {
	return c.Exec(service, "sh", "-c", script)
}

// Logs reads a service's container log from the start, paging to the end.
func (c *Client) Logs(service string) (string, error) {
	var sb strings.Builder
	var cursor uint64
	for {
		reply, err := c.Do(&Request{Op: "logs", Container: strPtr(service), Cursor: u64Ptr(cursor)})
		if err != nil {
			return "", err
		}
		if reply.Data != nil {
			sb.WriteString(*reply.Data)
		}
		if reply.NextCursor != nil {
			cursor = *reply.NextCursor
		}
		if reply.Eof == nil || *reply.Eof {
			return sb.String(), nil
		}
	}
}

// ---------------------------------------------------------------------------
// Waiting (virtual time)
// ---------------------------------------------------------------------------

// WaitUntil polls pred until it returns true, sleeping every between attempts.
// The sleeps are ordinary time.Sleep, so an idle guest fast-forwards through
// them: a long timeout costs no real time if nothing else is running. A pred that
// panics is treated as "not yet" (a probe that throws just has not succeeded
// yet), matching the Python SDK. It returns an error on timeout.
func (c *Client) WaitUntil(pred func() bool, timeout, every time.Duration, what string) error {
	if every <= 0 {
		every = time.Second
	}
	deadline := time.Now().Add(timeout)
	for {
		if safePred(pred) {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("timed out after %s waiting for %s", timeout, what)
		}
		time.Sleep(every)
	}
}

// safePred runs pred, recovering a panic as false ("not yet").
func safePred(pred func() bool) (ok bool) {
	defer func() {
		if recover() != nil {
			ok = false
		}
	}()
	return pred()
}

// WaitForServices blocks (in virtual time) until every named service has a
// running container.
func (c *Client) WaitForServices(services []string, timeout time.Duration) error {
	want := append([]string(nil), services...)
	return c.WaitUntil(func() bool {
		running, err := c.Running()
		if err != nil {
			return false
		}
		for _, s := range want {
			if !running[s] {
				return false
			}
		}
		return true
	}, timeout, time.Second, fmt.Sprintf("services %v to be running", want))
}

// ---------------------------------------------------------------------------
// Ending the run
// ---------------------------------------------------------------------------

// Finish ends the run with a verdict: 0 passes, anything else fails. This is what
// makes a run a test. It blocks until the agent acknowledges, then returns; the
// harness tears the VM down immediately afterwards, so treat it as the last
// statement your driver runs. It does NOT close the socket. The FIRST finish
// decides the run; a second one is refused and returns a *CommandError. Pass an
// empty message to omit it.
func (c *Client) Finish(exitCode int, message string) error {
	code := int64(exitCode)
	req := &Request{Op: "finish", Exit: &code}
	if message != "" {
		req.Message = strPtr(message)
	}
	if _, err := c.Do(req); err != nil {
		return err
	}
	c.mu.Lock()
	c.finished = true
	c.mu.Unlock()
	return nil
}

// Fail ends the run as a FAILURE with a reason. Shorthand for Finish(1, message).
func (c *Client) Fail(message string) error { return c.Finish(1, message) }
