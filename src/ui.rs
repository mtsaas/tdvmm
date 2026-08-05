//! `tdvmm build`'s progress UI (Fable CLI-UX ruling).
//!
//! Interactive TTY: a single stepped spinner line (`⠋ [3/8] label … 12s`) that,
//! on every step transition, PERSISTS the finished step as a `✓ [i/n] label`
//! line (plus an aligned indented sub-list for multi-item steps, via
//! [`Progress::item`], or a short inline note, via [`Progress::note`]) before
//! the next spinner appears. [`Progress::finish_steps`] flushes the LAST step's
//! checkmark and clears the bar for good — always called before the final
//! summary, so the step counter deterministically reaches `[n/n]` and no stale
//! spinner frame is ever left behind (never dependent on the steady-tick timer
//! catching up). Gated on `stderr.is_terminal()` — NEVER the run-loop's
//! `interactive` gate (that gate is untouched; see `main.rs`).
//!
//! Non-terminal stderr, `CI`, `TERM=dumb`, or `--no-progress` all fall back to
//! plain lines, BYTE-FOR-BYTE identical to the pre-progress output (scripts
//! tee/grep build stderr — this is FROZEN): [`Progress::step`], [`Progress::note`],
//! [`Progress::item`], [`Progress::finish_steps`] and [`Progress::print_summary`]
//! all no-op when the bar is inactive, and [`Progress::detail`] (the routine,
//! now-TTY-suppressed chrome) falls back to plain `eprintln!` — identical to
//! [`Progress::println`]'s frozen branch. Only [`Progress::println`] is for
//! lines that must ALWAYS surface (warnings/errors): it prints in both modes.
//! `NO_COLOR` strips color only; it does not disable the bar.
//!
//! This module is UI-only plumbing: it knows nothing about podman, the bake
//! pipeline, or the `engine` choke point (module-placement rule — UI depends
//! on nothing, the choke point depends on nothing UI). It is used ONLY by
//! `build.rs`'s orchestrator, never by `run`/`test`/`boot`/etc. A [`Progress`]
//! is always a value LOCAL to its owner (never a global — Fable coexistence
//! rule): it cannot outlive a bake, so it can never coexist with a running
//! VM's raw-tty console.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

/// One in-flight step's TTY render state (frozen/non-TTY mode never builds
/// one — [`Progress::step`] no-ops when the bar is inactive).
struct StepState {
    i: u32,
    n: u32,
    label: String,
    note: Option<String>,
    items: Vec<(String, u64)>,
    /// When this step's `step()` began; its `✓` line renders `start.elapsed()`
    /// right-aligned as the step's wall-clock duration.
    start: Instant,
}

/// One stepped spinner line, or a no-op (plain-line fallback). Every method is
/// safe to call regardless of whether progress is active; `Drop` clears the
/// bar as a belt-and-suspenders backstop for early returns.
pub struct Progress {
    bar: Option<ProgressBar>,
    /// `NO_COLOR` snapshot, for the manually-rendered checkmark glyph (it
    /// bypasses indicatif's own template coloring).
    color: bool,
    /// TTY-only: the step currently owning the spinner, so the NEXT `step()`
    /// (or `finish_steps()`) persists its checkmark line first.
    current: RefCell<Option<StepState>>,
    /// Wall-clock start, for the final summary's `time` field.
    start: Instant,
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
    /// The `tdvmm build` constructor: active iff [`enabled`] says so.
    pub fn new(no_progress_flag: bool) -> Progress {
        let color = std::env::var_os("NO_COLOR").is_none();
        if !enabled(no_progress_flag) {
            return Progress { bar: None, color, current: RefCell::new(None), start: Instant::now() };
        }
        let bar = ProgressBar::new_spinner();
        let template = if color {
            "{spinner:.cyan.bold} {prefix:.cyan.bold} {msg} … {elapsed}"
        } else {
            "{spinner} {prefix} {msg} … {elapsed}"
        };
        if let Ok(style) = ProgressStyle::with_template(template) {
            bar.set_style(style);
        }
        bar.enable_steady_tick(Duration::from_millis(120));
        Progress { bar: Some(bar), color, current: RefCell::new(None), start: Instant::now() }
    }

