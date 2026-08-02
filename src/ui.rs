//! `dvmm build`'s progress UI (Fable CLI-UX ruling): ONE spinner line with a
//! step counter, e.g. `[3/8] squash images … 12s`. Gated on
//! `stderr.is_terminal()` — NEVER the run-loop's `interactive` gate (that gate
//! is untouched; see `main.rs`). Non-terminal stderr, `CI`, `TERM=dumb`, or
//! `--no-progress` all fall back to plain lines, BYTE-FOR-BYTE identical to
//! the pre-progress output (scripts tee/grep build stderr — this is FROZEN).
//! `NO_COLOR` strips color only; it does not disable the bar.
//!
//! This module is UI-only plumbing: it knows nothing about podman, the bake
//! pipeline, or the `engine` choke point (module-placement rule — UI depends
//! on nothing, the choke point depends on nothing UI). It is used ONLY by
//! `build.rs`'s orchestrator, never by `run`/`test`/`boot`/etc. A [`Progress`]
//! is always a value LOCAL to its owner (never a global — Fable coexistence
//! rule): it cannot outlive a bake, so it can never coexist with a running
//! VM's raw-tty console.

use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};

/// One stepped spinner line, or a no-op (plain-line fallback). Every method is
/// safe to call regardless of whether progress is active; `Drop` clears the
/// bar as a belt-and-suspenders backstop for early returns.
pub struct Progress {
    bar: Option<ProgressBar>,
}

/// Decide ONCE whether the bar should be active: a real stderr terminal, no
/// truthy `CI`, `TERM` isn't `dumb`, and `--no-progress` wasn't given.
fn enabled(no_progress_flag: bool) -> bool {
    if no_progress_flag {
        return false;
    }
    if !std::io::stderr().is_terminal() {
        return false;
    }
    if is_truthy_env("CI") {
        return false;
    }
    if std::env::var("TERM").as_deref() == Ok("dumb") {
        return false;
    }
    true
}

fn is_truthy_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => false,
    }
}

impl Progress {
    /// The `dvmm build` constructor: active iff [`enabled`] says so.
    pub fn new(no_progress_flag: bool) -> Progress {
        if !enabled(no_progress_flag) {
            return Progress { bar: None };
        }
        let bar = ProgressBar::new_spinner();
        let template = if std::env::var_os("NO_COLOR").is_some() {
            "{prefix} {msg} … {elapsed}"
        } else {
            "{prefix:.cyan.bold} {msg} … {elapsed}"
        };
        if let Ok(style) = ProgressStyle::with_template(template) {
            bar.set_style(style);
        }
        bar.enable_steady_tick(Duration::from_millis(120));
        Progress { bar: Some(bar) }
    }

    /// Always-disabled instance: for commands that share bake-pipeline helpers
    /// (`build-agent`, `build-kernel`) but must NEVER show a spinner — the
    /// progress UI is `build`-only (scope lock). Its `println` still falls
    /// back to plain `eprintln!`, so those commands' output is unchanged.
    pub fn disabled() -> Progress {
        Progress { bar: None }
    }

    /// Whether the bar is actually showing (used by the orchestrator to pick
    /// the `engine::OutputMode`).
    pub fn active(&self) -> bool {
        self.bar.is_some()
    }

    /// `[i/n] label` — sets the step-counter prefix + the step's headline
    /// message. Per-item detail within a step goes through [`Progress::msg`].
    pub fn step(&self, i: u32, n: u32, label: &str) {
        if let Some(b) = &self.bar {
            b.set_prefix(format!("[{i}/{n}]"));
            b.set_message(label.to_string());
        }
    }

    /// Update the message within the current step (e.g. per-image detail).
    pub fn msg(&self, text: impl Into<String>) {
        if let Some(b) = &self.bar {
            b.set_message(text.into());
        }
    }

    /// One chrome line. With the bar active this goes through
    /// `ProgressBar::println` (clears, prints, redraws); disabled, it is
    /// `eprintln!` verbatim — byte-for-byte what today's plain build prints
    /// (non-TTY output is FROZEN).
    pub fn println(&self, text: impl AsRef<str>) {
        match &self.bar {
            Some(b) => b.println(text.as_ref()),
            None => eprintln!("{}", text.as_ref()),
        }
    }

    /// Clear the bar (idempotent — safe to call more than once, e.g. once
    /// explicitly on a failure path and again via `Drop`). A `ProgressBar`
    /// must never outlive the bake.
    pub fn finish(&self) {
        if let Some(b) = &self.bar {
            b.finish_and_clear();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}
