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

### Fixed
- `classify` now treats a *measured* WAN blackhole (`wanok=0` on a live
  session) as `WanDown`. Omitted/unknown `wanok` is still not a fault.
- WAN reachability probe no longer uses HTTP to 1.1.1.1/8.8.8.8 (TSPU RSTs
  those on a working line). It uses HTTPS to a domestic resolver. curl 0 or a
  TLS failure after the TCP handshake (35/51/60) counts as reachable: the
  handshake completed, nothing more is claimed.
- `sovereign-autoinject` used the same plain-HTTP check as its health gate, so
  on a TSPU line it never injected. It now uses the check above; the probe
  address is `SOVEREIGN_INJECT_PROBE` (default `77.88.8.8`), and any other
  result still skips the run.
- `wan-selfheal` referenced an unset `$next` under `set -u` and aborted on its
  last log line when TCP stayed dead.
- `dns-selfheal` / `restart_singbox` pin community rule-sets to flash
  before restarting, so a GitHub fetch cannot crash-loop sing-box.
- `podkop-selfheal` still rewrites local rule-sets and starts sing-box
  when the subscription fetch itself fails (TCP dead).
- New on-router `wan-selfheal` (cron `*/2`): fail-closed TCP probe, finite
  ladder (PPPoE bounce → PHY bounce of the WAN NIC → local rule-sets +
  start). Does not reboot. Rate-limited on `/proc/uptime`.

### Added
- Offline checks for the on-router scripts (`deploy/router/tests`, run by
  `scripts/test-router.sh` inside a network namespace with every router
  command stubbed), plus shellcheck, in CI.
- CI job that builds the Lean spec in `proofs/` with the toolchain pinned in
  `proofs/lean-toolchain`.
- Optional L1/PPPoE-phase telemetry (`link`, `linkmbps`, `ppp`). Old records omit
  them and still parse as unmeasured.
- `sovereign-train --inject` trains on ground-truth `inject=` labels (organic
  rows dropped) so the model is not a copy of the rules.
- Lean 4 spec of `classify`: WAN outranks shallower faults; omitted aux is not a
  measured fault (so the corpus cannot authorize a WAN-blackhole rule).
- Community rule-sets are kept on flash and re-pinned after a podkop regenerate,
  so sing-box can start without a GitHub fetch (that fetch deadlocks when the
  tunnel is down).
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
- Clash API probes from on-router scripts and SSH-wrapped actuators resolve the
  listen address from UCI at runtime (no hardcoded management-plane address).
- Probe buffer is mode `600` after each write.
- Behaviour change: the default static degrade budget is 1500 ms (was 800).
  At 800 ms the slower exit's ordinary RTT was labelled `node_degraded`. Set
  `SOVEREIGN_DEGRADED_MS=800` to keep the old threshold.
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
