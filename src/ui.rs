//! `tdvmm build`'s progress UI (Fable CLI-UX ruling), on ratatui's INLINE
//! viewport.
//!
//! Interactive TTY: a fixed live region below the cursor — the current step's
//! spinner row (`⠙ [3/10] label … 12s`, cyan-bold glyph + counter) over a dim,
//! bounded tail of streamed child output (the kernel/agent container compiles,
//! BuildKit-style; see [`Progress::tail_line`]) — animated by a background
//! ticker (mirroring indicatif's steady tick). The inline viewport cannot
//! resize after creation, so its height (1 + [`MAX_TAIL_ROWS`], terminal
//! permitting) is decided once at start and unused tail rows render blank. On
//! every step transition the finished step PERSISTS into terminal scrollback —
//! its `✓ [i/n] label` line (plus an aligned indented sub-list for multi-item
//! steps, via [`Progress::item`], or a short inline note, via
//! [`Progress::note`]); the tail is ephemeral (success collapses it, BuildKit
//! behavior) — via [`Terminal::insert_before`], so completed lines survive
//! above the redrawing region and remain after exit (the alternate screen is
//! NEVER used — it would wipe scrollback). [`Progress::finish_steps`] flushes
//! the LAST step's checkmark and blanks the live region; [`Progress::finish`]
//! then collapses it, reclaiming the rows so nothing stale is left behind
//! (never dependent on tick timing) — and a step still pending at collapse did
//! NOT succeed, so it persists as a red `✗` plus its final tail lines instead.
//! Gated on `stderr.is_terminal()` — NEVER the run-loop's `interactive` gate.
//!
//! Non-terminal stderr, `CI`, `TERM=dumb`, `--no-progress`, OR a failed terminal
//! init all fall back to plain lines, BYTE-FOR-BYTE identical to the pre-progress
//! output (scripts tee/grep build stderr — this is FROZEN): [`Progress::step`],
//! [`Progress::note`], [`Progress::item`], [`Progress::finish_steps`] and
//! [`Progress::print_summary`] all no-op when the viewport is inactive, and
//! [`Progress::detail`] (the routine, now-TTY-suppressed chrome) falls back to
//! plain `eprintln!`. Only [`Progress::println`] is for lines that must ALWAYS
//! surface (warnings/errors): it prints in both modes. `NO_COLOR` strips color
//! only; it does not disable the viewport.
//!
//! Thread-safety: the bake pipeline shares one `&Progress` across a
//! `thread::scope`, so worker threads (parallel image bakes, the overlapped
//! agent build) call [`Progress::item`]/[`Progress::detail`]/[`Progress::println`]
//! concurrently, and the ticker thread redraws alongside them. ratatui's
//! `Terminal` is not `Sync`, so it lives — with the current-step state — behind
//! ONE `Mutex` (mirroring the old `Mutex<Option<StepState>>`); every terminal
//! touch takes that lock, keeping `Progress: Sync`.
//!
//! This module is UI-only plumbing: it knows nothing about podman, the bake
//! pipeline, or the `engine` choke point (module-placement rule — UI depends
//! on nothing, the choke point depends on nothing UI). It is used ONLY by
//! `build.rs`'s orchestrator, never by `run`/`test`/`boot`/etc. A [`Progress`]
//! is always a value LOCAL to its owner (never a global — Fable coexistence
//! rule): it cannot outlive a bake, so it can never coexist with a running
//! VM's raw-tty console.

use std::collections::VecDeque;
use std::io::{IsTerminal, Stderr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{Clear, ClearType};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};

/// The inline viewport's backend: crossterm over stderr (stdout stays clean —
/// the progress UI is stderr, and `build.rs` gates the stdout identity line).
type Term = Terminal<CrosstermBackend<Stderr>>;

/// The braille "dots" spinner (the glyphs the old bar drew), advanced one frame
/// per tick by the ticker thread.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Ticker cadence — the old bar's steady tick.
const TICK: Duration = Duration::from_millis(120);

