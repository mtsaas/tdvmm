// Package tdvmm drives the test harness from inside your own container.
//
// A tdvmm guest runs your compose stack in a single-vCPU VM whose idle time is
// fast-forwarded. This package is how a container inside that stack talks back to
// the harness: it injects faults into its own cluster (partition, kill, stop,
// start, heal), and it ends the run with a verdict.
//
// The point is that the workload and the faults are ONE program. You can cut the
// network while a request is in flight and observe what the cluster does with a
// half-delivered operation — the Jepsen shape — instead of scripting faults from
// outside against wall-clock guesses:
//
//	h, err := tdvmm.Connect()
//	if err != nil {
//		log.Fatal(err)
//	}
//	defer h.Close()
//
//	committed := make(chan error, 1)
//	go func() { committed <- db.Write("orders", row) }() // in flight to the cluster
//	h.Partition("pg-primary", "pg-standby")               // cut it mid-write
//	// ... assert the commit stays blocked ...
//	h.Heal()
//	h.Finish(0, "")
//
// # What you are talking to
//
// tdvmm_proto::CONTROL_SOCKET_PATH ([SocketPath]), a unix socket the in-guest
// tdvmm-agent serves. It is bind-mounted into every container in the stack, so
// any of them can drive. The wire protocol is one JSON object per line — the SAME
// protocol and the SAME handler the VMM itself uses, so a container has exactly
// the fault vocabulary the harness has, no more.
//
// # Faults are applied before the call returns
//
// Every fault method is synchronous: it returns only after the agent has actually
// installed the rule (nftables) or the container has actually reached its new
// state (podman kill + podman wait). So
//
//	h.Partition("a", "b")
//	fireRequest() // the network is ALREADY cut here
//
// is deterministic — there is no "did it land yet?" window to guess at. Order
// your fault and your workload however you need; the ordering is program order.
//
// # Virtual time
//
// time.Sleep(time.Hour) costs microseconds of wall time: the guest goes idle, the
// VMM fast-forwards the clock, and your sleep returns having "taken" an hour. That
// is how you write "wait a day, then partition" without waiting a day. Sleep (and
// [Client.WaitUntil], which sleeps between probes) is the virtual-time API.
//
// # Ending the run
//
// [Client.Finish] is what makes a run a test. It ends the run and its code becomes
// the verdict: 0 passes, anything else fails. The FIRST finish wins. If your
// driver exits without calling it, the run is bounded by --wall-timeout /
// --max-virtual-time instead and does not pass.
//
// This package is the Go sibling of the Python SDK in sdk/python; both speak the
// exact same wire protocol.
package tdvmm
