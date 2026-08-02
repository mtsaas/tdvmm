// deterministic-vmm TEST-1a control-channel agent (dvmm-agent).
//
// A tiny STATIC guest-side binary, baked into every artifact, running OUTSIDE the
// workload containers. It is the guest end of the modeled control channel: the
// 2nd 16550 (COM2 / ttyS1). Protocol: line-delimited JSON, one request per line
// in, one reply per line out.
//
// Fast-forward transparency is the whole point of the transport: the agent
// BLOCKS reading /dev/ttyS1. A blocked read arms no timer and generates no wakes,
// so an idle guest with the agent baked in fast-forwards exactly as it would
// without it. When the VMM delivers a command (at its scheduled virtual time), it
// raises IRQ3; the agent wakes, runs the command, writes one reply line, and
// blocks again.
//
// TEST-1a ops: `ping`, `exec` (run a command in a compose service's container via
// `podman exec`, report exit + stdout/stderr), and `containers` (a census of the
// stack's containers).
//
// TEST-1b fault ACTIONS (this file):
//   * kill  <service> — `podman kill` the resolved running container (SIGKILL).
//   * stop  <service> — `podman stop` it (SIGTERM then SIGKILL after the timeout).
//   * start <service> — `podman start` a previously stopped/killed container.
//   * partition <A> <B> — drop all traffic between the two services' container IPs
//     (both directions) via nftables drop rules in the guest root netns. The guest
//     kernel has no nft `bridge` family, so this uses the `inet` family FORWARD
//     hook; `bridge-nf-call-iptables` (already set by netavark) makes bridged
//     intra-network packets traverse it. Rules live in our OWN table
//     (`inet dvmm_faults`), never netavark's, and the whole ruleset is rebuilt
//     atomically from the agent's active-partition set on every change.
//   * heal <A> <B> | heal (all) — remove the partition rule(s).
// kill/stop/start after the op WAIT for the container to reach its new state, so a
// following container census is deterministic. An unknown op is still rejected.
//
// Build: static + reproducible, exactly like the Go A/B service —
//   CGO_ENABLED=0 go build -trimpath -buildvcs=false -ldflags="-s -w -buildid="
// stdlib only, so the bytes are identical bake-to-bake (keeps `.dvmm` bit-repro).

package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"sort"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

const agentID = "dvmm-agent/1"

// The compose label podman/compose sets on each service's container(s).
const composeServiceLabel = "com.docker.compose.service"

type request struct {
	ID        uint64 `json:"id"`
	Op        string `json:"op"`
	Container string `json:"container,omitempty"`
	// Peer is the SECOND service for the two-party network faults
	// (partition/heal); Container is the first. Empty for the single-target ops.
	Peer     string   `json:"peer,omitempty"`
	Cmd      []string `json:"cmd,omitempty"`
	TimeoutS uint64   `json:"timeout_s,omitempty"`
}

type containerInfo struct {
	Name     string `json:"name"`
	Service  string `json:"service"`
	State    string `json:"state"`
	ExitCode int64  `json:"exit_code"`
	Health   string `json:"health"`
}

type reply struct {
	ID         uint64          `json:"id"`
	OK         bool            `json:"ok"`
	Op         string          `json:"op"`
	Exit       *int64          `json:"exit,omitempty"`
	Stdout     string          `json:"stdout,omitempty"`
	Stderr     string          `json:"stderr,omitempty"`
	Error      string          `json:"error,omitempty"`
	DurMs      uint64          `json:"dur_ms,omitempty"`
	Containers []containerInfo `json:"containers,omitempty"`
	Agent      string          `json:"agent,omitempty"`
}

// podman ps --format json entry (only the fields we use).
type podmanPS struct {
	ID       string            `json:"Id"`
	Names    []string          `json:"Names"`
	State    string            `json:"State"`
	ExitCode int64             `json:"ExitCode"`
	Labels   map[string]string `json:"Labels"`
	Status   string            `json:"Status"`
}

