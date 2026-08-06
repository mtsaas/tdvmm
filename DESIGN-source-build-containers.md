# Design — build the kernel and agent from source in pinned containers

**Branch:** `feat/standalone-build-assets` (on top of Phase 2, b89b14f)
**Status:** DESIGN ONLY — nothing implemented. Fable design, 2026-08-05.

Owner ruling: precompiled artifacts are a security risk — the agent runs
privileged inside the guest and the kernel *is* the guest. So `tdvmm` must
compile **both** from source inside pinned containers and extract the results.
No prebuilt-binary downloads, no release-asset publishing. A good terminal UI
must show the container build progress live.

## 0. Engine assumption (flagged up front)

The owner said "Docker". The entire bake — pulls, squash, seed store,
`unshare` re-exec — already runs on **podman** through the single choke point
`src/engine.rs:19-25`, and the seed/assemble steps depend on `podman unshare`
(`src/engine.rs:44-48`), which has **no docker equivalent**. This design
therefore reads "Docker" as "containers" and builds on the existing podman
tooling. If the owner literally wants the docker CLI, that is a separate,
much larger project (replacing the rootless-unshare assembly model), not a
constant swap — decision point #2 in §8.

## 1. Reconciliation with committed Phase 2

The Phase-2 mechanism splits cleanly: everything "embed pins + cache + build
in a pinned container" stays; everything "fetch a prebuilt release asset"
goes.

### REMOVE (the download machinery)

| What | Where |
|---|---|
| Kernel release-asset fetch (the PRIMARY branch of `ensure_kernel`) | `src/build/kernel.rs:157-174` |
| `KernelLock.release_asset_url` / `release_asset_name` fields + parse/write | `src/build/kernel.rs:46-47,64-65,78-79,113-117` |
| `RELEASE_ASSET_URL` / `RELEASE_ASSET_NAME` lines + "fetch is primary" prose | `guest/kernel/kernel.lock:1-6,16-17` |
| Checkout requirement for the kernel **config** in the build path (config gets embedded, §3) | `src/build/kernel.rs:178-181,234` |
| Agent release-asset fetch + cache-by-release-version in `ensure_agent` | `src/build/agent.rs:130-158` |
| Agent checkout gate + "until the first release" error | `src/build/agent.rs:160-166` |
| From-source-vs-pin verify (no pin exists anymore) | `src/build/agent.rs:168-180` |
| `agent.lock` as a whole: embed, struct, parse, write, `--record --tag` flow | `src/build/agent.rs:18,24,28-85,262-287`; delete `tdvmm-agent/agent.lock` |
| `agent_cache_input`'s recorded-release branch (function simplifies, §2.4) | `src/build/agent.rs:107-115` |
| `--record` / `--tag` on `build-agent` | `src/cli.rs:173-181`, `src/build/mod.rs:127-136` |
| The agent release workflow | delete `.github/workflows/agent-release.yml` |
| `standalone_bake_test.sh` release assumptions: agent-gap logic, `EXPECT_AGENT_GAP`, the stale-kernel-asset seeding | `scripts/standalone_bake_test.sh:10-23,31-32,67-93,105-124` (rewritten, §7) |

Non-code cleanup: the stale `kernel-6.1.128` GitHub release asset (and the
"re-upload with `--clobber`" owner action) become moot — the release can be
deleted. `.github/workflows/release.yml` (the tdvmm *binary* release) stays.

### KEEP (build on this)

