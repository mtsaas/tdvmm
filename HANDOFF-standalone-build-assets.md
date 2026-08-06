# Handoff — make `tdvmm build` work from an installed binary (assets → cache, `guest/` → `testdata/`)

**Branch:** `feat/standalone-build-assets` (off `main`)
**Status:** Phase 2 complete + committed (b89b14f). **DIRECTION CHANGE (owner,
2026-08-05) — IMPLEMENTED + verified, see "Direction change result" below.**
Phase 3 (rename/docs) not started.

## ⚠ DIRECTION CHANGE — build from source in containers, no precompiled artifacts
Owner ruling (2026-08-05): **precompiled artifacts are a security risk**, especially
run privileged in the VM. So the CLI must **compile the kernel and the tdvmm-agent
from source in containers, then extract the artifacts** — do NOT download prebuilt
binaries. Design: `DESIGN-source-build-containers.md`. **DONE** (all phases A-D).

## Direction change result (2026-08-05) — DONE
Both guest binaries now compile from source inside pinned containers and land in
the cache; nothing precompiled is ever downloaded. Verified against the full
byte-identity suite (golden `2b86ab69…` unchanged).

**Kernel (build-only).** `ensure_kernel` = sha-verified cache hit → reproducible
container build from the sha-pinned cdn.kernel.org tarball → hard-verify against
the embedded `KERNEL_SHA256`. The release-asset fetch and the
`RELEASE_ASSET_URL/NAME` lock fields are GONE (kernel.lock rewritten; the stale
`kernel-6.1.128` GitHub release asset is moot and can be deleted). The kernel
CONFIG is now embedded beside the pin, tripwired against
`KERNEL_CONFIG_SHA256` (unit test + runtime check — a config edit without
`build-kernel --record` fails instantly). `build-kernel --record` stays the
maintainer bootstrap (reads/writes the checkout lock + config).

**Agent (embedded source).** The whole release/download mechanism is DELETED:
`agent.lock` (file + embed + parse + `--record --tag`) and
`.github/workflows/agent-release.yml`. The agent source now travels EMBEDDED in
the tdvmm binary: a root `build.rs` generates an `include_bytes!` table over
exactly the set `agent_src_id` hashes (`tdvmm-agent/` + `tdvmm-proto/` + root
`Cargo.lock`; ~300 KB, no new deps), plus the root `Cargo.toml` verbatim
(materialize-only: carries `[profile.agent-release]` — a checkout's local
profile edit can no longer change agent bytes) and a stub root `src/main.rs`.
`ensure_agent` materializes the set into a scratch dir, mounts it at `/src:ro`
(same path, same remap, same `TDVMM_AGENT_BUILD`), builds, and caches. The
identity is unchanged: build `a14c50c8…`, agent sha `1c0b5393…` — the exact
golden bake's agent. Tripwire unit test: embedded set == checkout set (identity
value + file list, both directions). `build-agent` (no flags now beyond `-o`)
always does a fresh container build from the embed — the double-build gate
still means two fresh containers.

**Cache.** `CACHE_VERSION` 5 → 6 (the bake key's `agent:` line is always the
source identity now). New cache paths:
- `<cache>/agent/tdvmm-agent-<build_key16>` + `.sha256` sidecar (verified on
  every reuse). `build_key` = sha256 over source-id + root-manifest sha +
  RUSTFLAGS sha + build-script sha + BUILD_EPOCH — everything that shapes the
  bytes, so no stale binary can ever be served (closes the documented
  profile-edit hole).
- `<cache>/diagnostics/kernel-build-<ver>.log` + `agent-build.log`: full build
  transcripts, written on success and failure.
- kernel/kernel-src/downloads/ledgers/artifacts/bake/base-runtime: unchanged.