func main() {
	// The control channel is ttyS1. Open read+write; the VMM captures our TX and
	// feeds our RX at scheduled virtual times.
	dev := "/dev/ttyS1"
	if v := os.Getenv("DVMM_AGENT_TTY"); v != "" {
		dev = v
	}
	f, err := os.OpenFile(dev, os.O_RDWR, 0)
	if err != nil {
		// No control channel: nothing to do. Exit quietly (no wakes).
		fmt.Fprintf(os.Stderr, "dvmm-agent: cannot open %s: %v\n", dev, err)
		return
	}
	defer f.Close()

	// Put ttyS1 in RAW mode. Critical: the default tty line discipline ECHOes
	// input, which would bounce every command we receive straight back to the
	// VMM as spurious "reply" bytes and desync the line-delimited protocol. Raw
	// mode also drops canonical line-buffering and \n<->\r\n translation, so the
	// bytes on the wire are exactly what each side wrote.
	if err := setRaw(f.Fd()); err != nil {
		fmt.Fprintf(os.Stderr, "dvmm-agent: setRaw(%s): %v\n", dev, err)
	}

	w := bufio.NewWriter(f)
	writeLine := func(v interface{}) {
		b, err := json.Marshal(v)
		if err != nil {
			return
		}
		b = append(b, '\n')
		_, _ = w.Write(b)
		_ = w.Flush()
	}

	// Proactive hello: the VMM's harness waits for this to mark the agent ready
	// (no ping round-trip needed, and no early-RX buffering concerns).
	writeLine(struct {
		Agent string `json:"agent"`
	}{Agent: agentID})

	// The blocking read loop. A large buffer: exec commands can be long-ish.
	r := bufio.NewReaderSize(f, 1<<16)
	for {
		line, err := r.ReadString('\n') // BLOCKS here when idle — no wakes.
		if err != nil {
			// EOF/closed control channel: stop.
			return
		}
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		var req request
		if err := json.Unmarshal([]byte(line), &req); err != nil {
			writeLine(reply{OK: false, Op: "?", Error: "bad_request: " + err.Error()})
			continue
		}
		writeLine(handle(req))
	}
}

// Linux termios ioctls + flag bits (amd64), defined locally to keep the agent
// stdlib-only (no golang.org/x/sys dependency).
const (
	tcgets = 0x5401
	tcsets = 0x5402
	// c_iflag
	fIGNBRK = 0x1
	fBRKINT = 0x2
	fPARMRK = 0x8
	fISTRIP = 0x20
	fINLCR  = 0x40
	fIGNCR  = 0x80
	fICRNL  = 0x100
	fIXON   = 0x400
	// c_oflag
	fOPOST = 0x1
	// c_lflag
	fECHO   = 0x8
	fECHONL = 0x40
	fICANON = 0x2
	fISIG   = 0x1
	fIEXTEN = 0x8000
	// c_cflag
	fCSIZE  = 0x30
	fPARENB = 0x100
	fCS8    = 0x30
	// c_cc indices
	iVTIME = 5
	iVMIN  = 6
)

// setRaw applies a cfmakeraw-equivalent termios to the given fd: no echo, no
// canonical mode, no signal generation, no I/O post-processing — and a blocking
// read that returns as soon as >=1 byte is available (VMIN=1, VTIME=0), so an
// idle read parks the process with no timer (fast-forward-transparent).
func setRaw(fd uintptr) error {
	var t syscall.Termios
	if _, _, e := syscall.Syscall(syscall.SYS_IOCTL, fd, tcgets, uintptr(unsafe.Pointer(&t))); e != 0 {
		return e
	}
	t.Iflag &^= fIGNBRK | fBRKINT | fPARMRK | fISTRIP | fINLCR | fIGNCR | fICRNL | fIXON
	t.Oflag &^= fOPOST
	t.Lflag &^= fECHO | fECHONL | fICANON | fISIG | fIEXTEN
	t.Cflag &^= fCSIZE | fPARENB
	t.Cflag |= fCS8
	t.Cc[iVMIN] = 1
	t.Cc[iVTIME] = 0
	if _, _, e := syscall.Syscall(syscall.SYS_IOCTL, fd, tcsets, uintptr(unsafe.Pointer(&t))); e != 0 {
		return e
	}
	return nil
}

func handle(req request) reply {
	switch req.Op {
	case "ping":
		return reply{ID: req.ID, OK: true, Op: "ping", Agent: agentID}
	case "containers":
		return doContainers(req)
	case "exec":
		return doExec(req)
	case "kill":
		return doLifecycle(req, "kill")
	case "stop":
		return doLifecycle(req, "stop")
	case "start":
		return doStart(req)
	case "partition":
		return doPartition(req)
	case "heal":
		return doHeal(req)
	default:
		// Unknown op.
		return reply{ID: req.ID, OK: false, Op: req.Op, Error: "unknown_op: " + req.Op}
	}
}