| What | Where |
|---|---|
| The reproducible kernel container build — becomes the only path | `src/build/kernel.rs:201-290` |
| Kernel sha-verify on cache hit and after every build | `src/build/kernel.rs:144-155,183-193` |
| `build-kernel --record` bootstrap (maintainer, checkout-only) | `src/build/kernel.rs:317-362` |
| The reproducible musl agent container build | `src/build/agent.rs:191-237` |
| `agent_src_id` — same construction, same value (source flips to the embedded set, §2.3) | `src/build/agent.rs:97-102` |
| Embedded pins: rootfs-builder, agent images.lock, compose-engine, kernel.lock | `src/build/pins.rs:17-32`, `src/build/kernel.rs:32` |
| Overlay embedding (static table, pinned modes) | `src/build/overlay.rs` |
| In-container fetch + sha verify | `src/build/pins.rs:83-120` |
| `self_here` → optional `find_guest_dir`/`find_repo_root` | `src/build/util.rs:24-43` |
| Phase-1 cache layout (`kernel/`, `kernel-src/`, `downloads/`, `ledgers/`, `artifacts/`, `bake/`, `base-runtime/`, `diagnostics/`) | `src/build/cache.rs`, `src/build/bake.rs:170-183` |
| `tdvmm-agent/images.lock` (builder pin, stays inside the hashed tree) | `tdvmm-agent/images.lock` |

### MODIFY

| What | Where |
|---|---|
| `ensure_kernel`: cache → container build → verify → cache (no fetch branch) | `src/build/kernel.rs:128-194` |
| `ensure_agent`: cache → build-from-embedded-source → verify sidecar → cache | `src/build/agent.rs:125-182` |
| Bake step structure: kernel + agent become visible steps; agent leaves the thread scope | `src/build/bake.rs:132-146,194,316,473-479`; `TOTAL_STEPS` `src/build/mod.rs:66` |
| `engine`: add a line-streaming runner (additive, UI-free) | `src/engine.rs` |
| `ui`: taller inline viewport with a bounded live tail | `src/ui.rs` |
| `CACHE_VERSION` 5 → 6 (key-semantics change; one cold miss) | `src/build/cache.rs:21` |
| New: root `build.rs` generating the embedded agent-source module | new file |

## 2. THE CRUX — agent source for a no-repo install

To build the agent from source with no checkout, the source must travel with
the binary. Options considered:

- **(a) Embed the agent source tree in the `tdvmm` binary** and materialize
  it into a scratch build context. **← RECOMMENDED.**
- (b) Pinned source tarball, fetched + sha-verified (kernel-source model).
  Rejected: tdvmm has no stable upstream tarball host — GitHub auto-generated
  archives are explicitly not byte-stable, so we would have to *publish* a
  tarball as a release asset, resurrecting exactly the publishing machinery,
  version-skew window, and chicken-and-egg the owner just cancelled. It also
  adds a network dependency for something we already have locally at compile
  time.
- (c) Require a checkout. Rejected — defeats standalone.

Why (a) wins: the source is ~300 KB (agent `src/` 96K + proto `src/` 44K +
proto goldens 112K + `Cargo.lock` 36K + manifests), trivial next to the
binary; there is zero network and zero publishing; and the tdvmm binary and
its agent source can never be version-skewed — they are one artifact. It is
the same move as the overlay embed, just bigger.

### 2.1 What gets embedded (the exact set)

The embedded set is **exactly the set `agent_src_id` hashes today**
(`src/build/agent.rs:97-102`), so the identity value is unchanged:

- `tdvmm-agent/` — `Cargo.toml`, `deps.allow`, `images.lock`, `src/*`
  (`agent.lock` is deleted, and was excluded from the hash anyway, so the
  hash is unaffected)
- `tdvmm-proto/` — `Cargo.toml`, `src/*`, `goldens/*` (goldens don't compile
  into the agent, but they are in today's `tree_hash` — embedding them keeps
  `agent_src_id`, and therefore `TDVMM_AGENT_BUILD` and the agent bytes,
  bit-identical; 112 KB is a fine price for holding the golden)
- Root `Cargo.lock`

