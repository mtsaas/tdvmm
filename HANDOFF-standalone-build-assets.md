# Handoff — make `tdvmm build` work from an installed binary (assets → cache, `guest/` → `testdata/`)

**Branch:** `feat/standalone-build-assets` (off `main`)
**Status:** Phase 1 in progress (see checklist). Update this doc at every phase boundary.

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

### Phase 1 — output-side, no byte risk  ✅ DONE (see "Phase 1 result")
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
- [ ] Embed `kernel.lock` + kernel config; `ensure_kernel` output → cache.
- [ ] Embed `compose-engine.lock`, `rootfs-builder.lock` as consts.
- [ ] Embed the `initramfs-alpine/overlay/` (7 files, ~40 KB) — reproduce bytes+modes exactly.
- [ ] Add `agent.lock` + `ensure_agent(cache_dir)` (release asset + source fallback).
- [ ] Add the tag-triggered GH Actions workflow to publish the agent asset.
- [ ] Delete `self_here()`; rework cache-key inputs (repo tree hashes → pinned shas).
- [ ] Gate: byte-identity suite + **new empty-cwd installed-binary bake test**.

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