    /// Always-disabled instance: for commands that share bake-pipeline helpers
    /// (`build-agent`, `build-kernel`) but must NEVER show a spinner — the
    /// progress UI is `build`-only (scope lock). Its `println` still falls
    /// back to plain `eprintln!`, so those commands' output is unchanged.
    pub fn disabled() -> Progress {
        Progress { bar: None, color: false, current: RefCell::new(None), start: Instant::now() }
    }

    /// Whether the bar is actually showing (used by the orchestrator to pick
    /// the `engine::OutputMode`).
    pub fn active(&self) -> bool {
        self.bar.is_some()
    }

    /// Wall-clock time since construction (the final summary's `time` field).
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// The TTY-only title line (`tdvmm build  <stack>` + a blank line), printed
    /// once before the first step. A no-op in frozen mode — the plain output
    /// has no equivalent line.
    pub fn title(&self, stack: &str) {
        if self.bar.is_none() {
            return;
        }
        eprintln!("tdvmm build  {stack}");
        eprintln!();
    }

    /// `[i/n] label` — starts a new step's spinner. Frozen/non-TTY: a no-op,
    /// exactly as before (the label/step-count are never part of the plain
    /// output). TTY: first PERSISTS the previous step's `✓` line (+ its note /
    /// item sub-list, if any) above the bar, THEN resets the spinner's prefix,
    /// message, and per-step elapsed clock for the new step.
    pub fn step(&self, i: u32, n: u32, label: &str) {
        let Some(b) = &self.bar else { return };
        self.flush_current(b);
        *self.current.borrow_mut() = Some(StepState {
            i, n, label: label.to_string(), note: None, items: Vec::new(), start: Instant::now(),
        });
        b.set_prefix(format!("[{i}/{n}]"));
        b.set_message(label.to_string());
        b.reset_elapsed();
    }

    /// A short inline note for the CURRENT step's `✓` line (e.g. `miss → full
    /// bake`), for single-fact steps. TTY-only; a no-op in frozen mode.
    pub fn note(&self, text: impl Into<String>) {
        if self.bar.is_none() {
            return;
        }
        if let Some(st) = self.current.borrow_mut().as_mut() {
            st.note = Some(text.into());
        }
    }

    /// One entry (`name`, size in MiB) in the CURRENT step's aligned indented
    /// sub-list (e.g. per-image pull/build results) — rendered under the `✓`
    /// line once the step finishes. TTY-only; a no-op in frozen mode.
    pub fn item(&self, name: impl Into<String>, size_mib: u64) {
        if self.bar.is_none() {
            return;
        }
        if let Some(st) = self.current.borrow_mut().as_mut() {
            st.items.push((name.into(), size_mib));
        }
    }

    /// One chrome line that must ALWAYS surface — warnings, gate failures,
    /// errors (Fable CLI-UX ruling: routine detail may be demoted, these never
    /// are). With the bar active this clears/prints/redraws via
    /// `ProgressBar::println` (width-clamped to the terminal); disabled, it is
    /// `eprintln!` verbatim — byte-for-byte what today's plain build prints
    /// (non-TTY output is FROZEN).
    pub fn println(&self, text: impl AsRef<str>) {
        match &self.bar {
            Some(b) => b.println(clamp_lines(text.as_ref(), term_width())),
            None => eprintln!("{}", text.as_ref()),
        }
    }

    /// One ROUTINE chrome line (the old wall-of-detail): frozen/non-TTY prints
    /// it verbatim via `eprintln!` (byte-identical to before — these call
    /// sites used to go through `println`, and this branch is identical to
    /// that one). TTY: suppressed entirely — `build.rs` relocates anything
    /// informative here into the diagnostics file instead.
    pub fn detail(&self, text: impl AsRef<str>) {
        if self.bar.is_none() {
            eprintln!("{}", text.as_ref());
        }
    }