/// Live-tail height budget: the viewport is `1 + min(MAX_TAIL_ROWS,
/// term_height - 4)` rows, fixed at [`Live::start`] (the inline viewport
/// cannot resize, and tearing the terminal down mid-build to grow it is
/// exactly the corruption this module exists to prevent).
const MAX_TAIL_ROWS: u16 = 6;

/// One in-flight step's render state (frozen/non-TTY mode never builds one —
/// [`Progress::step`] no-ops when the viewport is inactive).
struct StepState {
    i: u32,
    n: u32,
    label: String,
    note: Option<String>,
    items: Vec<(String, u64)>,
    /// The bounded live tail of streamed child output (display ring only —
    /// the full transcript lives in the build log, not the UI).
    tail: VecDeque<String>,
    /// When this step's `step()` began; its `✓` line renders `start.elapsed()`
    /// right-aligned as the step's wall-clock duration.
    start: Instant,
}

/// The inline viewport plus the step it is currently drawing — everything that
/// touches the terminal, guarded by ONE `Mutex` so `Progress` stays `Sync`.
/// `term` becomes `None` once collapsed, so [`Renderer::collapse`] is idempotent.
struct Renderer {
    term: Option<Term>,
    current: Option<StepState>,
    /// Spinner frame index (the ticker advances it).
    frame: usize,
    /// `NO_COLOR` snapshot: cyan-bold spinner + green checkmark when true.
    color: bool,
    /// Tail rows below the spinner row (fixed at [`Live::start`]; unused rows
    /// render blank).
    tail_rows: u16,
}

impl Renderer {
    /// The live viewport's full width (= terminal width, for the inline
    /// viewport). Used to right-align durations and clamp overlong lines.
    fn width(&mut self) -> usize {
        match &mut self.term {
            Some(t) => t.get_frame().area().width as usize,
            None => 0,
        }
    }

    /// Redraw the live region: the current step's spinner row (or blank when
    /// no step is active) over the step's dim live tail, blank-padded to the
    /// fixed viewport height. Best-effort: terminal write errors are dropped —
    /// the UI must never break the build.
    fn redraw(&mut self) {
        let Renderer { term, current, frame, color, tail_rows } = self;
        let Some(term) = term.as_mut() else { return };
        let width = term.get_frame().area().width as usize;
        let glyph = SPINNER[*frame % SPINNER.len()];
        let mut rows = Vec::with_capacity(1 + *tail_rows as usize);
        rows.push(match current.as_ref() {
            Some(st) => spinner_line(glyph, st, *color),
            None => Line::raw(""),
        });
        for i in 0..*tail_rows as usize {
            rows.push(match current.as_ref().and_then(|st| st.tail.get(i)) {
                Some(text) => tail_row(text, *color, width),
                None => Line::raw(""),
            });
        }
        // Pad every row to full width so a shorter frame can't leave a previous
        // frame's trailing glyphs behind (a bare `Line` only writes its own cells).
        let rows: Vec<Line> = rows.into_iter().map(|row| pad_to_width(row, width)).collect();
        let _ = term.draw(|f| {
            let area = f.area();
            for (i, row) in rows.iter().enumerate() {
                let y = area.y.saturating_add(i as u16);
                if y >= area.bottom() {
                    break;
                }
                f.render_widget(row, Rect { x: area.x, y, width: area.width, height: 1 });
            }
            // Park a VISIBLE cursor on the spinner row: ratatui hides the hardware
            // cursor on every frame that sets none, and only Drop restores it — so
            // a Ctrl-C mid-build (there is no SIGINT handler) would otherwise leave
            // the user's shell cursor invisible until `reset`.
            f.set_cursor_position(area.as_position());
        });
    }

