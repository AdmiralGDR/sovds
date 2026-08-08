# sovds — Sovereign Dataset Toolkit

🇬🇧 English | [🇷🇺 Русский](README.ru.md)

Build **labelled, ground-truth datasets from live systems** — collect telemetry,
deliberately and safely induce faults with provenance labels, segment the stream
into fault *episodes*, and measure detection and rollback-safety. So a monitoring
rule set (or a future model) can be validated on real system behaviour, not
guesswork.

`sovds` is **domain-agnostic**: it knows nothing about routers, services, or any
specific system. You plug in your own world through one trait and get the whole
dataset pipeline for free.

## Why

Built to this project's internal engineering principles:

- **Zero dependencies, agnostic core.** No third-party crates. The
  core compiles `no_std` (the on-disk parts sit behind the default `std` feature).
  Embed it without dragging a dependency tree into your project; compile it offline
  from a pinned toolchain in ten years.
- **Invalid states unrepresentable.** A record separates provenance
  (`Source`), the *diagnosed* fault, and the *ground-truth injection label*, so
  "we broke it on purpose" can never be confused with a real fault.
- **Bounded, no-panic.** `#![forbid(unsafe_code)]`; the lints deny
  `unwrap`/`expect`/`panic`/indexing/arithmetic-overflow. Nothing panics or
  overflows on hostile input; a corrupt log line is counted and skipped, never
  fatal.

## The contract

Implement [`Domain`] once for your system, declaring three types and how they
serialise into a record line:

```rust
use sovds::domain::{Accept, Domain, ParseError};
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Host;                                   // your domain tag

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Res { Ok, CpuSaturated, MemPressure }     // your fault vocabulary

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Load { cpu_pct: u8, mem_pct: u8 }       // your observation

impl Domain for Host {
    type Obs = Load;
    type Fault = Res;
    type Annot = bool;             // any per-record extra (e.g. "did we act?")
    type Builder = /* a small accumulator for parsing */;

    fn healthy() -> Res { Res::Ok }
    fn faults() -> &'static [Res] { &[Res::Ok, Res::CpuSaturated, Res::MemPressure] }
    fn fault_label(f: Res) -> &'static str { /* wire spelling */ }
    fn parse_fault(s: &str) -> Option<Res> { /* inverse */ }
    fn write_obs<W: fmt::Write>(w: &mut W, o: &Load) -> fmt::Result { /* " cpu=.. mem=.." */ }
    fn write_annot<W: fmt::Write>(w: &mut W, a: &bool) -> fmt::Result { /* " shed=.." */ }
    fn accept(b: &mut Self::Builder, key: &str, val: &str) -> Result<Accept, ParseError> { /* .. */ }
    fn finish(b: Self::Builder) -> Result<(Load, bool), ParseError> { /* .. */ }
}
```

That is all. See [`examples/host_monitor.rs`](examples/host_monitor.rs) for the
complete, runnable version:

```sh
cargo run -p sovds --example host_monitor
```

## What you get

| Module | Feature | What it does |
|--------|---------|--------------|
| `record` | core | `Record<D>` + one self-describing `key=value` line; order-tolerant, hostile-input-safe parse |
| `episode` | core | segment a record stream into fault episodes (the analysis unit — independent incidents, not rows) |
| `injection` | core | per-window detection (`detected`/`missed`/`silent`) + rollback safety (`recovered`/`stuck`/`pending`) over injected faults |
| `stats` | core | integer fixed-point online estimators (EWMA, bounded one-sided CUSUM) |
| `dataset` | std | load a rotating log back into a `Corpus<D>`, tolerant of corrupt lines |
| `recorder` | std | durable atomic append with bounded generation rotation |
| `readiness` | std | episodes-per-class rollup — "is there enough data to train yet?" |

## The record format

One line per observation cycle. Framing tokens are owned by the toolkit; your
domain owns its observation/annotation tokens:

```
t=<tick> ts=<unix> probed=<0|1> class=<fault> src=<live|backfill|synth> [inject=<fault>] <your obs tokens> <your annot tokens>
```

The parser is order-tolerant, so a log written by an older layout still loads.
`inject=` is present only when a fault was **deliberately induced** — that label is
what separates ground truth from organic behaviour.

## Install

```toml
[dependencies]
sovds = { path = "…/sovds" }             # no_std + std (default)
# or, for a no_std embed:
sovds = { path = "…/sovds", default-features = false }
```

## Licence

AGPL-3.0-or-later. See [LICENSE-AGPL](../../LICENSE-AGPL).