Plus two materialize-only files (NOT hashed, preserving today's value):

- Root `Cargo.toml` verbatim — carries `[workspace]` and
  `[profile.agent-release]` (`Cargo.toml:69-76`). Embedding it *fixes* the
  documented hole that a checkout's locally-edited profile silently changed
  agent bytes: the build now always uses the embedded manifest.
- A stub `src/main.rs` (`fn main() {}`) — cargo refuses to load a workspace
  whose root `[[bin]]` target file is missing; the root package is never
  compiled (`-p tdvmm-agent`), so the stub affects nothing.

`.cargo/config.toml` is deliberately NOT embedded: the container build sets
the `RUSTFLAGS` env (`src/build/agent.rs:206-207`), which takes precedence
over config-file rustflags — true today (the repo mount includes it and it's
already inert) and after the pivot alike.

### 2.2 How it's embedded: root `build.rs` codegen

A hand-rolled static table (overlay-style) would be ~40 entries and rot-prone.
Instead the root crate gets a small `build.rs` that walks the fixed set above,
emits `$OUT_DIR/agent_src.rs` — a `&[(relpath, bytes)]` table of
`include_bytes!` entries — and prints `cargo:rerun-if-changed` for every file
and directory, so editing agent source recompiles tdvmm exactly like editing
an overlay file does. No new dependencies (`include_dir` is rejected: its
change-tracking is unreliable and it adds a proc-macro dep for something a
20-line build script does).

At bake time, `build_agent` materializes the table into a `ScratchDir` (the
existing guard, `src/build/util.rs:68-88`) and bind-mounts it at `/src:ro` in
place of today's repo mount (`src/build/agent.rs:218`). Same `/src` path,
same `--remap-path-prefix=/src=/tdvmm`, same env, same script — the compiler
sees byte-identical input in a byte-identical location, so the output is
byte-identical to today's checkout build. File modes/mtimes in the scratch
tree don't reach rustc output (fresh `CARGO_TARGET_DIR` per build already
forces full rebuilds).

`cargo build --locked` inside the container still resolves the full workspace
lock (root manifest + both member manifests are present and verbatim), so
`--locked` passes and crates.io deps are fetched with `Cargo.lock`'s sha256
checksums — the same integrity model as the kernel source pin (see §6 risks).

### 2.3 Identity: `agent_src_id`, `TDVMM_AGENT_BUILD`, the golden

`agent_src_id` keeps its exact construction (per-tree `tree_hash` relpaths +
`Cargo.lock` sha, first 16 hex) but computes from the **embedded** table, not
a checkout — it becomes infallible and checkout-independent. Because the
embedded set == today's hashed set, the value is unchanged, so
`TDVMM_AGENT_BUILD` (compiled into the agent bytes, `src/build/agent.rs:230`)
is unchanged, so the agent bytes are unchanged, so insert-trim's `2b86ab69`
golden is expected to hold (§6).

Tripwire unit test (mirrors the overlay drift test): when a checkout is
present, embedded-set `agent_src_id` == checkout-computed `agent_src_id`, and
the embedded file list == the checkout file list. Catches an embed-set
mistake (e.g. a forgotten new source file exclusion) the moment it happens.

### 2.4 Cache interaction

- `agent_cache_input()` (`src/build/agent.rs:107-115`) collapses to "return
  the embedded `agent_src_id`" — no lock branch, no checkout branch, no
  error path. The bake key's `agent:` line (`src/build/cache.rs:86`) keeps
  its meaning: the identity of the agent that ends up in the artifact.
- The **built agent binary** is cached once per machine (§4) under a key that
  covers everything that shapes its bytes — see there for why `agent_src_id`
  alone is not enough for the *artifact* cache.

## 3. Kernel — container build becomes primary (and only)

`ensure_kernel` (`src/build/kernel.rs:128-194`) becomes:

1. **Cache hit:** `<cache>/kernel/vmlinux-<version>` exists and sha256 ==
   embedded `KERNEL_SHA256` → done (unchanged, `kernel.rs:144-155`).
2. **Build:** `build_kernel_container` (`kernel.rs:201-290`, kept verbatim in
   shape): fetch `linux-<v>.tar.xz` from the pinned `KERNEL_SOURCE_URL` into
   `<cache>/kernel-src/`, sha-verified against `KERNEL_SOURCE_SHA256`
   (in-container wget, `pins.rs:83-120`); build vmlinux in the digest-pinned
   builder with the existing determinism knobs (`SOURCE_DATE_EPOCH`,
   `KBUILD_BUILD_*`, `KCONFIG_NOTIMESTAMP`, the gnu11 CC wrapper).
