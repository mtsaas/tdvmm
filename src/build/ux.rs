//! The bake pipeline's command-running + progress plumbing: the [`Ux`] handle
//! (progress bar + child-output mode) threaded through the helpers, plus the thin
//! wrappers over the [`engine`] choke point (Fable guardrail §2 — every container
//! invocation routes through `engine`).

use std::path::Path;
use std::process::Command;

use crate::engine;
use crate::ui;

/// Run a command, returning stdout on success, or an Err(message) on failure.
/// `.output()` already captures both streams, so this needs no `OutputMode`
/// knob — the full stderr is already in the error on failure.
pub(super) fn capture(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!(
            "command {:?} failed ({}): {}",
            cmd.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command for its side effect, per `mode` (Fable CLI-UX ruling — see
/// `engine::run`). A thin pass-through: the engine choke point does the work.
pub(super) fn run(cmd: &mut Command, mode: engine::OutputMode) -> Result<(), String> {
    engine::run(cmd, mode)
}

/// How many transcript lines a failed build's error surfaces inline (the full
/// log is on disk at the printed path — dumping a multi-MB `make` log to the
/// terminal helps nobody).
const FAIL_EXCERPT_LINES: usize = 100;

/// Run a long BUILD-step child (the kernel/agent container compiles). With the
/// viewport active: stream its output into the live tail
/// ([`ui::Progress::tail_line`]) and write the full transcript to `log_path`
/// on success AND failure (a once-per-machine compile log is cheap and gold);
/// a failure's error carries the last [`FAIL_EXCERPT_LINES`] lines plus the
/// log path — nothing is swallowed. With the viewport inactive this is exactly
/// `engine::run(cmd, ux.mode)`, keeping the frozen non-TTY bytes for these
/// steps byte-identical (they inherit, as before).
pub(super) fn run_build(
    cmd: &mut Command,
    ux: &Ux,
    log_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if !ux.progress.active() {
        return Ok(run(cmd, ux.mode)?);
    }
    let result = engine::run_streamed(cmd, &|line| ux.progress.tail_line(line));
    let transcript = match &result {
        Ok(t) => t.as_str(),
        Err(e) => e.transcript.as_str(),
    };
    // Best-effort log write: losing the log must not fail a successful build.
    let logged = log_path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(log_path, transcript))
        .is_ok();
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let lines: Vec<&str> = e.transcript.lines().collect();
            let shown = &lines[lines.len().saturating_sub(FAIL_EXCERPT_LINES)..];
            let mut msg = format!(
                "{}\n--- last {} of {} output lines ---\n{}",
                e.message,
                shown.len(),
                lines.len(),
                shown.join("\n")
            );
            if logged {
                msg.push_str(&format!("\nfull log: {}", log_path.display()));
            }
            Err(msg.into())
        }
    }
}

/// Bundles the progress handle + child-output mode threaded through the bake
/// pipeline's helper functions. A plain borrowed local — never a global
/// (Fable coexistence rule) — so it cannot outlive `cmd_build`'s call.
/// `build-agent` / `build-kernel` construct a permanently-inherit instance
/// (via [`Ux::inherit`]) over a [`ui::Progress::disabled`], so their output
/// stays a plain inherited passthrough, unaffected by the progress bar.
pub(super) struct Ux<'a> {
    pub(super) progress: &'a ui::Progress,
    pub(super) mode: engine::OutputMode,
}

impl<'a> Ux<'a> {
    /// For commands that share bake-pipeline helpers but must never show
    /// progress UI or capture child output (scope lock — progress is
    /// `build`-only): `build-agent`, `build-kernel`.
    pub(super) fn inherit(progress: &'a ui::Progress) -> Ux<'a> {
        Ux { progress, mode: engine::OutputMode::Inherit }
    }
}

/// A container-engine invocation against a scratch vfs store (mirrors `bp()`),
/// with the clean CONTAINERS_CONF set. Routes through the single engine choke
/// point (Fable guardrail §2).
pub(super) fn podman(store: &Path, runroot: &Path, conf: &Path) -> Command {
    engine::scratch(store, runroot, conf)
}
