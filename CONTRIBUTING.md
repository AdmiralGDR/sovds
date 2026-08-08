# Contributing

This repo is built to this project's internal engineering principles. The rules
below are not style preferences — they are those principles made mechanical.

## The gates (all enforced by CI — a commit that fails one is not working code)

```sh
cargo fmt --all --check                       # formatting is not up for debate
cargo clippy --all-targets --locked -- -D warnings   # every warning is an error
cargo test --all --locked                     # deterministic tests
cargo build -p sovds --no-default-features     # the toolkit core stays no_std
sh scripts/reproducible-build.sh               # every binary byte-identical
```

The workspace lints (`Cargo.toml`) **deny** `unwrap`, `expect`, `panic`, indexing,
and arithmetic overflow. Use `?`, `match`, `.get()`, and `saturating_*`/`checked_*`
instead. `sovds` additionally denies missing docs on its public API, and the whole
tree is `#![forbid(unsafe_code)]`.

## Structure

- **`crates/sovds`** — the product: a domain-agnostic, zero-dependency, `no_std`
  dataset toolkit. This is what third parties embed. Keep it dependency-free and
  keep the core `no_std` (I/O lives behind the `std` feature).
- **`crates/core`** — the router domain + supervisor, an implementation of
  `sovds::Domain` (`RouterDomain`). `no_std`, zero third-party deps.
- **`crates/adapters`** — the only place allowed to touch the outside world, and
  only through the OS (`ssh`/`curl`), never a third-party crate.
- **`crates/app`** — composition root + CLIs.

## Extending the toolkit

**Add a fault class (router):** extend `FailureClass` (`core/src/failure.rs`), then
the compiler walks you through every exhaustive `match` that must handle it —
including `RouterDomain`'s `fault_label`/`parse_fault`/`faults` and the classifier.
Invalid states stay unrepresentable; there is no `_ =>` default to hide a gap.

**Use the toolkit for a new system:** implement `sovds::Domain` for your world —
your observation, your fault vocabulary, your annotation, and how they serialise.
See `crates/sovds/examples/host_monitor.rs` for a complete ~150-line integration.
Nothing else changes; you get records, episodes, injection metrics, and readiness
for free.

## Discipline

- **Metrics before theories:** don't tune a constant or change a
  decision rule without measuring it on real data first (`sovereign-replay`,
  `sovereign-scenario`). Record what the evidence showed in the commit.
- **Bounded, honest:** every loop terminates, every external call
  has a timeout, and the data never lies — provenance (`src=`), the diagnosed
  fault, and the injection label (`inject=`) are kept distinct so "we broke it on
  purpose" is never confused with a real fault.