// listContainers runs `podman ps -a --format json` and normalizes it.
func listContainers() ([]containerInfo, error) {
	out, err := exec.Command("podman", "ps", "-a", "--format", "json").Output()
	if err != nil {
		return nil, err
	}
	var raw []podmanPS
	if err := json.Unmarshal(out, &raw); err != nil {
		return nil, err
	}
	list := make([]containerInfo, 0, len(raw))
	for _, c := range raw {
		name := ""
		if len(c.Names) > 0 {
			name = c.Names[0]
		}
		svc := ""
		if c.Labels != nil {
			svc = c.Labels[composeServiceLabel]
		}
		health := ""
		s := strings.ToLower(c.Status)
		switch {
		case strings.Contains(s, "unhealthy"):
			health = "unhealthy"
		case strings.Contains(s, "healthy"):
			health = "healthy"
		case strings.Contains(s, "starting"):
			health = "starting"
		}
		list = append(list, containerInfo{
			Name:     name,
			Service:  svc,
			State:    strings.ToLower(c.State),
			ExitCode: c.ExitCode,
			Health:   health,
		})
	}
	return list, nil
}

func doContainers(req request) reply {
	list, err := listContainers()
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "containers", Error: "podman_ps: " + err.Error()}
	}
	return reply{ID: req.ID, OK: true, Op: "containers", Containers: list}
}

// resolveRunning finds a RUNNING container id for the given compose service.
func resolveRunning(service string) (string, string, error) {
	list, err := exec.Command("podman", "ps", "--format", "json").Output()
	if err != nil {
		return "", "", err
	}
	var raw []podmanPS
	if err := json.Unmarshal(list, &raw); err != nil {
		return "", "", err
	}
	for _, c := range raw {
		if c.State != "running" {
			continue
		}
		if c.Labels != nil && c.Labels[composeServiceLabel] == service {
			id := c.ID
			name := ""
			if len(c.Names) > 0 {
				name = c.Names[0]
			}
			return id, name, nil
		}
		// Fall back to matching the container name (single-name stacks).
		for _, n := range c.Names {
			if n == service {
				return c.ID, n, nil
			}
		}
	}
	return "", "", nil // not found (retryable / not-ready)
}

func doExec(req request) reply {
	if req.Container == "" || len(req.Cmd) == 0 {
		return reply{ID: req.ID, OK: false, Op: "exec", Error: "exec requires `container` and `cmd`"}
	}
	id, _, err := resolveRunning(req.Container)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "exec", Error: "podman_ps: " + err.Error()}
	}
	if id == "" {
		// No running container for this service — the VMM treats this as
		// retryable inside wait_for, or an infrastructure error for a hard exec.
		return reply{ID: req.ID, OK: false, Op: "exec",
			Error: "no_container: no running container for service " + req.Container}
	}

	args := append([]string{"exec", id}, req.Cmd...)
	cmd := exec.Command("podman", args...)
	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	start := time.Now()
	runErr := cmd.Run()
	dur := uint64(time.Since(start).Milliseconds())

	exit := int64(0)
	if runErr != nil {
		if ee, ok := runErr.(*exec.ExitError); ok {
			exit = int64(ee.ExitCode())
		} else {
			// podman itself could not run (not the command's exit) — infra.
			return reply{ID: req.ID, OK: false, Op: "exec",
				Error: "podman_exec: " + runErr.Error(), DurMs: dur}
		}
	}
	e := exit
	return reply{
		ID:     req.ID,
		OK:     true,
		Op:     "exec",
		Exit:   &e,
		Stdout: stdout.String(),
		Stderr: stderr.String(),
		DurMs:  dur,
	}
}

// ============================================================================
// TEST-1b fault executors: lifecycle (kill/stop/start) + network (partition/heal)
// ============================================================================

// runPodman runs a podman subcommand under a timeout (virtual seconds inside the
// guest, so it fast-forwards) and captures stdout/stderr.
func runPodman(timeoutS uint64, args ...string) (string, string, error) {
	if timeoutS == 0 {
		timeoutS = 60
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Duration(timeoutS)*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, "podman", args...)
	var so, se strings.Builder
	cmd.Stdout = &so
	cmd.Stderr = &se
	err := cmd.Run()
	return so.String(), se.String(), err
}

