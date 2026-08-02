//! The SINGLE container-runtime choke point (Fable guardrail §2).
//!
//! Every `Command::new("podman")` in dvmm lives HERE. Today the engine is podman
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
/// re-exec helper `dvmm build` uses for `__seed-build` / `__assemble-initramfs`).
pub fn unshare(conf: &Path) -> Command {
    let mut c = command();
    c.env("CONTAINERS_CONF", conf).arg("unshare");
    c
}
