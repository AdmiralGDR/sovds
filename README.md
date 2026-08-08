# Sovereign Router Brain

🇬🇧 English | [🇷🇺 Русский](README.ru.md)

This repository holds two things, cleanly separated:

1. **[`sovds` — Sovereign Dataset Toolkit](crates/sovds/README.md)** — a
   **domain-agnostic, zero-dependency, `no_std`** library for building labelled
   ground-truth datasets from live systems (safe controlled fault injection +
   telemetry → episodes → detection & rollback-safety metrics). This is the
   reusable product; embed it in any project via one trait. See its
   [README](crates/sovds/README.md) and
   [`host_monitor` example](crates/sovds/examples/host_monitor.rs).
2. **The Sovereign Router Brain** — the **reference implementation**: an off-router
   supervisor for the home network, built on `sovds`. The router stays dumb and
   reliable; the intelligence — health scoring, failure classification, and
   self-healing remediation — lives here, on a machine with real compute (the
   laptop today, a home server later). It reaches the router only through isolated
   adapters over SSH.

Both are built strictly to this project's internal engineering principles
(below) and, since 2026-08-08, to its IP-Shield hardening — see
[`docs/SECURITY-POSTURE.md`](docs/SECURITY-POSTURE.md) for what was adopted,
what was deliberately not, and why.

## Why off-router

The router (MediaTek MT7981, dual Cortex-A53, ~256 MB RAM, no NPU) can run a
robust rules engine but not real analytics or an LLM. So the design splits
cleanly: a tiny, dependency-free decision **core** that could run anywhere, fed
by **adapters** that do all the I/O. The same core scales from a shell-driven
poller today to an ML/LLM-assisted brain on a home server, without a rewrite.

## Architecture (hexagonal / ports & adapters)

```
            ┌─────────────────────────────────────────┐
            │              app (composition root)      │  ← wires everything, runs the loop
            │   ┌───────────────────────────────────┐  │
   SSH  ───►│   │   adapters (I/O bridges)           │  │◄─── ntfy
  router    │   │   telemetry · actuator · notifier  │  │     notifications
            │   └──────────────┬────────────────────┘  │
            │                  │ implement                │
            │            ports (traits)  ← contracts      │
            │                  │ used by                   │
            │   ┌──────────────▼────────────────────┐    │
            │   │  core (RouterDomain over sovds)    │    │
            │   │  domain · failure · fsm::decide    │    │
            │   └──────────────┬────────────────────┘    │
            │                  │ implements sovds::Domain  │
            │   ┌──────────────▼────────────────────┐    │
            │   │  sovds (product — domain-agnostic) │    │
            │   │  record · episode · injection ·    │    │
            │   │  dataset · recorder · readiness    │    │
            │   └───────────────────────────────────┘    │
            └─────────────────────────────────────────────┘
```

- **`crates/sovds`** — the reusable toolkit product. `#![no_std]` core, zero
  dependencies, `std` feature for the on-disk parts. See its own README.
- **`crates/core`** — the router's `sovds::Domain` implementation
  (`RouterDomain`) plus the pure decision core: `fsm::decide(state, telemetry,
  cfg, now)`, a total, deterministic, panic-free function from an observation to
  an action.
- **`crates/adapters`** — SSH telemetry + actuator, ntfy notifier, the router
  fallback-buffer drain. The only code allowed to touch the network, the disk,
  or a clock.
- **`crates/app`** — the composition root and every CLI tool (below).
- **`crates/sovml`** — a from-scratch, zero-dependency, integer decision tree
  (interpretable, `no_std+alloc`) trained on the `sovds` corpus.

## The decision core, in one paragraph

Each cycle, telemetry is collapsed into the single most urgent `FailureClass`
(WAN → process → path → DNS, by causal depth). The state machine then applies
**hysteresis** (never react to one blip — a fault must be confirmed N cycles),
**escalation** (climb a fixed, finite ladder of remedies, least-disruptive
first: switch node → restart → bounce WAN → failover → reboot), and **cooldown
with backoff** (never thrash). Recovery is honored immediately at any point.

## How this maps to the engineering principles

