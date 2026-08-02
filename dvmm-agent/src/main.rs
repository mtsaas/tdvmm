//! `dvmm-agent` — the guest-side control-channel executor (Rust rewrite of the
//! former `guest/agent/main.go`, ported behavior-for-behavior).
//!
//! A tiny STATIC (musl) binary baked into every `.dvmm`, running OUTSIDE the
//! workload containers. It is the guest end of the modeled control channel: the
//! 2nd 16550 (COM2 / ttyS1). Protocol: line-delimited JSON ([`dvmm_proto`]), one
//! request per line in, one reply per line out.
//!
//! **Fast-forward transparency is the whole point of the transport:** the agent
//! BLOCKS reading `/dev/ttyS1`. A blocked read arms no timer and generates no
//! wakes, so an idle guest with the agent baked in fast-forwards exactly as it
//! would without it. When the VMM delivers a command (at its scheduled virtual
//! time) it raises IRQ3; the agent wakes, runs the command, writes one reply
//! line, and blocks again.
//!
//! Ops: `ping`, `exec`, `containers`, `kill`, `stop`, `start`, `partition`,
//! `heal`. An unknown op is rejected. kill/stop/start WAIT for the container to
//! reach its new state so a following census is deterministic.
//!
//! Deps: `dvmm-proto` + `serde_json` (+ `serde` derive, already transitive) +
//! `std` ONLY. Raw-mode termios is done with a std-only inline-asm `ioctl`
//! syscall — no `libc`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dvmm_proto::{decode_line, encode_line, ContainerInfo, ErrorKind, Reply, Request, SCHEMA};
use serde::Deserialize;

const AGENT_ID: &str = "dvmm-agent/1";

/// Build hash embedded at compile time by the reproducible builder (the compat-
/// ibility oracle reported in the hello + `ping`). `dev` for plain host builds.
const BUILD: &str = match option_env!("DVMM_AGENT_BUILD") {
    Some(s) => s,
    None => "dev",
};

/// The compose label podman/compose sets on each service's container(s).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

fn main() {
    // The control channel is ttyS1. Open read+write; the VMM captures our TX and
    // feeds our RX at scheduled virtual times.
    let dev = std::env::var("DVMM_AGENT_TTY").unwrap_or_else(|_| "/dev/ttyS1".to_string());
    let file = match OpenOptions::new().read(true).write(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            // No control channel: nothing to do. Exit quietly (no wakes).
            eprintln!("dvmm-agent: cannot open {dev}: {e}");
            return;
        }
    };

    // Put ttyS1 in RAW mode. Critical: the default tty line discipline ECHOes
    // input, which would bounce every received command straight back to the VMM
    // as spurious "reply" bytes and desync the line-delimited protocol. Raw mode
    // also drops canonical line-buffering and \n<->\r\n translation, so the bytes
    // on the wire are exactly what each side wrote. VMIN=1/VTIME=0 => a read
    // blocks until >=1 byte (no timer armed => fast-forward-transparent).
    if let Err(e) = set_raw(file.as_raw_fd()) {
        eprintln!("dvmm-agent: setRaw({dev}): errno {e}");
    }

    let mut writer = match file.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("dvmm-agent: dup {dev}: {e}");
            return;
        }
    };
    let mut reader = BufReader::with_capacity(1 << 16, file);

    let mut agent = Agent {
        partitions: BTreeMap::new(),
    };

    // Proactive hello: the VMM's harness waits for this to mark the agent ready
    // (no ping round-trip needed). Carries schema + build (the compat oracle).
    write_line(&mut writer, &Reply::hello(AGENT_ID, BUILD));

    // The blocking read loop.
    let mut line: Vec<u8> = Vec::with_capacity(1 << 12);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return, // EOF / closed control channel: stop.
            Ok(_) => {}
            Err(_) => return,
        }
        if dvmm_proto::trim_frame(&line).is_empty() {
            continue;
        }
        let req: Request = match decode_line(&line) {
            Ok(r) => r,
            Err(e) => {
                write_line(
                    &mut writer,
                    &Reply {
                        ok: Some(false),
                        op: Some("?".into()),
                        error: Some(ErrorKind::BadRequest.msg(e.to_string())),
                        ..Default::default()
                    },
                );
                continue;
            }
        };
        let reply = agent.handle(&req);
        write_line(&mut writer, &reply);
    }
}