    /// Emit a slice of pre-built lines into scrollback, above the viewport.
    fn push_before(&mut self, lines: &[Line<'_>]) {
        let Some(term) = self.term.as_mut() else { return };
        if lines.is_empty() {
            return;
        }
        let _ = term.insert_before(u16::try_from(lines.len()).unwrap_or(u16::MAX), |buf| {
            let max = buf.area.width;
            for (y, line) in lines.iter().enumerate() {
                buf.set_line(0, y as u16, line, max);
            }
        });
    }

    /// Persist a finished step (`✓` line + item sub-list) into scrollback, with
    /// the step's wall-clock duration right-aligned on the `✓` line. The live
    /// tail is ephemeral by design — success collapses it (BuildKit behavior).
    fn flush_step(&mut self, st: StepState) {
        let width = self.width();
        let color = self.color;
        let mut lines = Vec::with_capacity(1 + st.items.len());
        lines.push(step_line(&st, color, width));
        lines.extend(item_lines(&st.items, width));
        self.push_before(&lines);
    }

    /// Persist a FAILED step into scrollback: its red `✗` line plus the final
    /// live tail, so multi-minute compile context survives the viewport
    /// collapse (a vanished failed step is not acceptable at these durations).
    fn flush_failed(&mut self, mut st: StepState) {
        let width = self.width();
        let color = self.color;
        st.note = Some("failed".to_string());
        let mut lines = Vec::with_capacity(1 + st.tail.len());
        lines.push(failed_step_line(&st, color, width));
        for text in &st.tail {
            lines.push(tail_row(text, color, width));
        }
        self.push_before(&lines);
    }

    /// Remove the inline viewport and park the cursor on its reclaimed top
    /// row, so the shell prompt (or later output) doesn't sit under a stale
    /// spinner/tail. Idempotent — takes `term`, so a second call is a no-op.
    fn collapse(&mut self) {
        let Some(mut term) = self.term.take() else { return };
        self.current = None;
        // Blank the row, then clear it and anchor the cursor at its top-left
        // (mirrors indicatif's `finish_and_clear`: no leftover line).
        let _ = term.draw(|f| {
            let area = f.area();
            f.render_widget(pad_to_width(Line::raw(""), area.width as usize), area);
            f.set_cursor_position(area.as_position());
        });
        let area = term.get_frame().area();
        let _ = execute!(term.backend_mut(), MoveTo(area.x, area.y), Clear(ClearType::FromCursorDown));
        // `term` drops here; its cursor-visibility restore is now a no-op backstop
        // (the cursor is never hidden — every frame parks a visible cursor).
    }
}

/// The active-viewport bundle: the shared [`Renderer`] and the ticker thread
/// that animates it. Only present on a live TTY.
struct Live {
    inner: Arc<Mutex<Renderer>>,
    stop: Arc<AtomicBool>,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

impl Live {
    /// Build the inline viewport over stderr and start the ticker. Returns
    /// `None` if the terminal can't be initialized (e.g. no controlling tty),
    /// so the caller falls back to the frozen/plain path. The viewport height
    /// (spinner row + tail rows) is decided HERE, once — the inline viewport
    /// cannot resize afterwards.
    fn start(color: bool) -> Option<Live> {
        let tail_rows = ratatui::crossterm::terminal::size()
            .map(|(_, h)| h.saturating_sub(4).min(MAX_TAIL_ROWS))
            .unwrap_or(MAX_TAIL_ROWS);
        let backend = CrosstermBackend::new(std::io::stderr());
        let opts = TerminalOptions { viewport: Viewport::Inline(1 + tail_rows) };
        let term = Terminal::with_options(backend, opts).ok()?;
        let inner = Arc::new(Mutex::new(Renderer {
            term: Some(term),
            current: None,
            frame: 0,
            color,
            tail_rows,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = spawn_ticker(Arc::clone(&inner), Arc::clone(&stop));
        Some(Live { inner, stop, ticker: Mutex::new(Some(ticker)) })
    }

    /// Lock the renderer, shrugging off poisoning: this is render state only,
    /// and a panicked bake thread must not be able to wedge the UI.
    fn lock(&self) -> MutexGuard<'_, Renderer> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Signal the ticker to stop and join it. Idempotent (the handle is taken).
    /// Called BEFORE [`Renderer::collapse`] so no redraw races the teardown.
    fn stop_ticker(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let handle = self.ticker.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// The animation loop: wake every [`TICK`], and while running advance the
/// spinner and redraw the live region.
fn spawn_ticker(inner: Arc<Mutex<Renderer>>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(TICK);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let mut r = inner.lock().unwrap_or_else(PoisonError::into_inner);
            r.frame = r.frame.wrapping_add(1);
            r.redraw();
        }
    })
}

/// One stepped inline viewport, or a no-op (plain-line fallback). Every method is
/// safe to call regardless of whether the viewport is active; `Drop` collapses
/// it as a belt-and-suspenders backstop for early returns.
pub struct Progress {
    /// `Some` iff the inline TUI is live; `None` in frozen/non-TTY/disabled mode.
    live: Option<Live>,
    /// Wall-clock start, for the final summary's `time` field.
    start: Instant,
}

/// Decide ONCE whether the viewport should be active: a real stderr terminal, no
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
    /// The `tdvmm build` constructor: the inline viewport is active iff
    /// [`enabled`] says so AND the terminal initializes; otherwise frozen.
    pub fn new(no_progress_flag: bool) -> Progress {
        let start = Instant::now();
        if !enabled(no_progress_flag) {
            return Progress { live: None, start };
        }
        let color = std::env::var_os("NO_COLOR").is_none();
        Progress { live: Live::start(color), start }
    }

    /// Always-disabled instance: for commands that share bake-pipeline helpers
    /// (`build-agent`, `build-kernel`) but must NEVER show a viewport — the
    /// progress UI is `build`-only (scope lock). Its `println` still falls
    /// back to plain `eprintln!`, so those commands' output is unchanged.
    pub fn disabled() -> Progress {
        Progress { live: None, start: Instant::now() }
    }

    /// Whether the viewport is actually showing (used by the orchestrator to
    /// pick the `engine::OutputMode`).
    pub fn active(&self) -> bool {
        self.live.is_some()
    }

    /// Wall-clock time since construction (the final summary's `time` field).
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// The TTY-only title line (`tdvmm build  <stack>` + a blank line), pushed
    /// once into scrollback before the first step. A no-op in frozen mode — the
    /// plain output has no equivalent line.
    pub fn title(&self, stack: &str) {
        if let Some(live) = &self.live {
            let title = format!("tdvmm build  {stack}");
            live.lock().push_before(&[Line::from(title), Line::raw("")]);
        }
    }

    /// `[i/n] label` — starts a new step's spinner. Frozen/non-TTY: a no-op,
    /// exactly as before (the label/step-count are never part of the plain
    /// output). TTY: first PERSISTS the previous step's `✓` line (+ its note /
    /// item sub-list, if any) into scrollback, THEN resets the live region to the
    /// new step's spinner and per-step elapsed clock.
    pub fn step(&self, i: u32, n: u32, label: &str) {
        let Some(live) = &self.live else { return };
        let mut r = live.lock();
        if let Some(prev) = r.current.take() {
            r.flush_step(prev);
        }
        r.current = Some(StepState {
            i,
            n,
            label: label.to_string(),
            note: None,
            items: Vec::new(),
            tail: VecDeque::new(),
            start: Instant::now(),
        });
        r.frame = 0;
        r.redraw();
    }

    /// Run `f` on the CURRENT step's state under the renderer lock; a no-op in
    /// frozen mode or between steps.
    fn with_current(&self, f: impl FnOnce(&mut StepState)) {
        if let Some(live) = &self.live {
            if let Some(st) = live.lock().current.as_mut() {
                f(st);
            }
        }
    }

    /// Replace the CURRENT step's spinner label (e.g. `guest kernel` →
    /// `guest kernel · compiling 6.1.128 (first run)` once a compile actually
    /// starts). TTY-only; a no-op in frozen mode.
    pub fn relabel(&self, label: impl Into<String>) {
        self.with_current(|st| st.label = label.into());
    }

    /// One streamed line of the CURRENT step's child output (the live build
    /// tail). Pushes into the step's bounded ring under the renderer lock and
    /// returns WITHOUT redrawing — the ticker paints the next frame, so
    /// thousands of lines/sec (a kernel `make -j`) coalesce into at most ~8
    /// repaints/sec, and every terminal write still happens under the one
    /// renderer Mutex (scrollback pushes, warnings, and the tail can never
    /// interleave corruptly). Frozen/non-TTY: a no-op — those steps' children
    /// inherit the terminal, keeping the frozen byte contract.
    pub fn tail_line(&self, line: &str) {
        let Some(live) = &self.live else { return };
        let mut r = live.lock();
        let cap = r.tail_rows as usize;
        if cap == 0 {
            return;
        }
        if let Some(st) = r.current.as_mut() {
            if st.tail.len() == cap {
                st.tail.pop_front();
            }
            st.tail.push_back(line.to_string());
        }
    }

    /// A short inline note for the CURRENT step's `✓` line (e.g. `miss → full
    /// bake`), for single-fact steps. TTY-only; a no-op in frozen mode.
    pub fn note(&self, text: impl Into<String>) {
        self.with_current(|st| st.note = Some(text.into()));
    }

    /// One entry (`name`, size in MiB) in the CURRENT step's aligned indented
    /// sub-list (e.g. per-image pull/build results) — rendered under the `✓`
    /// line once the step finishes. TTY-only; a no-op in frozen mode. Safe from
    /// the parallel bake workers (guarded by the renderer lock).
    pub fn item(&self, name: impl Into<String>, size_mib: u64) {
        self.with_current(|st| st.items.push((name.into(), size_mib)));
    }

    /// One chrome line that must ALWAYS surface — warnings, gate failures,
    /// errors (Fable CLI-UX ruling: routine detail may be demoted, these never
    /// are). With the viewport active it inserts the (width-clamped) lines into
    /// scrollback above the live region; disabled, it is `eprintln!` verbatim —
    /// byte-for-byte what today's plain build prints (non-TTY output is FROZEN).
    pub fn println(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        match &self.live {
            Some(live) => {
                let mut r = live.lock();
                // Post-`finish()` the viewport is gone (`term` taken); don't silently
                // drop a late warning — fall back to stderr like the frozen path.
                if r.term.is_none() {
                    eprintln!("{text}");
                    return;
                }
                let width = r.width();
                let lines: Vec<Line> = if text.is_empty() {
                    vec![Line::raw("")]
                } else {
                    text.lines().map(|l| Line::from(clamp(l, width))).collect()
                };
                r.push_before(&lines);
                // Repaint immediately: `insert_before` (no scrolling-regions) clears
                // the live row, which would otherwise sit blank until the next tick.
                r.redraw();
            }
            None => eprintln!("{text}"),
        }
    }

    /// One ROUTINE chrome line (the old wall-of-detail): frozen/non-TTY prints
    /// it verbatim via `eprintln!` (byte-identical to before). TTY: suppressed
    /// entirely — `build.rs` relocates anything informative here into the
    /// diagnostics file instead.
    pub fn detail(&self, text: impl AsRef<str>) {
        if self.live.is_none() {
            eprintln!("{}", text.as_ref());
        }
    }

    /// Flush the LAST step's `✓` line and blank the live region (Fix: the
    /// counter must never leave a stale spinner frame on screen). Idempotent;
    /// a no-op in frozen/disabled mode. Always call this BEFORE any final
    /// summary — never rely on tick timing.
    pub fn finish_steps(&self) {
        if let Some(live) = &self.live {
            let mut r = live.lock();
            if let Some(st) = r.current.take() {
                r.flush_step(st);
            }
            r.redraw();
        }
    }

    /// The aligned final summary block (`built` / `sha256` / `size` / `time` +
    /// an optional `details` pointer). Calls [`Progress::finish_steps`] first,
    /// so the last checkmark lands before anything below it prints. TTY-only —
    /// a no-op in frozen mode, so `build.rs` can call it unconditionally.
    pub fn print_summary(&self, out_path: &Path, sha256: &str, size_bytes: u64, elapsed: Duration, diagnostics: Option<&Path>) {
        let Some(live) = &self.live else { return };
        self.finish_steps();
        let mut lines = vec![
            String::new(),
            format!("  built  {}", display_path(out_path)),
            format!("         sha256  {}", short_sha(sha256)),
            format!("         size    {}", human_size(size_bytes)),
            format!("         time    {}", human_duration(elapsed)),
        ];
        if let Some(d) = diagnostics {
            lines.push(String::new());
            lines.push(format!("  details  {}", display_path(d)));
        }
        let mut r = live.lock();
        let width = r.width();
        let rendered: Vec<Line> = lines.iter().map(|l| Line::from(clamp(l, width))).collect();
        r.push_before(&rendered);
    }

    /// Run `f` (arbitrary I/O — typically a plain `println!` to STDOUT, which
    /// this module never touches otherwise) while holding the renderer lock, so
    /// the ticker can't redraw the stderr region concurrently. A no-op wrapper
    /// (just calls `f`) when the viewport is inactive.
    ///
    /// `f` MUST NOT call back into any `Progress` method (the renderer lock is
    /// held and `std::sync::Mutex` is not reentrant → self-deadlock) nor write to
    /// the stderr TTY (it would garble the live row). Today's callers only write
    /// the stdout identity line, which is safe.
    pub fn suspend<F: FnOnce() -> R, R>(&self, f: F) -> R {
        match &self.live {
            Some(live) => {
                let _guard = live.lock();
                f()
            }
            None => f(),
        }
    }

    /// Collapse the viewport for good (idempotent — safe to call more than once,
    /// e.g. once explicitly on the cache-hit path and again via `Drop`). Stops
    /// the ticker first, then reclaims the live rows. A step still pending here
    /// did NOT succeed — success paths flush it via [`Progress::step`] /
    /// [`Progress::finish_steps`] first — so it persists into scrollback as a
    /// red `✗` line plus its final tail (never a `✓`), which is how a failed
    /// multi-minute compile keeps its context on every `?`-propagated error
    /// path (`Drop` runs this).
    pub fn finish(&self) {
        if let Some(live) = &self.live {
            live.stop_ticker();
            let mut r = live.lock();
            if let Some(st) = r.current.take() {
                r.flush_failed(st);
            }
            r.collapse();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

// ============================================================================
// Line building + compact formatting. Non-TTY/frozen output never goes through
// any of these (Fable guardrail — see the module doc comment).
// ============================================================================

/// The live viewport row: `  ⠙ [i/n] label … 12s`, spinner glyph + counter
/// cyan-bold (the old `{spinner:.cyan.bold} {prefix:.cyan.bold} {msg} …
/// {elapsed}` template, indented to align its glyph with the `✓` column).
fn spinner_line(glyph: &'static str, st: &StepState, color: bool) -> Line<'static> {
    let accent = if color {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let head = format!("[{}/{}]", st.i, st.n);
    let tail = format!(" {} … {}", st.label, human_duration(st.start.elapsed()));
    Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph, accent),
        Span::raw(" "),
        Span::styled(head, accent),
        Span::raw(tail),
    ])
}

/// The persistent `  ✓ [i/n] label [note]` line (see [`finished_line`]).
fn step_line(st: &StepState, color: bool, width: usize) -> Line<'static> {
    finished_line("\u{2713}", Color::Green, st, color, width)
}

/// The persistent `  ✗ [i/n] label failed` line for a step pending at collapse.
fn failed_step_line(st: &StepState, color: bool, width: usize) -> Line<'static> {
    finished_line("\u{2717}", Color::Red, st, color, width)
}

/// A persistent `  <mark> [i/n] label [note]` line, with `dur` right-aligned to
/// `width` (buildkit-style). The 4 fixed leading columns (`"  "` + the
/// 1-visible-column mark + `" "`) plus the clamped, colorless remainder never
/// overflow `width`; only the remainder is clamped, so the colored mark is never
/// split. `dur` is reserved its own width plus a one-space minimum gap flush at
/// the right edge; when the remainder is shorter, the gap grows so `dur` still
/// lands at column `width`. Narrow terminals degrade gracefully: the remainder
/// shrinks to whatever remains, and if even the mark + gap + `dur` won't fit,
/// `dur` is dropped rather than wrapping the line.
fn finished_line(
    glyph: &'static str,
    glyph_color: Color,
    st: &StepState,
    color: bool,
    width: usize,
) -> Line<'static> {
    const PREFIX: usize = 4;
    // The note column is fixed (matches the mockup): long enough for the longest
    // step label ("compose.lock + binds", 21 chars) plus a gap.
    const LABEL_COL: usize = 22;
    let plain = match &st.note {
        Some(note) => format!("[{}/{}] {:<LABEL_COL$}{note}", st.i, st.n, st.label),
        None => format!("[{}/{}] {}", st.i, st.n, st.label),
    };
    let mark = if color {
        Span::styled(glyph, Style::new().fg(glyph_color))
    } else {
        Span::raw(glyph)
    };
    let dur = human_duration(st.start.elapsed());
    let dur_len = dur.chars().count();
    let left_budget = width.saturating_sub(dur_len + 1);
    if left_budget <= PREFIX {
        let body = clamp(&plain, width.saturating_sub(PREFIX));
        return Line::from(vec![Span::raw("  "), mark, Span::raw(" "), Span::raw(body)]);
    }
    let body = clamp(&plain, left_budget - PREFIX);
    let gap = width.saturating_sub(PREFIX + body.chars().count() + dur_len).max(1);
    Line::from(vec![
        Span::raw("  "),
        mark,
        Span::raw(" "),
        Span::raw(body),
        Span::raw(" ".repeat(gap)),
        Span::raw(dur),
    ])
}

/// One dim, indented live-tail row (streamed child output under the spinner,
/// or the final tail lines kept under a failed step's `✗`).
fn tail_row(text: &str, color: bool, width: usize) -> Line<'static> {
    let body = clamp(&format!("      {text}"), width);
    if color {
        Line::from(Span::styled(body, Style::new().fg(Color::DarkGray)))
    } else {
        Line::from(body)
    }
}

/// The aligned indented sub-list under a finished step (`            name  size`,
/// e.g. per-image pull/build results), each line clamped to `width`.
fn item_lines(items: &[(String, u64)], width: usize) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }
    let name_w = items.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0) + 4;
    let vals: Vec<String> = items.iter().map(|(_, mib)| human_size_short(*mib)).collect();
    let val_w = vals.iter().map(|v| v.chars().count()).max().unwrap_or(0);
    items
        .iter()
        .zip(vals.iter())
        .map(|((name, _), val)| {
            let line = format!("            {name:<name_w$}{val:>val_w$}");
            Line::from(clamp(&line, width))
        })
        .collect()
}

/// Pad `line` on the right with spaces to fill `width` display columns, so a
/// redraw fully overwrites its row (a bare `Line` writes only its own cells).
fn pad_to_width(mut line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    if used < width {
        line.push_span(Span::raw(" ".repeat(width - used)));
    }
    line
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
