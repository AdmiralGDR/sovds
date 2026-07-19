# Changelog

Notable changes to `sovds` (the Sovereign Dataset Toolkit) and the Sovereign Router
Brain reference implementation. Dates are the working timeline, not release tags.

## Unreleased

### Added
- **`sovds` — Sovereign Dataset Toolkit**: a domain-agnostic, zero-dependency,
  `no_std` library extracted from the router brain. Implement one `Domain` trait for
  your system and get the whole dataset pipeline — `record`, `episode`, `injection`
  (per-window detection + rollback safety), `stats` (EWMA, bounded CUSUM), and,
  behind the `std` feature, `dataset`, `recorder`, `readiness`.
- A runnable third-party integration example (`host_monitor`) proving the toolkit
  works on a non-router domain end to end.
- Product documentation: `crates/sovds/README.md` (integration guide),
  `docs/SOVEREIGN-FAMILY.md` (family catalogue + the deliberate licensing split),
  `LICENSE-MIT`.

### Changed
- The Sovereign Router Brain now runs **on** `sovds` (`RouterDomain: sovds::Domain`)
  instead of duplicating its logic. The wire format is defined in exactly one place;
  `core::record::Record` is `sovds::Record<RouterDomain>`. Net −1837 lines.
- Record layout is now framing-first (`t ts probed class src [inject] <obs> <annot>`).
  The parser is order-tolerant, so the older layout still loads — a mixed-era corpus
  parses with zero skips.

### Fixed (earlier, pre-extraction)
- Bounded CUSUM: a latched throttle alarm that mislabelled a recovered path.
- Per-window injection detection: an honest metric replacing a misleading per-record
  rate.
- Honest dry-run outcomes (`outcome=dryrun`), so observe-only runs never claim a
  simulated action was verified.
- Window-level rollback-safety monitor (recovered / stuck / pending).

### Invariants held throughout
- `#![forbid(unsafe_code)]`; lints deny `unwrap`/`expect`/`panic`/indexing/overflow.
- Reproducible builds: all binaries byte-identical across clean builds.
- The live dataset corpus loads with zero unparseable lines across every change.