// resolveByService finds a container of the given compose service. If wantRunning
// it returns the first RUNNING one; otherwise the first NON-running one (for
// `start`). Returns ("","",nil) when none matches.
func resolveByService(service string, wantRunning bool) (id, name string, err error) {
	out, err := exec.Command("podman", "ps", "-a", "--format", "json").Output()
	if err != nil {
		return "", "", err
	}
	var raw []podmanPS
	if err := json.Unmarshal(out, &raw); err != nil {
		return "", "", err
	}
	for _, c := range raw {
		match := c.Labels != nil && c.Labels[composeServiceLabel] == service
		if !match {
			for _, n := range c.Names {
				if n == service {
					match = true
					break
				}
			}
		}
		if !match {
			continue
		}
		if (c.State == "running") == wantRunning {
			nm := ""
			if len(c.Names) > 0 {
				nm = c.Names[0]
			}
			return c.ID, nm, nil
		}
	}
	return "", "", nil
}

// doLifecycle handles `kill` and `stop`: resolve the RUNNING container, run the
// podman verb, then WAIT for it to actually stop so a following census is
// deterministic.
func doLifecycle(req request, op string) reply {
	if req.Container == "" {
		return reply{ID: req.ID, OK: false, Op: op, Error: op + " requires `container`"}
	}
	id, name, err := resolveByService(req.Container, true)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: op, Error: "podman_ps: " + err.Error()}
	}
	if id == "" {
		return reply{ID: req.ID, OK: false, Op: op,
			Error: "no_container: no running container for service " + req.Container}
	}
	_, se, runErr := runPodman(req.TimeoutS, op, id)
	if runErr != nil {
		return reply{ID: req.ID, OK: false, Op: op,
			Error: "podman_" + op + ": " + runErr.Error() + " " + strings.TrimSpace(se)}
	}
	// Block until the container has actually stopped (kill does not wait; stop
	// does, but this is idempotent + cheap and makes the census deterministic).
	_, _, _ = runPodman(req.TimeoutS, "wait", id)
	return reply{ID: req.ID, OK: true, Op: op, Stdout: op + " " + req.Container + " (" + name + ")"}
}

// doStart restarts a previously stopped/killed container of the service.
func doStart(req request) reply {
	if req.Container == "" {
		return reply{ID: req.ID, OK: false, Op: "start", Error: "start requires `container`"}
	}
	id, name, err := resolveByService(req.Container, false)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "start", Error: "podman_ps: " + err.Error()}
	}
	if id == "" {
		// Already running? Treat as an idempotent success.
		if rid, _, _ := resolveByService(req.Container, true); rid != "" {
			return reply{ID: req.ID, OK: true, Op: "start", Stdout: "already running " + req.Container}
		}
		return reply{ID: req.ID, OK: false, Op: "start",
			Error: "no_container: no stopped container for service " + req.Container}
	}
	_, se, runErr := runPodman(req.TimeoutS, "start", id)
	if runErr != nil {
		return reply{ID: req.ID, OK: false, Op: "start",
			Error: "podman_start: " + runErr.Error() + " " + strings.TrimSpace(se)}
	}
	return reply{ID: req.ID, OK: true, Op: "start", Stdout: "start " + req.Container + " (" + name + ")"}
}

// activePartitions maps a canonical unordered service-pair key to the two
// container IPs to drop between. The whole nft ruleset is rebuilt from this map on
// every partition/heal, so the installed rules are always exactly the active set.
var activePartitions = map[string][2]string{}

func pairKey(a, b string) string {
	if a > b {
		a, b = b, a
	}
	return a + "\x00" + b
}

// containerIP returns the first bridge IP of a container.
func containerIP(idOrName string) (string, error) {
	out, err := exec.Command("podman", "inspect", "-f",
		"{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}", idOrName).Output()
	if err != nil {
		return "", err
	}
	for _, tok := range strings.Fields(string(out)) {
		if tok != "" {
			return tok, nil
		}
	}
	return "", fmt.Errorf("no IP for %s", idOrName)
}

