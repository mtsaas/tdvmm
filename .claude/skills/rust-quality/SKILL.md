---
name: rust-quality
description: >-
  Raise a Rust module in this repo to the top quality bar: make illegal states
  unrepresentable with type-state, use only traits that earn their place,
  structured errors with source chains, module docs that read as a spec, and
  byte-identity golden tests for anything reproducible. Use when refactoring a
  module for quality, writing a new load-bearing one, or doing a quality (not
  bug-hunting) review. `src/artifact/` is the worked example.
---

# Rust quality bar

The standard for load-bearing modules. `src/artifact/` is the reference
implementation — match its shape when raising another module to this bar.

## Make illegal states unrepresentable

If a value can be incomplete or half-built, encode the stages as distinct types so
the invalid state cannot be constructed and the wrong call cannot compile.

- One struct per state. Each transition consumes `self` and returns the next
  state. The terminal state is consistent by construction — a value that exists
  has already satisfied every invariant.
- Put the one fallible step at the natural seal point, so fallibility converges on
  a single `Result` instead of noising every link in the chain.
- `#[must_use]` on the states so a dropped chain is a warning.

`ArtifactBuilder → NeedsKernel → NeedsInitramfs → SealedArtifact`: members are
added in order, `initramfs()` seals (size-cap + freeze the manifest from the
payload hashes), and a `SealedArtifact` cannot fail `verify`. There is no method to
omit, reorder, or duplicate a member.

Skip the ceremony: no marker trait or `Builder<S>` when nothing is generic over the
state — distinct structs with inherent methods *are* the type-state idiom. No
wrapper struct that only duplicates fields for zero safety gain.

## Traits earn their place

Implement a trait when it removes a footgun or unlocks composition; skip it when
it is ceremony.

- Worth it: `impl Write` for a wrapper that hashes/counts bytes on their way to the
  sink (one pass, no re-read); `impl Iterator` for a cursor that advances itself
  (the "forgot to advance" bug class disappears); a real `Error` with `source()`.
- Not worth it: a marker trait nothing is bounded on; a trait for plain data (a
  member is a `(name, bytes)` struct, not a behavior); a `TryFrom`/`Display` impl
  where a named constructor or caller-side formatting is clearer.

## Errors are structured

One enum per module. Variants name what actually went wrong — I/O, parse,
malformed-data, not-found — not one variant per call site. Each carries its
`source()`.

- Construct through context-attaching helpers (`io(what, e)`, …); a bare
  "No such file" is useless without the operation that produced it.
- No `From<io::Error>` — context is mandatory, so there is no zero-context path.
- `Display` reads as a sentence. Keep it to a handful of variants; a dozen nobody
  matches on is its own kind of noise.

## Docs are a spec, not a diary

The module doc explains what the thing is: its byte/wire layout, its identity and
invariants, its lifecycle. Each type's doc states the invariant that type
guarantees.

- Present tense, describing how the current design works, for a human reading the
  code.
- Never narrate the design journey ("a custom X was rejected", "chosen over Y"),
  cite review threads or people, or leave a comment pointing at deleted code. If a
  rationale matters, state the constraint the code satisfies — not who decided it.

## Guard reproducible output with a golden

If the module emits reproducible bytes, add a golden test: build a fixed sample,
assert its whole-output sha256 against a hardcoded hex. It is the tripwire for
accidental format or encoding drift (a serde bump, a stray header edit). Rebuild
the hash only when the byte change is intended, and say so.

## Harden every untrusted-input path

A reader/codec over bytes it did not write:

- Validate framing (magic, structure) and checksums before trusting any field, so a
  non-artifact fails as "not a .tdvmm" rather than a stray parse error.
- Bound every allocation against the actual input length — never `vec![0; n]` on an
  attacker-controlled `n`.
- Reject unsupported versions; cap sizes at the format's limit.
- No `unwrap`/panic on the read path.

## No garbage

- No dead `pub` API — a binary crate exports only what a caller names; reachability
  for a public signature is enough for the rest.
- No fixed-order-by-convention maintained in two places — enforce order with the
  types, or keep one source of truth.
- Take `impl AsRef<Path>`, not `&str`, for paths — it removes the UTF-8 dances at
  call sites.
- No long positional argument lists — pass a named struct.
- No silent truncation, no swallowed errors (`let _ = …` that can fail loudly).

## Keep scope disciplined

Every pattern must earn its place; prioritize simplicity over completeness. When
you decline a pattern to avoid gilding, note it in one line so the next reader
knows it was a choice, not an oversight.

## Applying it

- Split a large module into a directory: `mod.rs` carries the spec doc, the
  re-exports, and the cross-cutting tests; focused submodules each carry their own
  tests.
- Before it is done: clean build with zero warnings, the tests, the golden, and —
  for bake outputs — `bake-repeat`.
- Quality here is a blocking gate. Route the finished diff through a Fable review
  before it lands.