fn write_line<W: Write>(w: &mut W, reply: &Reply) {
    if let Ok(bytes) = encode_line(reply) {
        let _ = w.write_all(&bytes);
        let _ = w.flush();
    }
}

// ============================================================================
// Raw termios via a std-only inline-asm ioctl (no libc).
// ============================================================================

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
// c_iflag
const F_IGNBRK: u32 = 0x1;
const F_BRKINT: u32 = 0x2;
const F_PARMRK: u32 = 0x8;
const F_ISTRIP: u32 = 0x20;
const F_INLCR: u32 = 0x40;
const F_IGNCR: u32 = 0x80;
const F_ICRNL: u32 = 0x100;
const F_IXON: u32 = 0x400;
// c_oflag
const F_OPOST: u32 = 0x1;
// c_lflag
const F_ECHO: u32 = 0x8;
const F_ECHONL: u32 = 0x40;
const F_ICANON: u32 = 0x2;
const F_ISIG: u32 = 0x1;
const F_IEXTEN: u32 = 0x8000;
// c_cflag
const F_CSIZE: u32 = 0x30;
const F_PARENB: u32 = 0x100;
const F_CS8: u32 = 0x30;
// c_cc indices (kernel `struct termios`)
const I_VTIME: usize = 5;
const I_VMIN: usize = 6;

/// The kernel `struct termios` (asm-generic): 4 flag words + c_line + c_cc[19].
#[repr(C)]
#[derive(Default)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

/// `ioctl(fd, request, argp)` via the raw x86_64 syscall (nr 16). Returns the
/// kernel return value (negative errno on failure). std/core only.
#[cfg(target_arch = "x86_64")]
unsafe fn ioctl(fd: i32, request: u64, argp: *mut Termios) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") 16i64 => ret, // __NR_ioctl
        in("rdi") fd as i64,
        in("rsi") request,
        in("rdx") argp,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// cfmakeraw-equivalent on `fd`: no echo, no canonical mode, no signal genera-
/// tion, no I/O post-processing; VMIN=1/VTIME=0 blocking read. Returns Err(errno).
fn set_raw(fd: i32) -> Result<(), i64> {
    let mut t = Termios::default();
    let r = unsafe { ioctl(fd, TCGETS, &mut t) };
    if r < 0 {
        return Err(-r);
    }
    t.c_iflag &= !(F_IGNBRK | F_BRKINT | F_PARMRK | F_ISTRIP | F_INLCR | F_IGNCR | F_ICRNL | F_IXON);
    t.c_oflag &= !F_OPOST;
    t.c_lflag &= !(F_ECHO | F_ECHONL | F_ICANON | F_ISIG | F_IEXTEN);
    t.c_cflag &= !(F_CSIZE | F_PARENB);
    t.c_cflag |= F_CS8;
    t.c_cc[I_VMIN] = 1;
    t.c_cc[I_VTIME] = 0;
    let r = unsafe { ioctl(fd, TCSETS, &mut t) };
    if r < 0 {
        return Err(-r);
    }
    Ok(())
}

// ============================================================================
// Agent state + dispatch
// ============================================================================

struct Agent {
    /// Canonical unordered service-pair key -> the two container IPs to drop
    /// between. The whole nft ruleset is rebuilt from this on every change, so the
    /// installed rules are always exactly the active set. BTreeMap => sorted keys
    /// (deterministic ruleset, matching the Go agent's `sort.Strings`).
    partitions: BTreeMap<String, [String; 2]>,
}

