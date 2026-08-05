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

### Phase 1 — output-side, no byte risk  ⏳ IN PROGRESS
- [ ] Stop writing `stack.lock` / `compose.lock.yml` into `guest/stacks/<name>/`; write beside the artifact in the cache. Fix `create_dir_all` + golden-clobber.
- [ ] Move download cache (minirootfs tarball, compose CLI) → `cache_dir/downloads/`.
- [ ] Move `vmlinux` output → `cache_dir/kernel/`.
- [ ] Move `packages.lock` output out of the repo → cache.
- [ ] Preserve the committed-golden unit tests (keep a clean maintainer regen path).
- [ ] Bump `CACHE_VERSION` (currently 3, `cache.rs:19`).
- [ ] Update scripts reading those repo paths (`bake_repeat_test.sh`, `artifact_test.sh`, …).
- [ ] Gate: full test suite green + a real byte-identity bake still identical.

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

## Reference (investigation evidence)
Full classified reference list + per-asset provenance is in the conversation's Fable investigation (2026-08-05). Key files: `src/build/{util,bake,kernel,pins,agent,cache,base,stack_lock,mod}.rs`, `src/build/initramfs.rs`.