    /// Persist the pending step's `✓` line (+ note / item sub-list) above the
    /// bar, if any, with the step's wall-clock duration right-aligned on that
    /// `✓` line. A no-op if nothing is pending, or if the bar is inactive.
    fn flush_current(&self, b: &ProgressBar) {
        let Some(st) = self.current.borrow_mut().take() else { return };
        let width = term_width();
        let mark = if self.color { "\x1b[32m\u{2713}\x1b[0m" } else { "\u{2713}" };
        // The note column is fixed (matches the mockup): long enough for the
        // longest step label ("compose.lock + binds", 21 chars) plus a gap.
        const LABEL_COL: usize = 22;
        let plain = match &st.note {
            Some(note) => format!("[{}/{}] {:<LABEL_COL$}{note}", st.i, st.n, st.label),
            None => format!("[{}/{}] {}", st.i, st.n, st.label),
        };
        // Wall time between this step's `step()` and now, right-aligned on this
        // step's OWN `✓` line — never on the item sub-lines appended below.
        let dur = human_duration(st.start.elapsed());
        let mut out = render_step_line(mark, &plain, &dur, width);
        if !st.items.is_empty() {
            let name_w = st.items.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0) + 4;
            let vals: Vec<String> = st.items.iter().map(|(_, mib)| human_size_short(*mib)).collect();
            let val_w = vals.iter().map(|v| v.chars().count()).max().unwrap_or(0);
            for ((name, _), val) in st.items.iter().zip(vals.iter()) {
                let line = format!("            {name:<name_w$}{val:>val_w$}");
                out.push('\n');
                out.push_str(&clamp(&line, width));
            }
        }
        b.println(out);
    }

    /// Flush the LAST step's `✓` line and clear the bar for good (Fix: the
    /// counter must never leave a stale spinner frame on screen). Idempotent;
    /// a no-op in frozen/disabled mode. Always call this BEFORE any final
    /// summary — never rely on tick timing.
    pub fn finish_steps(&self) {
        if let Some(b) = &self.bar {
            self.flush_current(b);
            b.finish_and_clear();
        }
    }

    /// The aligned final summary block (`built` / `sha256` / `size` / `time` +
    /// an optional `details` pointer). Calls [`Progress::finish_steps`] first,
    /// so the bar is always cleared before anything below it prints. TTY-only
    /// — a no-op in frozen mode, so `build.rs` can call it unconditionally.
    pub fn print_summary(&self, out_path: &Path, sha256: &str, size_bytes: u64, elapsed: Duration, diagnostics: Option<&Path>) {
        if self.bar.is_none() {
            return;
        }
        self.finish_steps();
        let width = term_width();
        eprintln!();
        eprintln!("{}", clamp(&format!("  built  {}", display_path(out_path)), width));
        eprintln!("{}", clamp(&format!("         sha256  {}", short_sha(sha256)), width));
        eprintln!("{}", clamp(&format!("         size    {}", human_size(size_bytes)), width));
        eprintln!("{}", clamp(&format!("         time    {}", human_duration(elapsed)), width));
        if let Some(d) = diagnostics {
            eprintln!();
            eprintln!("{}", clamp(&format!("  details  {}", display_path(d)), width));
        }
    }

    /// Run `f` (arbitrary I/O — typically a plain `println!` to STDOUT, which
    /// this module never touches otherwise) with the bar cleared, then redraw
    /// it afterward. Without this, a raw `println!` racing the steady-tick
    /// redraw thread can interleave with the spinner line on screen. A no-op
    /// wrapper (just calls `f`) when the bar is inactive.
    pub fn suspend<F: FnOnce() -> R, R>(&self, f: F) -> R {
        match &self.bar {
            Some(b) => b.suspend(f),
            None => f(),
        }
    }

    /// Clear the bar (idempotent — safe to call more than once, e.g. once
    /// explicitly on a failure path and again via `Drop`). A `ProgressBar`
    /// must never outlive the bake. Does NOT flush a pending step's checkmark
    /// (a failed/aborted step should never appear to have succeeded) — use
    /// [`Progress::finish_steps`] on success paths instead.
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

// ============================================================================
// TTY rendering helpers: terminal width + truncation + compact formatting.
// Non-TTY/frozen output never goes through any of these (Fable guardrail —
// see the module doc comment).
// ============================================================================

const DEFAULT_WIDTH: usize = 80;
const MIN_WIDTH: usize = 40;

