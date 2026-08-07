// Package tdvmm drives the test harness from inside a container in the stack.
//
// A tdvmm guest runs a compose stack in a single-vCPU VM whose idle time is
// fast-forwarded. This package lets a container in that stack talk back to the
// harness: it injects faults into its own cluster (partition, kill, stop, start,
// heal) and ends the run with a verdict. Workload and faults are one program, so
// the network can be cut while a request is in flight:
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
// # The control socket
//
// The client speaks one JSON object per line over the unix socket at
// tdvmm_proto::CONTROL_SOCKET_PATH ([SocketPath]), which the in-guest tdvmm-agent
// serves and bind-mounts into every container. It is the same protocol and the
// same handler the VMM host uses.
//
// # Faults are applied before the call returns
//
// Each fault method returns only after the agent has installed the rule
// (nftables) or the container has reached its new state (podman kill + wait), so
// a request fired after the call is guaranteed to meet the fault. Ordering is
// program order.
//
// # Virtual time
//
// A sleeping guest is idle, so the VMM fast-forwards its clock: time.Sleep(time.Hour)
// returns in microseconds of wall time. Sleep, and [Client.WaitUntil] which sleeps
// between probes, is the virtual-time API.
//
// # Ending the run
//
// [Client.Finish] ends the run; its code is the verdict (0 passes, anything else
// fails). The FIRST finish wins. A driver that exits without calling it is bounded
// by --wall-timeout / --max-virtual-time and does not pass.
package tdvmm
