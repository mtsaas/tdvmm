// deterministic-vmm TEST-1a control-channel agent (dvmm-agent). Standard library
// ONLY (encoding/json, os/exec, bufio, time) — no external modules, so there is
// no go.sum and the build is trivially hermetic + reproducible.
module dvmm-agent

go 1.26
