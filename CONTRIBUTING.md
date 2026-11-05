# Contributing to cachekit-rs

Thanks for taking the time to look at the project. This file explains how
to build it, how the code is organised and what a change should contain.

## Getting started

You need a Rust toolchain with Cargo; nothing else. The crate has no
dependencies and must stay that way.

```sh
cargo build          # library and binary
cargo test           # unit, integration and documentation tests
cargo doc --no-deps  # API documentation, should build without warnings
cargo run --example quickstart
```

## Ground rules

- **Standard library only.** Do not add dependencies, including
  dev-dependencies.
- **No `unsafe`.** The crate is built with `#![forbid(unsafe_code)]`.
- **Public items are documented.** `#![warn(missing_docs)]` is on; keep the
  build free of warnings.
- **Tests are deterministic.** Anything involving time uses `ManualClock`,
  never `thread::sleep`. Randomised tests use fixed seeds. Tests that need
  files write them under `env!("CARGO_TARGET_TMPDIR")`.
- **Behaviour changes come with tests.** A bug fix starts with a test that
  fails without the fix.
- **Formatting.** Follow the default rustfmt style (four spaces, 100
  columns). The existing code was formatted by hand in that style, so
  `cargo fmt` may still move a few lines; keep formatting-only changes in
  their own commit.

## How the code fits together

`PolicyCache` in `src/cache.rs` is the engine. It owns the entries in a
slab of slots and a hash map from keys to slot indices. It knows about
bounds, weights, expiry, listeners and statistics, but not about eviction
order: that is the job of a `Policy` (`src/policy/`), which only ever sees
slot indices.

Everything else is layered on top of the engine:

- `CacheBuilder` (`src/builder.rs`) validates settings and constructs
  engines, including the shards of a `ShardedCache`.
- `SyncCache` and `ShardedCache` (`src/sync.rs`) add locking.
- `snapshot` reads and writes contents through the engine's public API.
- `trace` parses recorded operations and replays them against an engine.
- `cli` turns command line arguments into a builder, a trace and a report;
  `src/main.rs` only connects it to the process.

### Adding an eviction policy

1. Implement `Policy` in a new file under `src/policy/`. The `SlotList` in
   `src/list.rs` is usually enough to keep an order in O(1).
2. Add a variant to `PolicyKind` (name, parsing, `build()` and `ALL`) and a
   type alias next to `LruCache` in `src/cache.rs`.
3. Teach the reference model in `tests/model.rs` the new ordering. The
   model must stay simple enough to be obviously correct; it is what the
   engine is checked against.
4. Mention the policy in the README and the CLI usage text.

### Changing a text format

The snapshot and trace formats are described at the top of
`src/snapshot.rs` and `src/trace.rs` and again in the README. Keep the
three in step, and keep old snapshots readable or bump the version in the
snapshot header.

## Commits and pull requests

- One logical change per commit, with the build and tests passing.
- Commit subjects follow the conventional style used in the history:
  `feat(cache): ...`, `fix(cli): ...`, `test: ...`, `docs: ...`,
  `refactor(policy): ...`. Use the body to say why.
- Add a line to `CHANGELOG.md` for anything a user of the library or the
  binary would notice.
- If a change makes one of the notes under "Behaviour worth knowing" in the
  README untrue, update the README in the same change.

## Reporting problems

A good report has the smallest program or trace that shows the problem,
what you expected, and what happened instead. For the binary, the exact
command line and the trace file are usually enough.
