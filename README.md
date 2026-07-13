# Sovereign Router Brain

An off-router supervisor for the home network. The router stays dumb and
reliable; the intelligence — health scoring, failure classification, and
self-healing remediation — lives here, on a machine with real compute (the
laptop today, a home server later). It reaches the router only through isolated
adapters over SSH/Tailscale.

Built strictly to `the internal engineering charter` (the principles of Sovereign Engineering).

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
   SSH  ───►│   │   adapters (I/O bridges)           │  │◄─── ntfy / Telegram
  router    │   │   telemetry · actuator · notifier  │  │     notifications
            │   └──────────────┬────────────────────┘  │
            │                  │ implement                │
            │            ports (traits)  ← contracts      │
            │                  │ used by                   │
            │   ┌──────────────▼────────────────────┐    │
            │   │   core  (pure · no_std · 0 deps)   │    │
            │   │   domain · failure · fsm::decide   │    │
            │   └───────────────────────────────────┘    │
            └─────────────────────────────────────────────┘
```

- **`crates/core`** — the sovereign core. `#![no_std]`, zero dependencies, zero
  I/O, zero allocation. Its heart is `fsm::decide(state, telemetry, cfg, now)`:
  a total, deterministic, panic-free function from an observation to an action.
- **ports** (`core::ports`) — the traits the core needs from the world
  (`TelemetrySource`, `Actuator`, `Notifier`). Contracts only.
- **adapters** *(next milestone)* — implement the ports over SSH/ntfy. The only
  code allowed to touch the network, the disk, or a clock.
- **app** *(next milestone)* — the composition root: injects adapters into the
  core and runs the bounded supervision loop.

## The decision core, in one paragraph

Each cycle, telemetry is collapsed into the single most urgent `FailureClass`
(WAN → process → path → DNS, by causal depth). The state machine then applies
**hysteresis** (never react to one blip — a fault must be confirmed N cycles),
**escalation** (climb a fixed, finite ladder of remedies, least-disruptive
first: switch node → restart → bounce WAN → failover → reboot), and **cooldown
with backoff** (never thrash). Recovery is honored immediately at any point.

## How this maps to the set of principles

| the principle | Where it lives in the code |
|---|---|
| 0 · Code is a liability | One crate, one job; `[dependencies]` is empty and must stay so. |
| I · Agnostic core | `core` is `no_std`; all I/O is injected via `ports` traits. |
| I · Dependency quarantine | Third-party code (later) only ever behind an adapter. |
| II · Invalid states unrepresentable | Subsystems are enums, not bool soups; `State` is an ADT. |
| II · Zero global state | `decide` takes state + config explicitly; no statics. |
| II · ACID / rollback | `Actuator` returns `Ok` only after verifying the post-condition. |
| III · Compile-time enforcement | `#![forbid(unsafe_code)]`; clippy denies unwrap/panic/index/overflow. |
| III · Bounded execution | Fixed node array; finite escalation ladder; saturating math. |
| III · Graceful degradation | `Unknown` readings never trigger action; notify failures are non-fatal. |
| III · No panic | Errors are `Result` values; no `unwrap`/`expect`/`panic` anywhere. |
| IV · Justified optimization | Rust now; a Zig on-router agent only when profiling demands it. |
| V · Reproducible builds | `rust-toolchain.toml` pins the compiler; `Cargo.lock` pins the rest. |
| V · Zero-touch gates | CI: `fmt --check` + `clippy -D warnings` + `test`, all blocking. |

## Build & verify

```sh
cargo fmt --all --check          # formatting is not negotiable
cargo clippy --all-targets -- -D warnings   # warnings are errors
cargo test --all                 # deterministic unit tests
```

## Language split (where Zig is rational)

Rust owns the brain: its type system is what makes "invalid states won't
compile" real. Zig's rational place is a future **on-router native agent** — a
tiny static aarch64 binary that snapshots telemetry with zero allocation and
cross-compiles without an external toolchain. Per the principle we do **not** build it
until the shell-over-SSH telemetry adapter is proven insufficient; adding a second
language now would be unjustified complexity.

## Status

- [x] Sovereign core: domain, failure classifier, supervisor FSM, ports, tests.
- [ ] Adapters: SSH telemetry + actuator, ntfy notifier.
- [ ] App: composition root + bounded loop.
- [ ] Router access via Tailscale (removes the network-dependency for control).
- [ ] (Later, if justified) Zig on-router telemetry agent.