impl Agent {
    fn handle(&mut self, req: &Request) -> Reply {
        match req.op.as_str() {
            "ping" => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("ping".into()),
                agent: Some(AGENT_ID.into()),
                schema: Some(SCHEMA),
                build: Some(BUILD.into()),
                ..Default::default()
            },
            "containers" => self.do_containers(req),
            "exec" => self.do_exec(req),
            "kill" => self.do_lifecycle(req, "kill"),
            "stop" => self.do_lifecycle(req, "stop"),
            "start" => self.do_start(req),
            "partition" => self.do_partition(req),
            "heal" => self.do_heal(req),
            other => Reply {
                id: Some(req.id),
                ok: Some(false),
                op: Some(other.into()),
                error: Some(ErrorKind::UnknownOp.msg(other)),
                ..Default::default()
            },
        }
    }

    fn do_containers(&self, req: &Request) -> Reply {
        match list_containers() {
            Ok(list) => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("containers".into()),
                containers: Some(list),
                ..Default::default()
            },
            Err(e) => err(req.id, "containers", ErrorKind::PodmanPs.msg(e)),
        }
    }

    fn do_exec(&self, req: &Request) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        let cmd = req.cmd.clone().unwrap_or_default();
        if container.is_empty() || cmd.is_empty() {
            return err(req.id, "exec", "exec requires `container` and `cmd`".into());
        }
        let id = match resolve_running(&container) {
            Ok(Some((id, _))) => id,
            Ok(None) => {
                // No running container for this service — the VMM treats this as
                // retryable inside wait_for, or an infra error for a hard exec.
                return err(
                    req.id,
                    "exec",
                    ErrorKind::NoContainer.msg(format!("no running container for service {container}")),
                );
            }
            Err(e) => return err(req.id, "exec", ErrorKind::PodmanPs.msg(e)),
        };

        let start = Instant::now();
        let out = Command::new("podman")
            .arg("exec")
            .arg(&id)
            .args(&cmd)
            .output();
        let dur = start.elapsed().as_millis() as u64;
        match out {
            Ok(o) => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("exec".into()),
                exit: Some(o.status.code().unwrap_or(-1) as i64),
                stdout: nonempty(o.stdout),
                stderr: nonempty(o.stderr),
                dur_ms: Some(dur),
                ..Default::default()
            },
            // podman itself could not run (not the command's exit) — infra.
            Err(e) => Reply {
                id: Some(req.id),
                ok: Some(false),
                op: Some("exec".into()),
                error: Some(ErrorKind::PodmanExec.msg(e.to_string())),
                dur_ms: Some(dur),
                ..Default::default()
            },
        }
    }

    /// `kill` / `stop`: resolve the RUNNING container, run the podman verb, then
    /// WAIT for it to actually stop so a following census is deterministic.
    fn do_lifecycle(&self, req: &Request, op: &str) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        if container.is_empty() {
            return err(req.id, op, format!("{op} requires `container`"));
        }
        let (id, name) = match resolve_by_service(&container, true) {
            Ok(Some(v)) => v,
            Ok(None) => {
                return err(
                    req.id,
                    op,
                    ErrorKind::NoContainer.msg(format!("no running container for service {container}")),
                )
            }
            Err(e) => return err(req.id, op, ErrorKind::PodmanPs.msg(e)),
        };
        let (_so, se, run) = run_podman(req.timeout_s.unwrap_or(0), &[op, &id]);
        if let Err(e) = run {
            return err(req.id, op, format!("podman_{op}: {e} {}", se.trim()));
        }
        // Block until the container has actually stopped (kill does not wait; stop
        // does, but this is idempotent + cheap and makes the census deterministic).
        let _ = run_podman(req.timeout_s.unwrap_or(0), &["wait", &id]);
        ok_stdout(req.id, op, format!("{op} {container} ({name})"))
    }

    /// Restart a previously stopped/killed container of the service.
    fn do_start(&self, req: &Request) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        if container.is_empty() {
            return err(req.id, "start", "start requires `container`".into());
        }
        let (id, name) = match resolve_by_service(&container, false) {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Already running? Treat as an idempotent success.
                if let Ok(Some(_)) = resolve_by_service(&container, true) {
                    return ok_stdout(req.id, "start", format!("already running {container}"));
                }
                return err(
                    req.id,
                    "start",
                    ErrorKind::NoContainer.msg(format!("no stopped container for service {container}")),
                );
            }
            Err(e) => return err(req.id, "start", ErrorKind::PodmanPs.msg(e)),
        };
        let (_so, se, run) = run_podman(req.timeout_s.unwrap_or(0), &["start", &id]);
        if let Err(e) = run {
            return err(req.id, "start", format!("podman_start: {e} {}", se.trim()));
        }
        ok_stdout(req.id, "start", format!("start {container} ({name})"))
    }

    /// Drop all traffic between the two services' running containers.
    fn do_partition(&mut self, req: &Request) -> Reply {
        let a = req.container.clone().unwrap_or_default();
        let b = req.peer.clone().unwrap_or_default();
        if a.is_empty() || b.is_empty() {
            return err(
                req.id,
                "partition",
                "partition requires two services (`container` and `peer`)".into(),
            );
        }
        let aid = match resolve_by_service(&a, true) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e)),
        };
        let bid = match resolve_by_service(&b, true) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e)),
        };
        let (aid, bid) = match (aid, bid) {
            (Some((aid, _)), Some((bid, _))) => (aid, bid),
            (aopt, bopt) => {
                return err(
                    req.id,
                    "partition",
                    ErrorKind::NoContainer.msg(format!(
                        "need both running ({a} running={}, {b} running={})",
                        aopt.is_some(),
                        bopt.is_some()
                    )),
                )
            }
        };
        let aip = match container_ip(&aid) {
            Ok(ip) => ip,
            Err(e) => return err(req.id, "partition", format!("ip({a}): {e}")),
        };
        let bip = match container_ip(&bid) {
            Ok(ip) => ip,
            Err(e) => return err(req.id, "partition", format!("ip({b}): {e}")),
        };
        enable_bridge_netfilter();
        let key = pair_key(&a, &b);
        self.partitions.insert(key.clone(), [aip.clone(), bip.clone()]);
        if let Err(e) = self.apply_partitions() {
            self.partitions.remove(&key);
            return err(req.id, "partition", ErrorKind::Nft.msg(e));
        }
        ok_stdout(
            req.id,
            "partition",
            format!("partition {a}({aip}) <-x-> {b}({bip})"),
        )
    }

    /// Remove the partition between two services, or ALL partitions when no
    /// services are given (`heal` / `heal: all`).
    fn do_heal(&mut self, req: &Request) -> Reply {
        let a = req.container.clone().unwrap_or_default();
        let b = req.peer.clone().unwrap_or_default();
        match (a.is_empty(), b.is_empty()) {
            (true, true) => self.partitions.clear(),
            (false, false) => {
                self.partitions.remove(&pair_key(&a, &b));
            }
            _ => {
                return err(
                    req.id,
                    "heal",
                    "heal needs two services (`container` and `peer`) or none (heal all)".into(),
                )
            }
        }
        if let Err(e) = self.apply_partitions() {
            return err(req.id, "heal", ErrorKind::Nft.msg(e));
        }
        let detail = if !a.is_empty() {
            format!("heal {a} <-> {b}")
        } else {
            "heal all".to_string()
        };
        ok_stdout(req.id, "heal", detail)
    }

    /// Rebuild our nft table from `partitions` in ONE atomic transaction. The
    /// add/delete/add idiom gives a clean slate whether or not the table pre-
    /// existed. The chain is a default-ACCEPT forward base chain, so only our
    /// explicit drop rules matter and all other traffic falls through to netavark.
    fn apply_partitions(&self) -> Result<(), String> {
        let mut b = String::new();
        b.push_str("add table inet dvmm_faults\n");
        b.push_str("delete table inet dvmm_faults\n");
        b.push_str("add table inet dvmm_faults\n");
        b.push_str("add chain inet dvmm_faults partition { type filter hook forward priority -300 ; policy accept ; }\n");
        for ips in self.partitions.values() {
            b.push_str(&format!(
                "add rule inet dvmm_faults partition ip saddr {} ip daddr {} drop\n",
                ips[0], ips[1]
            ));
            b.push_str(&format!(
                "add rule inet dvmm_faults partition ip saddr {} ip daddr {} drop\n",
                ips[1], ips[0]
            ));
        }
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "exit status {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

// ============================================================================
// podman helpers
// ============================================================================

/// A subset of `podman ps --format json` (only the fields we use).
#[derive(Deserialize, Default)]
struct PodmanPs {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "ExitCode", default)]
    exit_code: i64,
    #[serde(rename = "Labels", default)]
    labels: BTreeMap<String, String>,
    #[serde(rename = "Status", default)]
    status: String,
}