/// The live stderr terminal width, via a raw `TIOCGWINSZ` ioctl on `libc`
/// (already a direct dependency for the KVM ioctls — adding NOTHING to
/// `Cargo.toml`/`Cargo.lock`, which matters here: `build.rs`'s bake-cache key
/// AND the guest agent's embedded build hash both fold in `Cargo.lock`'s
/// bytes, so touching it would perturb the hashed `.tdvmm` artifact — an
/// output-only UI change must not). Falls back to 80 columns if it can't be
/// determined, clamped to a sane minimum.
fn term_width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(std::io::stderr().as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 {
        (ws.ws_col as usize).max(MIN_WIDTH)
    } else {
        DEFAULT_WIDTH
    }
}

/// Truncate `s` to at most `max` display columns, appending `…` if it didn't
/// fit. Character-counted (not byte-counted) so multi-byte UTF-8 is never
/// split mid-codepoint.
fn clamp(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep: String = s.chars().take(max - 1).collect();
    format!("{keep}…")
}

/// [`clamp`], applied independently to every line of a possibly-multi-line
/// string (used for `println` — warnings/errors may span multiple lines).
fn clamp_lines(s: &str, max: usize) -> String {
    s.lines().map(|l| clamp(l, max)).collect::<Vec<_>>().join("\n")
}

/// The persistent `  ✓ [i/n] label [note]` line, with `dur` right-aligned to
/// the terminal `width` (buildkit-style). The 4 fixed leading columns (`"  "` +
/// the 1-visible-column `mark` + `" "`) plus the clamped, colorless `plain`
/// remainder never overflow `width`; only `plain` is clamped, so a colored
/// `mark`'s ANSI escape is never split. `dur` is reserved its own width plus a
/// one-space minimum gap flush at the right edge; when `plain` is shorter, the
/// gap grows so `dur` still lands at column `width`. Narrow terminals degrade
/// gracefully: `plain` shrinks to whatever remains, and if even the mark + gap
/// + `dur` won't fit, `dur` is dropped rather than wrapping the line.
fn render_step_line(mark: &str, plain: &str, dur: &str, width: usize) -> String {
    const PREFIX: usize = 4;
    let dur_len = dur.chars().count();
    let left_budget = width.saturating_sub(dur_len + 1);
    if left_budget <= PREFIX {
        return format!("  {mark} {}", clamp(plain, width.saturating_sub(PREFIX)));
    }
    let plain = clamp(plain, left_budget - PREFIX);
    let gap = width.saturating_sub(PREFIX + plain.chars().count() + dur_len).max(1);
    format!("  {mark} {plain}{pad}{dur}", pad = " ".repeat(gap))
}

/// Render an absolute path relative to the CWD, or `~/…` under `$HOME`, else
/// the path as-is. TTY display only — NEVER used for stdout, the diagnostics
/// file (which keeps full absolute paths), or anything hashed.
fn display_path(p: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = p.strip_prefix(&cwd) {
            if !rel.as_os_str().is_empty() {
                return rel.display().to_string();
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rel) = p.strip_prefix(PathBuf::from(home)) {
            return format!("~/{}", rel.display());
        }
    }
    p.display().to_string()
}

/// First 8 + last 7 hex chars of a full digest (`8afaa03a…6c88507`); short
/// strings pass through unchanged. Display only — never used where the full
/// digest matters (stdout, the diagnostics file, stack.lock).
fn short_sha(s: &str) -> String {
    if s.len() <= 16 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..8], &s[s.len() - 7..])
    }
}

/// Compact size for the per-item sub-list (`283M`, `1.2G`).
fn human_size_short(mib: u64) -> String {
    if mib >= 1024 {
        format!("{:.1}G", mib as f64 / 1024.0)
    } else {
        format!("{mib}M")
    }
}

/// Verbose size for the final summary (`198 MiB`, `1.23 GiB`).
fn human_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{} MiB", (b / MIB).round() as u64)
    }
}

/// `1m26s` / `12.3s` / `0.4s` — reused for BOTH each step's right-aligned
/// duration and the final summary's `time` field, so the two always agree.
/// Sub-minute renders one decimal (`0.4s`); a minute or more is whole seconds
/// (`1m26s`), where the fractional part is noise.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}