3. **Verify:** built sha must equal `KERNEL_SHA256` or hard-fail with the
   existing "not reproducing the recorded kernel" error (`kernel.rs:184-191`).
4. Result lands in `<cache>/kernel/`, sha-checked on every later use.

**The config gets embedded.** Phase 2 left `microvm-kernel-x86_64-6.1.config`
(92 KB) as a checkout file because the fetch was primary; with build primary
it is a required standalone input. `include_str!` it beside the kernel.lock
embed (`kernel.rs:28-32`), write it into the build scratch dir, and mount
that instead of `guest_dir.join("kernel")` (`kernel.rs:234,268`). Runtime
tripwire: embedded config sha must equal the embedded lock's
`KERNEL_CONFIG_SHA256` or fail loudly with "re-run `tdvmm build-kernel
--record`". `--record` keeps reading/writing the *checkout* files
(`kernel.rs:323-361`) — the on-disk lock+config are the source of truth being
edited; a rebuild embeds them.

**External inputs, complete list:** (1) the kernel source tarball —
sha-pinned, verified before use; (2) the builder image — digest-pinned;
(3) the toolchain packages `apt-get install`ed inside the builder
(`kernel.rs:243-244`) — **not pinned** (bookworm point-release drift is
possible). This is pre-existing, not new. It cannot *silently* corrupt
anything — a drifted gcc produces a different vmlinux and step 3 hard-fails
against the pin — but it can *break first builds* until the owner re-records.
Mitigation now: the verify gate + a clear error. Hardening later (owner
decision #4): bake a dedicated kernel-builder image with the toolchain
preinstalled and pin *its* digest; that removes apt from the build entirely.

**Cost:** first kernel build ≈ tarball download (~140 MB) + apt (~100 MB) +
a `make -j vmlinux` of the microvm config — single-digit minutes on a typical
dev box. Once per machine per kernel pin; every later bake is a sha-verified
cache hit. `tdvmm boot`'s default-kernel path (`kernel.rs:385-397`) inherits
this: with a cold cache it now compiles instead of fetching (raw inherited
output — the tail UI is `tdvmm build`-scoped, per the existing scope lock).
The deferred `tdvmm doctor` becomes the natural pre-warmer: its "download
dependencies" half becomes "pre-build kernel + agent into the cache".

## 4. Caching

Both artifacts build once per machine and are reused by every bake, keyed by
content, in the Phase-1 cache layout:

| Artifact | Path | Key / gate |
|---|---|---|
| kernel | `<cache>/kernel/vmlinux-<version>` | embedded `KERNEL_SHA256` verified on every use. The pin *is* the content key: version, config sha, source sha, and builder digest are all folded into it at `--record` time, so any input change re-records a new expected sha and a stale cache entry self-heals via the mismatch → rebuild path (`kernel.rs:148-155`). |
| kernel source | `<cache>/kernel-src/linux-<v>.tar.xz` | `KERNEL_SOURCE_SHA256` (existing) |
| agent | `<cache>/agent/tdvmm-agent-<build_key16>` + `.sha256` sidecar | `build_key` below; sidecar verified on every reuse |
| agent crate downloads | none persisted — `CARGO_HOME` stays per-build scratch (hermetic); the built-agent cache makes the cold fetch a once-per-machine event | `Cargo.lock` checksums |

**Agent `build_key`** — this is new and load-bearing. The agent has no
recorded pin anymore, so a cached binary can only be trusted if its key
covers *everything* that shapes its bytes. `agent_src_id` alone does not:
the profile lives in the root manifest (not hashed, by design — §2.3) and
the RUSTFLAGS/build script live in tdvmm code. So:

```
build_key = sha256(
    "tdvmm-agent-build v1\n"
  + agent_src_id + "\n"                 # embedded source set (incl. images.lock = builder digest)
  + sha256(embedded root Cargo.toml)    # [profile.agent-release] + workspace shape
  + sha256(RUSTFLAGS string) + sha256(container script) + BUILD_EPOCH
)[..16]
```

All inputs are compile-time constants or embedded data — computable with no
I/O. A profile edit, flag change, or builder bump each miss the cache and
rebuild; nothing can serve stale bytes. (The *bake* key keeps its `agent:`
line = `agent_src_id` for legibility; the `self:` line — the tdvmm binary
sha, which now embeds the manifest, flags, and source — already covers the
rest, `src/build/cache.rs:62,80-93`.)

`ensure_agent` becomes: sidecar-verified cache hit → materialize + container
build → write binary + sidecar to `<cache>/agent/` → return
`(path, agent_src_id)`. No checkout, no fetch, no failure mode besides "the
container build failed".

`CACHE_VERSION` 5 → 6 (`cache.rs:21`): the `agent:` line's semantics change
(always source-id, never a release sha). One cold miss, matching precedent.

## 5. THE TERMINAL UI (headline)

### 5.1 What the user sees

Step list grows from 8 to 10 (`TOTAL_STEPS`, `src/build/mod.rs:66`): the two
compiles become first-class steps instead of hiding inside "resolve inputs"
and the mid-bake thread scope.

```
1 resolve inputs · 2 guest kernel · 3 bake cache · 4 guest agent
5 pull + build images · 6 seed store · 7 compose.lock + binds
8 assemble initramfs · 9 pack artifact · 10 cache
```

Order matters: the kernel must exist before the bake key (its bytes are
hashed, `cache.rs:63`), and a bake-cache HIT still exits at step 3 without
ever touching the agent — the hit path stays as fast as today.

**During a first-run kernel compile** (live region = spinner row + a dim,
bounded tail of the container's output, BuildKit/cargo style):

```
tdvmm build  shop-backend

  ✓ [1/10] resolve inputs                                          0.3s
  ⠸ [2/10] guest kernel · compiling 6.1.128 (first run) …        4m07s
      CC      net/ipv4/tcp_input.o
      CC      net/ipv4/tcp_output.o
      CC      drivers/virtio/virtio_ring.o
      CC      drivers/virtio/virtio_mmio.o
      AR      drivers/built-in.a
      LD      vmlinux.o