**Terminal UI.** The `tdvmm build` inline viewport is now `1 + tail` rows
(tail = min(6, height-4), fixed at start): the spinner row over a dim bounded
live tail of the streamed container output (BuildKit-style) during the two
compile steps. Additive `engine::run_streamed(cmd, sink)` (two drain threads,
full transcript kept); `ux::run_build` dispatches: viewport active → streamed +
transcript to `<cache>/diagnostics/`, failure error carries the last 100 lines
+ the log path; inactive → `Inherit` (non-TTY bytes frozen). `tail_line` only
mutates the ring under the single renderer Mutex — the 120 ms ticker is the
only painter (a kernel `make -j` cannot corrupt the viewport). Steps went
8 → 10: "guest kernel" (2) and "guest agent" (4) are real steps with
cached/compiled notes; a bake-cache HIT still exits at step 3. The agent left
the mid-bake thread scope (serial step; zero cost warm, coherent tail cold).
On failure the pending step persists as a red `✗` plus its final tail lines
(flushed by `Progress::finish`, which `Drop` runs on every error path).

**Gates (all run 2026-08-05, on this code).**
- insert-trim two-bake `--no-cache`: `2b86ab69…` at every phase boundary — PASS.
- `scripts/bake_repeat_test.sh`: PASS (identical lock + ledger).
- `scripts/agent_double_build.sh`: PASS — both builds `1c0b5393…`, build
  `a14c50c8…`, byte-identical, from the embedded source.
- `cargo test --release`: 210 green (incl. new kernel-config + embedded-source
  tripwires); build + clippy clean on the new code.
- `scripts/standalone_bake_test.sh` REWRITTEN: a full standalone bake must now
  SUCCEED (no agent-gap logic, no stale-asset branch). Default seeds only the
  sha-verified kernel; `FULL_COLD=1` compiles the kernel in-container for real.
  `FULL_COLD=1` run (2026-08-05): installed binary, empty cwd, fresh cache, NO
  repo → the kernel was fetched as the pinned source tarball and compiled in
  the pinned container from the EMBEDDED config, and the result byte-matched
  `KERNEL_SHA256` (19506f47…); the agent compiled from the embedded source
  (1c0b5393…, build a14c50c8…); minirootfs + compose CLI verified against
  embedded pins; a `.tdvmm` was produced. The zero-checkout promise holds with
  zero prebuilt downloads.
- `git status guest/` clean through all bakes.

## Why
`tdvmm build` silently requires running from a full source checkout, so the
"install the binary, bake your compose file" promise is broken. Root cause:
`self_here()` (`src/build/util.rs:20`) locates `guest/` relative to the repo
layout (or `$CWD/guest`), and the bake also needs the whole repo (agent compiled
from source, cache key hashes repo trees). Goal: the binary sources all runtime
assets from the cache dir (`--cache-dir` > `$TDVMM_CACHE_DIR` > `$HOME/.tdvmm`)
or from embedded data, and `guest/` becomes test-only material named `testdata/`.

## Two real bugs found (fixed in Phase 1)
- `stack_lock.rs:83,91` bare `fs::write` with **no `create_dir_all`** → `tdvmm build myname ~/x/compose.yml` fails at step 7 *after the whole bake* if `guest/stacks/myname/` doesn't exist.
- The bake writes ledgers into `guest/stacks/<name>/` (`bake.rs:177,567`), so reusing an example name (`demo`) **silently overwrites committed lock goldens**.

## Decisions (owner, 2026-08-05)
- **Agent binary → pinned release asset** (mirror the kernel: embedded `agent.lock` = version+sha+release URL+builder digest; `ensure_agent(cache_dir)` fetches+verifies, falls back to the from-source container build when a checkout is present). **Add a tag-triggered GitHub Actions workflow** that builds the reproducible musl agent and publishes it as the release asset.
- **Rename `guest/` → `testdata/`** once nothing runtime/user-facing reads it.
- **Keep `guest/initramfs/`** (busybox clock guest) as `testdata/initramfs/` — it's the default `--initrd` for the `tdvmm boot` dev/bring-up verb + 2 smoke tests, never in a `.tdvmm`. (Owner may still choose to retire `tdvmm boot` + delete it — not yet decided; default is keep.)
- **All 3 phases, each gated by the byte-identity suite, report at each boundary.**
- Smaller calls (taken as recommended): retire `guest/manifest.txt` + `gen_manifest.sh`; user-bake ledgers go beside the artifact in the cache; `tdvmm boot` keeps a testdata default with a clear out-of-repo error.