| Principle | Where it lives in the code |
|---|---|
| Code is a liability | Every generic mechanism lives once in `sovds`; the router only supplies its domain. |
| Agnostic core | `core`/`sovds` are `no_std`; all I/O is injected via `ports` traits. |
| Dependency quarantine | Zero third-party crates anywhere in the workspace. |
| Invalid states unrepresentable | Subsystems are enums, not bool soups; `State` is an ADT; `Source`/diagnosed-fault/injection-label can never be confused. |
| Zero global state | `decide` takes state + config explicitly; no statics. |
| ACID / rollback | `Actuator` returns `Ok` only after verifying the post-condition. |
| Compile-time enforcement | `#![forbid(unsafe_code)]`; clippy denies unwrap/panic/index/overflow. |
| Bounded execution | Fixed node array; finite escalation ladder; saturating math. |
| Graceful degradation | `Unknown` readings never trigger action; notify failures are non-fatal; integrity checks alert, never self-destruct. |
| No panic | Errors are `Result` values; no `unwrap`/`expect`/`panic` anywhere. |
| Justified optimization | Rust now; a Zig on-router agent only when profiling demands it. |
| Reproducible builds | `rust-toolchain.toml` + `Cargo.lock` pin everything; `scripts/reproducible-build.sh` self-verifies byte-identical binaries. |
| Zero-touch gates | CI: `fmt --check` + `clippy -D warnings` + `test`, all blocking. |
| IP Shield | AGPL-3.0-or-later, signed commits, `SHA256SUMS` + OpenTimestamps. See [security posture](docs/SECURITY-POSTURE.md). |

## Build & verify

```sh
cargo fmt --all --check                      # formatting is not negotiable
cargo clippy --all-targets --locked -- -D warnings   # warnings are errors
cargo test --workspace --locked              # deterministic unit tests
sh scripts/reproducible-build.sh --print     # reproducibility check: all binaries byte-identical
sh scripts/verify-integrity.sh               # installed binaries match SHA256SUMS
```

## Tools

10 CLI binaries, all built by `cargo build --release --locked`:

- `sovereign` — the live supervisor (observe-only unless `SOVEREIGN_ACTUATE` is set).
- `sovereign-inspect [log]` — dataset metrics (class balance, latency distribution).
- `sovereign-episodes [log] [--list]` — segment the log into **fault episodes**
  (the real training/analysis unit) and report training-readiness per class.
- `sovereign-inject <class> [--secs N]` — arm a **ground-truth injection label**.
- `sovereign-export [log] [out.tsv]` — flatten the dataset into a TSV feature table.
- `sovereign-anomaly [log] [--top N]` — unsupervised scan for rule-gap candidates.
- `sovereign-train [log] [--depth N]` — train an interpretable decision tree
  (`sovml`) on the corpus, evaluate against a held-out slice.
- `sovereign-replay` — replay the corpus under different adaptive configs.
- `sovereign-backfill` — loss-free drain of the router's fallback buffer.
- `sovereign-scenario` — replay the canonical fault-scenario suite through the
  live decision pipeline; the only way to measure rare classes (`wan_down`,
  `singbox_dead`, `dns_fail`) that passive collection sees too little of.

See [`FORMAT.md`](FORMAT.md) for the record layout and
[`deploy/README.md`](deploy/README.md) ([Russian](deploy/README.ru.md)) for a
from-scratch install.

## Status

- [x] `sovds` — feature-complete, domain-agnostic, independently tested against
      a third-party example domain (`host_monitor.rs`).
- [x] `sovereign-core` (`RouterDomain`) — router migrated onto `sovds`, live in
      production, 0 records skipped against the historical corpus.
- [x] Adapters — SSH telemetry + actuator, ntfy notifier, router fallback drain.
- [x] App — composition root, bounded loop, 10 CLI tools.
- [x] `sovml` — zero-dependency decision tree, trained on the live corpus.
- [x] IP Shield — AGPL relicense, signed commits, hash+timestamp anchoring.
- [ ] Router access via Tailscale (removes the network-dependency for control).
- [ ] (Later, if justified) Zig on-router telemetry agent.