```

The tail scrolls live (apt lines, then tar, then thousands of `CC` lines —
the raw output is the progress). On completion the tail vanishes and the step
collapses into scrollback exactly like every other step:

```
  ✓ [2/10] guest kernel         compiled + sha verified          9m41s
  ✓ [3/10] bake cache           miss → full bake                  0.1s
  ⠴ [4/10] guest agent · compiling (musl, first run) …             48s
      Compiling serde v1.0.219
      Compiling serde_json v1.0.132
      Compiling tdvmm-proto v0.1.0 (/tdvmm/tdvmm-proto)
      Compiling tdvmm-agent v0.1.0 (/tdvmm/tdvmm-agent)
```

Warm cache (every bake after the machine's first):

```
  ✓ [2/10] guest kernel         cached (sha verified)             0.1s
  ✓ [4/10] guest agent          cached (sha verified)             0.0s
```

**On failure**, the step persists as a red `✗` line plus its final tail
lines (so context survives the viewport collapse), then the error with a
bounded excerpt and a pointer to the full log:

```
  ✗ [2/10] guest kernel — build failed                           2m13s
      make[1]: *** [scripts/Makefile.build:250: net/core/dev.o] Error 1
      make: *** [Makefile:1992: net] Error 2
error: kernel container build failed (exit 2)
  last 100 lines shown above; full log: ~/.tdvmm/diagnostics/kernel-build-6.1.128.log