// enableBridgeNetfilter makes bridged (intra-network) packets traverse the
// ip/inet netfilter hooks, so our FORWARD drop rules see container-to-container
// traffic. netavark already sets this; we set it defensively and ignore errors.
func enableBridgeNetfilter() {
	for _, p := range []string{
		"/proc/sys/net/bridge/bridge-nf-call-iptables",
		"/proc/sys/net/bridge/bridge-nf-call-ip6tables",
	} {
		_ = os.WriteFile(p, []byte("1\n"), 0644)
	}
}

// applyPartitions rebuilds our nft table from activePartitions in ONE atomic
// transaction. The add/delete/add idiom gives a clean slate whether or not the
// table pre-existed. The chain is a default-ACCEPT FORWARD base chain, so only our
// explicit drop rules matter and all other traffic falls through to netavark.
// NB: `partition` (not the reserved keyword `fwd`) is a valid nft chain name.
func applyPartitions() error {
	var b strings.Builder
	b.WriteString("add table inet dvmm_faults\n")
	b.WriteString("delete table inet dvmm_faults\n")
	b.WriteString("add table inet dvmm_faults\n")
	b.WriteString("add chain inet dvmm_faults partition { type filter hook forward priority -300 ; policy accept ; }\n")
	keys := make([]string, 0, len(activePartitions))
	for k := range activePartitions {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		ips := activePartitions[k]
		fmt.Fprintf(&b, "add rule inet dvmm_faults partition ip saddr %s ip daddr %s drop\n", ips[0], ips[1])
		fmt.Fprintf(&b, "add rule inet dvmm_faults partition ip saddr %s ip daddr %s drop\n", ips[1], ips[0])
	}
	cmd := exec.Command("nft", "-f", "-")
	cmd.Stdin = strings.NewReader(b.String())
	var se strings.Builder
	cmd.Stderr = &se
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%v: %s", err, strings.TrimSpace(se.String()))
	}
	return nil
}

// doPartition drops all traffic between the two services' running containers.
func doPartition(req request) reply {
	a, b := req.Container, req.Peer
	if a == "" || b == "" {
		return reply{ID: req.ID, OK: false, Op: "partition",
			Error: "partition requires two services (`container` and `peer`)"}
	}
	aid, _, err := resolveByService(a, true)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "partition", Error: "podman_ps: " + err.Error()}
	}
	bid, _, err := resolveByService(b, true)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "partition", Error: "podman_ps: " + err.Error()}
	}
	if aid == "" || bid == "" {
		return reply{ID: req.ID, OK: false, Op: "partition",
			Error: fmt.Sprintf("no_container: need both running (%s running=%v, %s running=%v)",
				a, aid != "", b, bid != "")}
	}
	aip, err := containerIP(aid)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "partition", Error: "ip(" + a + "): " + err.Error()}
	}
	bip, err := containerIP(bid)
	if err != nil {
		return reply{ID: req.ID, OK: false, Op: "partition", Error: "ip(" + b + "): " + err.Error()}
	}
	enableBridgeNetfilter()
	key := pairKey(a, b)
	activePartitions[key] = [2]string{aip, bip}
	if err := applyPartitions(); err != nil {
		delete(activePartitions, key)
		return reply{ID: req.ID, OK: false, Op: "partition", Error: "nft: " + err.Error()}
	}
	return reply{ID: req.ID, OK: true, Op: "partition",
		Stdout: fmt.Sprintf("partition %s(%s) <-x-> %s(%s)", a, aip, b, bip)}
}

// doHeal removes the partition between two services, or ALL partitions when no
// services are given (`heal` with no args).
func doHeal(req request) reply {
	a, b := req.Container, req.Peer
	switch {
	case a == "" && b == "":
		activePartitions = map[string][2]string{}
	case a != "" && b != "":
		delete(activePartitions, pairKey(a, b))
	default:
		return reply{ID: req.ID, OK: false, Op: "heal",
			Error: "heal needs two services (`container` and `peer`) or none (heal all)"}
	}
	if err := applyPartitions(); err != nil {
		return reply{ID: req.ID, OK: false, Op: "heal", Error: "nft: " + err.Error()}
	}
	detail := "heal all"
	if a != "" {
		detail = "heal " + a + " <-> " + b
	}
	return reply{ID: req.ID, OK: true, Op: "heal", Stdout: detail}
}