/// `podman <args>` capturing stdout; Err on spawn failure or nonzero exit.
fn podman_json(args: &[&str]) -> Result<Vec<PodmanPs>, String> {
    let out = Command::new("podman")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// `podman ps -a --format json`, normalized to the wire census shape.
fn list_containers() -> Result<Vec<ContainerInfo>, String> {
    let raw = podman_json(&["ps", "-a", "--format", "json"])?;
    let mut list = Vec::with_capacity(raw.len());
    for c in &raw {
        let name = c.names.first().cloned().unwrap_or_default();
        let svc = c.labels.get(COMPOSE_SERVICE_LABEL).cloned().unwrap_or_default();
        let s = c.status.to_lowercase();
        let health = if s.contains("unhealthy") {
            "unhealthy"
        } else if s.contains("healthy") {
            "healthy"
        } else if s.contains("starting") {
            "starting"
        } else {
            ""
        };
        list.push(ContainerInfo {
            name,
            service: svc,
            state: c.state.to_lowercase(),
            exit_code: c.exit_code,
            health: health.to_string(),
        });
    }
    Ok(list)
}

/// Find a RUNNING container id + name for the given compose service (label, else
/// a name match for single-name stacks). `Ok(None)` = not found (retryable).
fn resolve_running(service: &str) -> Result<Option<(String, String)>, String> {
    let raw = podman_json(&["ps", "--format", "json"])?;
    for c in &raw {
        if c.state != "running" {
            continue;
        }
        if c.labels.get(COMPOSE_SERVICE_LABEL).map(|v| v == service) == Some(true) {
            return Ok(Some((c.id.clone(), c.names.first().cloned().unwrap_or_default())));
        }
        for n in &c.names {
            if n == service {
                return Ok(Some((c.id.clone(), n.clone())));
            }
        }
    }
    Ok(None)
}

/// Find a container of the service: the first RUNNING one if `want_running`, else
/// the first NON-running one (for `start`). Matches label or name.
fn resolve_by_service(service: &str, want_running: bool) -> Result<Option<(String, String)>, String> {
    let raw = podman_json(&["ps", "-a", "--format", "json"])?;
    for c in &raw {
        let mut matched = c.labels.get(COMPOSE_SERVICE_LABEL).map(|v| v == service) == Some(true);
        if !matched {
            matched = c.names.iter().any(|n| n == service);
        }
        if !matched {
            continue;
        }
        if (c.state == "running") == want_running {
            return Ok(Some((c.id.clone(), c.names.first().cloned().unwrap_or_default())));
        }
    }
    Ok(None)
}

/// The first bridge IP of a container.
fn container_ip(id_or_name: &str) -> Result<String, String> {
    let out = Command::new("podman")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
            id_or_name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }
    Err(format!("no IP for {id_or_name}"))
}

