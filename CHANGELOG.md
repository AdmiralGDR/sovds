# Changelog

Notable changes to `sovds` (the Sovereign Dataset Toolkit) and the Sovereign Router
Brain reference implementation. Dates are the working timeline, not release tags.

## Unreleased

### Security
- SSH host keys are now checked. The bridge used to run with
  `StrictHostKeyChecking=no` and `/dev/null` known_hosts, so anyone who could
  answer on the router address got the root password. `SshConfig::host_key`
  selects strict known_hosts (default, `SOVEREIGN_KNOWN_HOSTS`), a pinned key
  (`SOVEREIGN_HOST_KEY`), or the old behaviour behind
  `SOVEREIGN_SSH_INSECURE_ACCEPT_ANY_HOST_KEY=1`. A rejected key returns
  `RunError::HostKeyRejected` and prints how to fix it.
- Public-key auth is tried first; `SOVEREIGN_SSH_IDENTITY` passes a key with
  `IdentitiesOnly=yes`. The askpass password is only a fallback, and password
  auth is off (`BatchMode=yes`) when no askpass is set.
- The insecure mode never offers the password: without a verified host key
  only key auth is used, so an impostor cannot collect the root password.
- A valid `SOVEREIGN_HOST_KEY` wins over the insecure switch, and a malformed
  pin is reported on stderr instead of being dropped silently.
- Breaking: pin the router key (see `deploy/README.md`) before upgrading, or
  every SSH call is refused.

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
  `CONTRIBUTING.md`, and both licence texts (`LICENSE-MIT`, `LICENSE-APACHE`).
- An external-consumer integration test (`crates/sovds/tests/integration.rs`) that
  builds the whole pipeline for an unrelated domain through only the public API.
- CI gates for the set of principles's sovereignty the principles: `no_std` core build and the
  byte-identical reproducible-build check.

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
