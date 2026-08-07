// tdvmm driverlab driver — exercises what a driver can do to a run's outcome.
//
// One baked artifact, several behaviors, chosen per run with the DRIVER_MODE
// environment variable (set from the kernel cmdline; see ../compose.yml).
package main

import (
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	tdvmm "github.com/mtsaas/tdvmm/sdk/go"
)

func log(msg string) { fmt.Printf("[driver] %s\n", msg) }

func main() {
	mode := strings.TrimSpace(os.Getenv("DRIVER_MODE"))
	if mode == "" {
		mode = "pass"
	}
	log(fmt.Sprintf("mode=%q", mode))

	h, err := tdvmm.Connect()
	if err != nil {
		fmt.Printf("[driver] cannot reach the harness: %v\n", err)
		os.Exit(1)
	}
	defer h.Close()
	if r, _ := h.Ping(); r != nil && r.Schema != nil {
		log(fmt.Sprintf("connected: agent schema %d", *r.Schema))
	}

	switch mode {
	case "pass":
		_ = h.Finish(0, "driverlab pass")

	case "fail":
		_ = h.Finish(3, "driverlab deliberate failure")

	case "faults":
		runFaults(h)

	case "hang":
		// Never finish: the run must be ended by the wall-clock safety timeout.
		log("hanging deliberately; the wall-clock timeout should end this run")
		for {
			time.Sleep(time.Hour)
		}

	default:
		_ = h.Finish(1, fmt.Sprintf("unknown driver mode %q", mode))
	}
}

func runFaults(h *tdvmm.Client) {
	if err := h.WaitForServices([]string{"peer"}, 2*time.Minute); err != nil {
		_ = h.Finish(1, err.Error())
		return
	}
	// kill() returns only once the container is really dead, so this census needs
	// no retry loop.
	if err := h.Kill("peer"); err != nil {
		_ = h.Finish(1, fmt.Sprintf("kill peer: %v", err))
		return
	}
	if running, _ := h.Running(); running["peer"] {
		_ = h.Finish(1, "peer still running immediately after kill()")
		return
	}
	log("peer is down")

	if err := h.Start("peer"); err != nil {
		_ = h.Finish(1, fmt.Sprintf("start peer: %v", err))
		return
	}
	if err := h.WaitUntil(func() bool { r, _ := h.Running(); return r["peer"] },
		2*time.Minute, time.Second, "peer to restart"); err != nil {
		_ = h.Finish(1, err.Error())
		return
	}
	log("peer is back")

	// A fault against a service that does not exist must fail cleanly.
	err := h.Partition("peer", "nope")
	var ce *tdvmm.CommandError
	if err == nil {
		_ = h.Finish(1, "partitioning an unknown service unexpectedly succeeded")
		return
	}
	if errors.As(err, &ce) {
		log(fmt.Sprintf("unknown service rejected as expected: %s", ce.Code))
	}
	_ = h.Finish(0, "driverlab faults ok")
}