/// Make bridged (intra-network) packets traverse the ip/inet netfilter hooks, so
/// our forward drop rules see container-to-container traffic. netavark already
/// sets this; set it defensively and ignore errors.
fn enable_bridge_netfilter() {
    for p in [
        "/proc/sys/net/bridge/bridge-nf-call-iptables",
        "/proc/sys/net/bridge/bridge-nf-call-ip6tables",
    ] {
        let _ = std::fs::write(p, "1\n");
    }
}

/// Run `podman <args>` under a timeout (guest seconds; fast-forwards), capturing
/// stdout/stderr. Returns `(stdout, stderr, Ok/Err)`. std-only timeout: poll
/// `try_wait`, `child.kill()` on the deadline (no libc, no raw signal syscall).
fn run_podman(timeout_s: u64, args: &[&str]) -> (String, String, Result<(), String>) {
    let timeout = Duration::from_secs(if timeout_s == 0 { 60 } else { timeout_s });
    let mut child = match Command::new("podman")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (String::new(), String::new(), Err(e.to_string())),
    };
    // Drain the pipes on threads so a chatty command can't deadlock on a full pipe
    // while we poll for the timeout.
    let mut so_pipe = child.stdout.take();
    let mut se_pipe = child.stderr.take();
    let so_h = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = so_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let se_h = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = se_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    };

    let so = String::from_utf8_lossy(&so_h.join().unwrap_or_default()).into_owned();
    let se = String::from_utf8_lossy(&se_h.join().unwrap_or_default()).into_owned();
    let res = match status {
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!("exit status {}", st.code().unwrap_or(-1))),
        None => Err("timed out".to_string()),
    };
    (so, se, res)
}