```

### 5.2 Streaming: a third child-output mode, kept out of the UI's way

`engine::OutputMode` (`src/engine.rs:58-95`) stays as-is (`Inherit`,
`CaptureOnFailure` are Copy and everywhere). Add one **additive, UI-free**
function to the choke point:

```rust
// src/engine.rs
pub fn run_streamed(
    cmd: &mut Command,
    sink: &(dyn Fn(&str) + Sync),          // called once per output line
) -> Result<String, StreamError>            // Ok(full transcript) / Err{transcript, status}
```

Spawn with both stdio pipes; two reader threads (stdout, stderr) `BufRead`
lines (trimming `\r`), each line goes to (1) the shared transcript buffer and
(2) the sink. Pipes are fully drained (no deadlock), line order is
best-effort interleaved — fine for a log tail. `engine` still knows nothing
about ratatui: the sink is an opaque callback, honoring the module rule
stated at `src/ui.rs:35-38` and `src/engine.rs:50-57`.

`ux.rs` gains the dispatcher the two build sites call:

```rust
// src/build/ux.rs
pub(super) fn run_build(cmd, ux, log_name) -> Result<(), Box<dyn Error>>
```

- viewport active → `engine::run_streamed(cmd, |l| ux.progress.tail_line(l))`;
  transcript is written to `<cache>/diagnostics/<log_name>.log` on success
  *and* failure (a once-per-machine compile log is cheap and gold); on
  failure the returned error carries the last 100 lines + the log path
  (nothing is swallowed — the full bytes are on disk at a printed path,
  which beats dumping a 3 MB make log to the terminal).
- viewport inactive → `engine::run(cmd, Inherit)` — **byte-identical to
  today's frozen non-TTY behavior** for these steps (they already inherit).

Only `ensure_kernel`/`build_agent` under `tdvmm build` use `run_build`; every
other bake child keeps `CaptureOnFailure`/`Inherit` exactly as now. The
standalone `build-kernel`/`build-agent` verbs keep their plain inherited
passthrough (`Ux::inherit`, the existing "progress is build-only" scope lock,
`agent.rs:255-257`, `kernel.rs:320-321`).

### 5.3 ratatui integration (the single-Mutex renderer stays single)

Changes to `src/ui.rs`, all inside the existing architecture:

- **Viewport height:** `Viewport::Inline(1)` (`ui.rs:189`) →
  `Inline(1 + TAIL_ROWS)`, `TAIL_ROWS = min(6, term_height.saturating_sub(4))`,
  decided once at `Live::start`. ratatui's inline viewport cannot resize
  after creation, and tearing the terminal down mid-build to grow it is
  exactly the corruption we must avoid — so the height is fixed and unused
  tail rows render blank (padded, `pad_to_width`) outside the compile steps.
  Cost: the live region is a few rows taller for the whole build; the cursor
  still parks on the spinner row and `collapse()`'s
  `Clear(FromCursorDown)` (`ui.rs:169`) already reclaims the whole region.
- **State:** `StepState` gains `tail: VecDeque<String>` capped at
  `TAIL_ROWS` (display buffer only — the full log lives in the transcript,
  not the UI).
- **`Progress::tail_line(&self, line)`** (new): lock the renderer, push +
  pop the ring, **do not redraw**. The 120 ms ticker (`ui.rs:216-228`)
  repaints the whole region on its next tick. This is the key integrity
  property: a kernel `make -j` emits thousands of lines/sec, and per-line
  redraws would thrash; with ticker-only painting the terminal sees at most
  ~8 coalesced frames/sec no matter the line rate, and every terminal write
  still happens under the one renderer Mutex, so `insert_before` scrollback
  pushes, `println` warnings, and the tail can never interleave corruptly.
- **`Renderer::redraw`** (`ui.rs:107-128`): row 0 = spinner line (unchanged);
  rows 1..=TAIL_ROWS = the ring's lines, indented, dim (`Color::DarkGray`,
  plain when `NO_COLOR`), clamped to width, blank-padded.
- **Step transitions** (`Progress::step`, `ui.rs:311-322`): flushing the
  previous step persists only its `✓`/note/items lines — the tail is
  ephemeral by design (BuildKit behavior: success collapses to one line).
- **`Progress::fail_step(&self)`** (new, small): flush the current step as a
  red `✗` line *plus its current tail* into scrollback. `cmd_build` calls it
  on its error path before propagating (today a failed step just vanishes,
  `ui.rs:445-448` — with multi-minute compiles that's no longer acceptable).
  `finish()`'s no-checkmark-on-failure rule is preserved: `✗` is not `✓`.
- **Frozen/non-TTY mode:** `tail_line` no-ops (like `step`/`note`/`item`);
  output for these steps is `Inherit` — the frozen byte contract holds.

### 5.4 What the bake orchestrator changes

- `bake.rs:132-146`: `ensure_kernel` moves out of step 1 into its own
  `step(2, …, "guest kernel")`; hit/miss sets the note ("cached (sha
  verified)" / "compiled + sha verified").
- `bake.rs:316,473-479`: the agent leaves the thread scope. It becomes the
  serial `step(4, …, "guest agent")` *before* the scope; `MidBake`
  (`bake.rs:51-61`) drops its agent fields; the minirootfs/compose download
  threads stay overlapped. Trade-off, stated honestly: on a **cold agent
  cache** the agent build no longer overlaps the image pulls (it used to run
  alongside steps 3-5). On a warm cache — every bake after the machine's
  first — the cost is zero, and the win is a coherent tail (one live build at
  a time, no interleaved writers) plus a simpler pipeline. Cold-path wall
  time is dominated by the kernel compile anyway.

## 6. Reproducibility

**The claim: insert-trim's `2b86ab69` golden holds.** The `.tdvmm` bytes are
a function of (kernel bytes, agent bytes, everything else Phase 2 already
pinned). The kernel path *is* today's fallback path, verified against the
same `KERNEL_SHA256=19506f47…`; the build refuses any other output. The agent
build has the same source bytes (embedded set == checkout set, tripwire
test), the same `TDVMM_AGENT_BUILD` (same `agent_src_id` construction over
the same set), the same builder digest, flags, profile, epoch, and the same
`/src` mount path for the remap. Nothing else in the bake changes. If the
golden moves, something is wrong — that is the gate doing its job (exactly
how the Phase-2 `cd926acb` mistake was caught).

**Gates, run at every phase boundary (§7):**
- `scripts/artifact_test.sh` (cold==warm + corpus) and
  `scripts/bake_repeat_test.sh` — `.tdvmm` byte-identity, must emit `2b86ab69`
  for insert-trim.
- `scripts/agent_double_build.sh` — two fresh-container builds byte-identical
  (now exercising the embedded-source materialization path, since
  `build-agent` builds from the embed).
- The cpio golden unit tests + the two overlay tests.
- New unit tests: embedded-vs-checkout agent-source identity (§2.3); embedded
  kernel config sha == lock's `KERNEL_CONFIG_SHA256`.
- The rewritten `standalone_bake_test.sh` (§7): a full bake must now succeed
  from an installed binary in an empty cwd — the agent gap is gone.
- No new kernel double-build script: every kernel build is already gated
  against the recorded cross-machine pin, which is strictly stronger.

**Determinism knobs preserved verbatim:** `SOURCE_DATE_EPOCH=BUILD_EPOCH`,
`KBUILD_BUILD_{TIMESTAMP,USER,HOST}`, `KCONFIG_NOTIMESTAMP`
(`kernel.rs:260-282`); `RUSTFLAGS` remaps + `--build-id=none`, scratch
`CARGO_HOME`/`CARGO_TARGET_DIR` (`agent.rs:206-232`).

**Risk register (all fail loud, none can drift silently):**

| Risk | Detection | Note |
|---|---|---|
| Debian toolchain drift in the kernel builder (apt not pinned) | built sha ≠ `KERNEL_SHA256` → hard fail + re-record instructions | pre-existing; hardening = dedicated builder image (owner #4) |
| crates.io outage / yanked crate on first agent build | cargo `--locked` fails; checksums in `Cargo.lock` prevent substitution | once per machine; vendoring the deps into the embed (~MBs) is possible later if the owner wants full offline builds |
| cdn.kernel.org outage on first kernel build | fetch fails, sha-verified when it succeeds | once per machine |
| Profile/flag edits reusing a stale cached agent | impossible — the agent `build_key` covers manifest + flags (§4) | closes a hole the old `agent.lock` note only documented |

## 7. Phasing (each phase gated by the full byte-identity suite, report at each boundary)

- **Phase A — kernel build-primary.** Embed the config + sha tripwire; strip
  the release-fetch branch and `RELEASE_ASSET_*` from code and
  `guest/kernel/kernel.lock`; `ensure_kernel` = cache → build → verify.
  Small, self-contained, immediately closes the stale-asset issue.
  Gate: full suite (kernel bytes must still be `19506f47…`, golden holds).
- **Phase B — agent source embed + build-primary.** Root `build.rs` codegen;
  materialization; `ensure_agent` rewrite + `<cache>/agent/<build_key>`;
  delete `agent.lock`, the `--record --tag` flow, and `agent-release.yml`;
  simplify `agent_cache_input`; `CACHE_VERSION` 6; identity unit tests.
  Gate: full suite + `agent_double_build` + the self-consistency check that
  `build-agent` from the embed reproduces build `a14c50c8…` / the golden's
  agent bytes.
- **Phase C — the build-progress UI.** `engine::run_streamed`; `ui.rs` tail
  region + `tail_line` + `fail_step`; step restructure 8 → 10; agent
  de-overlap; transcripts to `<cache>/diagnostics/`. Gate: full suite (UI
  must not move a byte — non-TTY output for the two steps stays Inherit) +
  eyeball runs of cold/warm/failure paths.
- **Phase D — acceptance + docs.** Rewrite `standalone_bake_test.sh`: full
  standalone bake must SUCCEED (no `EXPECT_AGENT_GAP`, no asset-staleness
  branch). Pragmatics: by default it may seed the fresh cache's *kernel* from
  an existing sha-verified copy (default cache or checkout — verified bytes,
  nothing faked) so the test isn't a 10-minute kernel compile per run, with
  `FULL_COLD=1` forcing the true cold path for occasional/CI-nightly runs.
  Update HANDOFF + README/GETTING_STARTED first-build expectations ("first
  build compiles a kernel — minutes; cached afterwards"). The pre-existing
  Phase 3 (`guest/` → `testdata/` rename) lands after, unchanged.

## 8. Owner decision points

1. **Agent source approach (§2)** — recommendation: **embed** (option a).
   Sign-off requested; it is the load-bearing choice.
2. **Podman vs literal docker (§0)** — recommendation: stay on podman.
   The bake's unshare-based assembly has no docker equivalent; "docker
   support" would be a separate project, not part of this change.
3. **Delete `agent.lock` entirely (§1)** — recommendation: yes. With source
   as truth there is no independent sha to record; the `.tdvmm` golden +
   double-build gates already pin the bytes. Keeping an empty ceremony file
   is complexity with no consumer.
4. **Kernel builder toolchain pinning (§3/§6)** — recommendation: defer.
   The sha gate makes drift loud, not silent; a dedicated pinned builder
   image is a clean later hardening if a drift-breakage ever actually bites.
5. **First-build cost (§3)** — accept that a cold machine's first bake
   compiles a kernel (single-digit minutes) + the agent (~1 min). The cache
   makes it once per machine; `tdvmm doctor` (already queued) becomes the
   explicit pre-warmer. This is the direct price of the no-prebuilt-binaries
   ruling and the UI in §5 is what makes it acceptable.
6. **Agent step serialization (§5.4)** — recommendation: serialize (UI
   coherence + simpler pipeline; zero cost warm). Flagging because it trades
   a little cold-path parallelism.
