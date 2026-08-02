// deterministic-vmm Go A/B service. Standard library ONLY (os/exec to psql +
// pg_isready, time, fmt, os, strconv) — no external modules, so there is no
// go.sum and the build is trivially hermetic + reproducible.
module dvmm-go-service

go 1.26