// ============================================================================
// small helpers
// ============================================================================

/// Canonical unordered service-pair key: `min(a,b) \0 max(a,b)`.
fn pair_key(a: &str, b: &str) -> String {
    if a > b {
        format!("{b}\0{a}")
    } else {
        format!("{a}\0{b}")
    }
}

fn nonempty(bytes: Vec<u8>) -> Option<String> {
    if bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn err(id: u64, op: &str, error: String) -> Reply {
    Reply {
        id: Some(id),
        ok: Some(false),
        op: Some(op.into()),
        error: Some(error),
        ..Default::default()
    }
}

fn ok_stdout(id: u64, op: &str, stdout: String) -> Reply {
    Reply {
        id: Some(id),
        ok: Some(true),
        op: Some(op.into()),
        stdout: Some(stdout),
        ..Default::default()
    }
}

// ============================================================================
// Tests — the agent side of the GOLDEN round-trip (its real decode/encode paths).
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn goldens_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dvmm-proto/goldens")
    }

    /// The agent's own code path: every REQUEST golden decodes via the same
    /// `decode_line::<Request>` the read loop uses, and every REPLY golden
    /// round-trips through `encode_line`/`decode_line` (what the agent emits).
    #[test]
    fn agent_roundtrips_goldens() {
        let dir = goldens_dir();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        assert!(!files.is_empty());
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let raw = std::fs::read(&path).unwrap();
            let golden: Value = decode_line(&raw).unwrap();
            let reenc: Value = if name.starts_with("req_") {
                let m: Request = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&encode_line(&m).unwrap()).unwrap()
            } else {
                let m: Reply = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&encode_line(&m).unwrap()).unwrap()
            };
            assert_eq!(golden, reenc, "{name}: agent round-trip mismatch");
        }
    }

    #[test]
    fn unknown_op_is_rejected() {
        let mut a = Agent {
            partitions: BTreeMap::new(),
        };
        let r = a.handle(&Request {
            id: 9,
            op: "frobnicate".into(),
            ..Default::default()
        });
        assert_eq!(r.ok, Some(false));
        assert_eq!(r.error.as_deref(), Some("unknown_op: frobnicate"));
    }

    #[test]
    fn ping_carries_schema_and_build() {
        let mut a = Agent {
            partitions: BTreeMap::new(),
        };
        let r = a.handle(&Request {
            id: 1,
            op: "ping".into(),
            ..Default::default()
        });
        assert_eq!(r.ok, Some(true));
        assert_eq!(r.schema, Some(SCHEMA));
        assert_eq!(r.agent.as_deref(), Some(AGENT_ID));
        assert!(r.build.is_some());
    }

    #[test]
    fn pair_key_is_order_independent() {
        assert_eq!(pair_key("a", "b"), pair_key("b", "a"));
    }
}
