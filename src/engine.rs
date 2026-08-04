//! The SINGLE container-runtime choke point (Fable guardrail §2).
//!
//! Every `Command::new("podman")` in tdvmm lives HERE. Today the engine is podman
//! (Fable §7: podman-only); routing all ~30 build.rs call sites through this
//! module stages the future docker-or-podman switch as an ADDITIVE change in ONE
//! place rather than a scatter-gun edit across every site.
//!
//! The host's DEFAULT podman OCI runtime is misconfigured on this project's build
//! host (points at a missing `det-runsc`), so callers that actually *run* or
//! *build* containers pass a `containers.conf` pinning `runtime="runc"`; this
//! module never relies on the host default runtime.

use std::path::Path;
use std::process::Command;

/// The container-engine binary. Podman-only for now (Fable §7). A docker switch
/// would be an additive change to THIS constant + the constructors below, never a
/// change at the call sites.
pub const ENGINE: &str = "podman";

/// The raw choke point: a bare `podman` [`Command`]. Every other constructor
/// builds on this, and NOTHING outside this module calls `Command::new("podman")`.
pub fn command() -> Command {
    Command::new(ENGINE)
}

/// A scratch-store invocation — `podman --root <store> --runroot <run>
/// --storage-driver vfs` with the clean `CONTAINERS_CONF` — used for the bake's
/// pull/squash/build/save steps against a throwaway vfs store.
pub fn scratch(store: &Path, runroot: &Path, conf: &Path) -> Command {
    let mut c = command();
    c.env("CONTAINERS_CONF", conf)
        .arg("--root")
        .arg(store)
        .arg("--runroot")
        .arg(runroot)
        .arg("--storage-driver")
        .arg("vfs");
    c
}

/// `podman unshare …` with the clean `CONTAINERS_CONF` (the user-namespace
/// re-exec helper `tdvmm build` uses for `__seed-build` / `__assemble-initramfs`).
pub fn unshare(conf: &Path) -> Command {
    let mut c = command();
    c.env("CONTAINERS_CONF", conf).arg("unshare");
    c
}

/// How a child's stdout/stderr are handled (Fable CLI-UX ruling). `Inherit`
/// streams live — today's behavior everywhere, and the ONLY mode ever used
/// outside `tdvmm build`'s orchestrator. `CaptureOnFailure` buffers both
/// streams and discards them on success; on failure the full captured bytes
/// are folded into the returned error message (never swallowed). The BUILD
/// ORCHESTRATOR picks the mode (from whether its progress bar is active) —
/// this module stays UI-free: it never touches a progress bar, and it never
/// decides which mode to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    Inherit,
    CaptureOnFailure,
}

/// Run a command for its side effect, per `mode`. Every run-time path
/// (`run`/`test`/`boot`/…) never calls this with anything but `Inherit` — in
/// fact it never calls this at all; it is exclusively a `tdvmm build` helper.
pub fn run(cmd: &mut Command, mode: OutputMode) -> Result<(), String> {
    match mode {
        OutputMode::Inherit => {
            let status = cmd
                .status()
                .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
            if !status.success() {
                return Err(format!("command {:?} failed ({status})", cmd.get_program()));
            }
            Ok(())
        }
        OutputMode::CaptureOnFailure => {
            let out = cmd
                .output()
                .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
            if !out.status.success() {
                let mut msg = format!(
                    "command {:?} failed ({}); captured output:\n",
                    cmd.get_program(),
                    out.status
                );
                msg.push_str(&String::from_utf8_lossy(&out.stdout));
                msg.push_str(&String::from_utf8_lossy(&out.stderr));
                return Err(msg);
            }
            Ok(())
        }
    }
}