## Reproducibility gates (non-negotiable)
Every phase must keep `.tdvmm` **byte-identical**. Gates: `scripts/artifact_test.sh` (cold==warm + corpus), `scripts/bake_repeat_test.sh`, and the cpio golden unit tests. Big assets (kernel/minirootfs/compose/agent) are sha-pinned so relocating them can't change bytes. **The one real risk is overlay embedding (Phase 2)** — embedded bytes/paths/modes (esp. the 0644 files not covered by explicit `set_mode`) must reproduce exactly. New acceptance test: **bake from an installed binary in an empty cwd**.

## Plan / checklist

### Phase 1 — output-side, no byte risk  ✅ DONE + COMMITTED (b5f55ca) — verified independently (byte-identity 2b86ab69, bake_repeat exit 0, 203 tests)
- [x] Stop writing `stack.lock` / `compose.lock.yml` into `guest/stacks/<name>/`; write beside the artifact in the cache. Fix `create_dir_all` + golden-clobber.
- [x] Move download cache (minirootfs tarball, compose CLI) → `cache_dir/downloads/`.
- [x] Move `vmlinux` output → `cache_dir/kernel/`.
- [x] Move `packages.lock` output out of the repo → cache.
- [x] Preserve the committed-golden unit tests (keep a clean maintainer regen path).
- [x] Bump `CACHE_VERSION` (3 → 4, `cache.rs:19`).
- [x] Update scripts reading those repo paths (`bake_repeat_test.sh`, `artifact_test.sh`, …).
- [x] Gate: full test suite green + a real byte-identity bake still identical.

### Phase 1 result (2026-08-05)
**What changed.** All Phase-1 relocations are output-only; nothing that enters the
`.tdvmm` moved. New cache layout (root = `--cache-dir` > `$TDVMM_CACHE_DIR` > `$HOME/.tdvmm`):

| output (was) | now |
|---|---|
| `guest/stacks/<name>/compose.lock.yml` | `<cache>/ledgers/<name>.compose.lock.yml` |
| `guest/stacks/<name>/stack.lock` | `<cache>/ledgers/<name>.stack.lock` |
| `guest/initramfs-alpine/packages.lock` (output) | `<cache>/ledgers/<name>.packages.lock` |
| `guest/initramfs-alpine/alpine-minirootfs-*.tar.gz` | `<cache>/downloads/…` |
| `guest/initramfs-alpine/docker-compose-<ver>` | `<cache>/downloads/…` |
| `guest/kernel/vmlinux-<ver>` | `<cache>/kernel/vmlinux-<ver>` |

A new `ledgers/` dir keeps `artifacts/` pure-`.tdvmm` (the existing invariant in
`bake.rs`). The `.tdvmm`, the intermediate cpio (`bake/`), the content-hash cache
(`bake/<key>/`), and the base-runtime cache are unchanged. `CACHE_VERSION` 3 → 4
(busts both the bake-cache and base-runtime keys; one cold miss). Kernel/config/
overlay/`*-engine.lock`/`rootfs-builder.lock` are still READ from `guest/` (Phase 2
embeds them). Two real bugs fixed: `write_stack_lock` now `create_dir_all`s its
parent (no more post-bake step-7 failure on a fresh stack name), and a normal
`tdvmm build` never writes into the repo, so reusing an example stack name can no
longer clobber a committed lock golden.

**Committed goldens stay green.** The unit tests (`compose::tests::emit_lock_matches_committed_locks`,
`compose::yaml_emitter::tests::round_trips_committed_locks_byte_for_byte`) still read
the committed `guest/stacks/*/compose.lock.yml` fixtures directly — untouched by any
bake now. `stack.lock` / `packages.lock` are committed anchors that no Rust test reads.

**Maintainer regen path.** Unchanged for the pure-unit case: `TDVMM_REGEN_LOCKS=1 cargo test`
rewrites the `compose.lock.yml` goldens from `emit_lock`. Extended for the full bake:
`TDVMM_REGEN_LOCKS=1 tdvmm build <name> <compose>` now copies THIS bake's cache
ledgers back over the committed fixtures — `guest/stacks/<name>/{compose.lock.yml,stack.lock}`
plus the `guest/initramfs-alpine/packages.lock` reference (skipped on a cache HIT,
where packages.lock isn't restored). Without the env var, the repo is never touched.

**Residuals (deferred, not blocking).** `tests/manifest.toml` still `requires` the repo
kernel path for the boot-smoke tests, and `scripts/gen_manifest.sh` (slated for retirement)
still reads `guest/kernel/vmlinux-6.1.128`. The repo kernel copy still exists, so these
pass today; the boot-smoke scripts (`smoke_test*.sh`, `artifact_test.sh`) were made
cache-first with a repo-tree fallback. Full cleanup lands with the Phase-3 `testdata/` rename.

### Phase 2 — input-side embedding + agent asset (reproducibility risk)
- [x] Embed the `kernel.lock` PIN (not the kernel, not the config — owner decision
      2026-08-05 mid-phase: vmlinux stays a pure GitHub release asset fetched into
      the cache; the pin is a compiled-in pointer via `include_str!`; the kernel
      config stays a checkout file read only by the container-REBUILD fallback +
      `--record`, mirroring the agent's from-source fallback).
- [x] Embed `compose-engine.lock`, `rootfs-builder.lock` (`include_str!`, parsed
      at use — the committed files stay the single source of truth).
- [x] Embed the `initramfs-alpine/overlay/` (7 files, ~40 KB) — reproduce bytes+modes exactly.
- [x] Add `agent.lock` + `ensure_agent(cache_dir, …)` (release asset + source fallback).
- [x] Add the tag-triggered GH Actions workflow to publish the agent asset.
  - **NOTE — chicken-and-egg:** no agent release asset exists yet. Phase 2 builds the
    full mechanism; `ensure_agent` uses the source-build fallback (needs a checkout)
    until the OWNER cuts the first release (pushes a tag → the new workflow publishes
    the asset → record its sha into `agent.lock`). Do NOT cut a release in Phase 2.
    Kernel/minirootfs/compose/overlay have real pinned assets already, so THOSE go
    fully standalone now; the zero-checkout **agent** path only closes after the
    first release. The empty-cwd acceptance test verifies everything testable now and
    documents the agent-release dependency — it does not fake the agent path.
- [x] `self_here()` → optional `find_guest_dir()`/`find_repo_root()` (checkout
      locator for the fallbacks/maintainer flows only); cache-key inputs reworked
      (repo tree hashes → the pinned agent identity); `CACHE_VERSION` 4 → 5.
- [x] Gate: byte-identity suite + **new empty-cwd installed-binary bake test**
      (`scripts/standalone_bake_test.sh`).

### Phase 2 result (2026-08-05)
**What is embedded now (compile time, `include_str!`/`include_bytes!`).** The
committed files stay the single source of truth; the binary carries a copy:

| embedded | from | consumed by |
|---|---|---|
| kernel PIN (`kernel.lock`, ~1 KB) | `guest/kernel/kernel.lock` | `ensure_kernel` — fetches the vmlinux release asset into `<cache>/kernel/`, sha-verified. The KERNEL ITSELF IS NOT EMBEDDED (owner decision 2026-08-05): download + cache, pin compiled in. The kernel CONFIG is also not embedded — it's read from a checkout only by the container-REBUILD fallback + `--record` (maintainer paths, mirroring the agent's from-source fallback). |
| `compose-engine.lock` | `guest/initramfs-alpine/` | compose CLI pin (version+sha) |
| `rootfs-builder.lock` | `guest/initramfs-alpine/` | fetch/rootfs-builder image pin |
| `images.lock` | `tdvmm-agent/` | agent-builder image pin (see below why this stays a separate file) |
| agent PIN (`agent.lock`) | `tdvmm-agent/agent.lock` (NEW) | `ensure_agent` (empty = pending first release) |
| overlay (7 files, ~40 KB) | `guest/initramfs-alpine/overlay/` | `src/build/overlay.rs` static table |

**Overlay mode pinning (the reproducibility risk — resolved).** The overlay is a
static `(relpath, mode, bytes)` table (`src/build/overlay.rs`): `init` + the 3
`usr/local/bin/*.sh` scripts pinned 0755, `etc/inittab` + the 2
`etc/containers/*.conf` pinned 0644 (these three previously inherited the
checkout's modes — now pinned, so even a weird-umask checkout can't perturb the
cpio). Directories are created 0755 only where missing (`cp -a`-merge parity —
existing base-rootfs dirs keep their tar modes). Two unit tests guard it:
embedded-vs-checkout byte/mode identity (catches drift when overlay files are
edited without rebuilding the table) and umask-independence of `materialize`.

**Agent asset mechanism + the self-reference trap.** `ensure_agent(cache_dir,
source_out, ux)` mirrors `ensure_kernel`: cache hit (sha-verified) → release
asset fetch (sha-verified) → from-source pinned-container build (needs a
checkout; ALSO sha-verified against the recorded pin once one exists — the
drift tripwire). CRITICAL invariant: `agent.lock` lives inside `tdvmm-agent/`,
whose tree hash IS the agent build hash (`TDVMM_AGENT_BUILD`, compiled into the
agent bytes) — so `agent.lock` is EXCLUDED from that hash (`agent_src_id`,
`tree_hash(…, &["agent.lock"])`). Without the exclusion, recording a pin would
change the very bytes it pins (and adding the file already broke the golden —
caught by the byte-identity gate, fixed). `images.lock` stays a separate file
for the same reason in reverse: it MUST stay inside the hashed tree (a builder
bump is a real toolchain change). Root `Cargo.toml`'s `[profile.agent-release]`
is NOT hashed — noted in agent.lock; re-record after profile edits.

**Agent release flow (record-BEFORE-tag; owner action to close the gap).**
1. `tdvmm build-agent --record --tag agent-<version>` → writes the pin into `tdvmm-agent/agent.lock`
2. commit, tag that commit `agent-<version>`, push the tag
3. `.github/workflows/agent-release.yml` (NEW, `agent-*` tags) rebuilds via `tdvmm build-agent` itself (zero flag drift), double-builds for byte-identity, REFUSES to publish unless bytes == the committed pin, then uploads `tdvmm-agent-x86_64-unknown-linux-musl` + sha256sums
4. rebuild/re-release tdvmm so the pin is embedded

Until step 1-3 happen, agent.lock is a documented empty placeholder and the
from-source fallback (checkout required) is the only agent path.

**`self_here()` → optional locator.** Now `find_guest_dir()`/`find_repo_root()`
returning `Option` (`src/build/util.rs`). Consumers, each with its own clear
error only when actually needed: the agent from-source fallback, the kernel
container-rebuild fallback + `build-kernel --record`, `tdvmm boot`'s default
initrd, `TDVMM_REGEN_LOCKS` fixture regen, and `agent_cache_input`'s
source-identity case. Nothing else touches the checkout.

**Cache keys.** `CACHE_VERSION` 4 → 5 (one cold miss). The bake key's
`agent/proto/cargolock` repo-tree lines collapsed into one `agent:` line —
the recorded release sha when agent.lock has one, else `agent_src_id` (checkout).
Overlay + compose-engine lines now come from the embedded data. Keys never
enter artifact bytes; the `.tdvmm` golden confirms.

**Byte-identity + gates (all run on the corrected code, 2026-08-05).**
- insert-trim two-bake `--no-cache`: both bakes `2b86ab69…` (the Phase-1 golden) — PASS.
  (An intermediate version that put agent.lock in the hashed tree WITHOUT the
  exclusion baked `cd926acb…` — the gate caught it; that is why the exclusion is
  load-bearing.)
- `scripts/bake_repeat_test.sh`: PASS (identical compose.lock + ledger + initramfs across two `--no-cache` bakes).
- `scripts/agent_double_build.sh`: PASS (both builds `1c0b5393…`, build hash `a14c50c8…` — matching the golden bake's agent exactly).
- Self-reference check: with agent.lock POPULATED (real sha, no asset URL), a
  rebuilt tdvmm's `build-agent` reproduces the same build hash + bytes — recording
  a pin cannot move the `.tdvmm`.
- `cargo build` clean; new code clippy-clean; 205 tests green (203 + 2 new overlay tests).
- `git status guest/` stays clean through all bakes.

**Residual noticed (pre-existing, not Phase 2):** the committed
`guest/stacks/insert-trim/stack.lock` fixture records `agent_sha256 809d4845…`,
but the golden `2b86ab69…` bake (Phase 1 and now) actually embeds agent
`1c0b5393…` (build `a14c50c8…`) — the fixture predates an agent-source change
and was never regenerated. No Rust test reads it; refresh via
`TDVMM_REGEN_LOCKS=1` during Phase 3 if desired.

**NEW acceptance script: `scripts/standalone_bake_test.sh`** (installed binary,
empty cwd, fresh cache). Result: minirootfs + compose CLI + overlay + all pins
resolve standalone; the bake stops at the AGENT step with the documented
release-gap error. TWO owner actions gate full standalone:
1. **KNOWN ISSUE (found by this test): the `kernel-6.1.128` release asset is
   STALE** — the published `vmlinux-6.1.128` hashes `98d75369…`, but kernel.lock
   (and the checkout vmlinux) record `19506f47…`. The standalone kernel fetch
   therefore fails → falls back → refuses without a checkout. Fix:
   `gh release upload kernel-6.1.128 guest/kernel/vmlinux-6.1.128 --clobber`.
   (The script reports this, then seeds the sha-verified pinned vmlinux to keep
   exercising the rest of the standalone path — nothing faked.)
2. **The first agent release** (flow above), then re-run with `EXPECT_AGENT_GAP=0`.

### Phase 3 — rename + docs (mechanical, lands last)
- [ ] Rename `guest/` → `testdata/`; update `tests/manifest.toml`, `scripts/*`, unit-test path literals, `run.sh`, CONTRIBUTING.
- [ ] Rewrite README / GETTING_STARTED build examples to use the user's own `./compose.yml` (not `guest/stacks/...`); "clone the repo for worked examples" → `testdata/stacks/`.
- [ ] Update `cli.rs` `--help` example text.
- [ ] Resolve the `tdvmm boot` / `initramfs` keep-vs-retire question before finalizing.

## Deferred follow-ups (queued, do AFTER Phase 2)
- **`tdvmm doctor` command** (owner, 2026-08-05): a concise CLI that (a) checks all
  prereqs — `/dev/kvm` r+w, hardware virt, kernel ≥ 5.16 (TSC-offset device attr),
  podman present + working, network reachable, cache dir resolvable/writable + free
  disk; (b) downloads the dependencies into the cache (kernel, agent, busybox, alpine
  minirootfs, compose CLI), **skippable with `--skip-downloads`**; (c) prints a concise
  report flagging any issues. **Deferred until Phase 2** because its download half is a
  thin front-end over `ensure_kernel(cache)` / `ensure_agent` / the cache downloads that
  Phase 2 creates (`ensure_agent` doesn't exist yet; downloads currently target `guest/`).
  Build it as a thin front-end once those primitives are stable. Owner's steer: keep it
  VERY concise.

## Reference (investigation evidence)
Full classified reference list + per-asset provenance is in the conversation's Fable investigation (2026-08-05). Key files: `src/build/{util,bake,kernel,pins,agent,cache,base,stack_lock,mod}.rs`, `src/build/initramfs.rs`.
