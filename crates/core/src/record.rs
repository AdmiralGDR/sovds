// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The router's audit / training record — now a thin instantiation of the generic
//! [`sovds::Record`] over [`RouterDomain`].
//!
//! The format used to be defined here; it now lives in exactly one place
//! ([`crate::routerdomain`]), and this module is just the router-flavoured names:
//! `Record` is `sovds::Record<RouterDomain>`, and `write_record`/`parse_record` are
//! its serialisation specialised to the router. Everything the rest of the brain
//! imported from here (`Record`, `Source`, `Outcome`, `ParseError`) still resolves.

use crate::routerdomain::RouterDomain;
use core::fmt::Write;

pub use crate::routerdomain::{Outcome, RouterAnnot};
pub use sovds::domain::ParseError;
pub use sovds::record::{Source, Tick};

/// A complete record of one supervision cycle: the router's telemetry, the
/// supervisor's decision, and the dataset framing (provenance, injection label).
pub type Record = sovds::record::Record<RouterDomain>;

/// Serialize a record as one self-describing line (no trailing newline).
///
/// # Errors
/// Propagates any error from the underlying sink `w`.
pub fn write_record<W: Write>(w: &mut W, r: &Record) -> core::fmt::Result {
    sovds::record::write_record(w, r)
}

/// Parse one serialized line back into a [`Record`] — a boundary guard: malformed
/// input is rejected, never trusted.
///
/// # Errors
/// Returns [`ParseError`] on a malformed, incomplete, or unknown-key line.
pub fn parse_record(line: &str) -> Result<Record, ParseError> {
    sovds::record::parse_record::<RouterDomain>(line)
}

#[cfg(test)]
mod tests {
    use super::{parse_record, write_record, Outcome, ParseError, Record, RouterAnnot, Source};
    use crate::domain::{
        Aux, Dns, LatencyMs, NodeHealth, NodeId, Proc, Telemetry, Tick, Tunnel, Wan, WanReach,
        MAX_NODES,
    };
    use crate::failure::FailureClass;
    use crate::fleet::NodeVerdict;
    use crate::fsm::{Action, Reason, State};

    fn sample() -> Record {
        Record {
            tick: Tick(42),
            wall_time: 1_700_000_000,
            probed: true,
            obs: Telemetry {
                wan: Wan::Up { uptime_s: 12345 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(439),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
                aux: Aux::UNKNOWN,
            },
            class: FailureClass::None,
            annot: RouterAnnot {
                verdict: NodeVerdict::Healthy,
                state: State::Healthy,
                reason: Reason::Nominal,
                action: Action::Observe,
                outcome: Outcome::NotActed,
            },
            source: Source::Live,
            label: None,
        }
    }

    #[test]
    fn round_trips_a_full_cycle() -> Result<(), ParseError> {
        let mut cooldown = sample();
        cooldown.obs.wan = Wan::Down;
        cooldown.obs.tunnel = Tunnel::Degraded {
            latency: LatencyMs(700),
        };
        cooldown.obs.dns = Dns::Failing;
        cooldown.obs.aux = Aux {
            wan_reach: WanReach::Blackhole,
            load_centi: Some(37),
            mem_free_kb: Some(151_204),
            active: Some(4),
        };
        cooldown.class = FailureClass::WanDown;
        cooldown.annot.verdict = NodeVerdict::Throttled;
        cooldown.annot.state = State::Cooldown {
            failing: FailureClass::WanDown,
            level: 2,
            until: Tick(99),
        };
        cooldown.annot.reason = Reason::Escalated {
            failing: FailureClass::WanDown,
            level: 2,
        };
        cooldown.annot.action = Action::FailoverBackup;
        cooldown.annot.outcome = Outcome::Unverified;
        cooldown.source = Source::Backfill;
        cooldown.label = Some(FailureClass::WanDown);
        let mut nodes = [None; MAX_NODES];
        if let Some(cell) = nodes.get_mut(0) {
            *cell = Some(NodeHealth {
                id: NodeId(1),
                latency: Some(LatencyMs(439)),
            });
        }
        if let Some(cell) = nodes.get_mut(1) {
            *cell = Some(NodeHealth {
                id: NodeId(2),
                latency: None,
            });
        }
        cooldown.obs.nodes = nodes;

        for original in [sample(), cooldown] {
            let mut line = String::new();
            let _ = write_record(&mut line, &original);
            let parsed = parse_record(&line)?;
            assert_eq!(parsed, original, "round-trip mismatch for: {line}");
        }
        Ok(())
    }

    #[test]
    fn rejects_hostile_input() {
        assert_eq!(parse_record("total garbage"), Err(ParseError::BadField));
        assert_eq!(parse_record("t=1 mystery=9"), Err(ParseError::UnknownKey));
        assert_eq!(parse_record("t=1 probed=1"), Err(ParseError::MissingField));
    }

    #[test]
    fn simulated_outcome_serializes_as_dryrun() {
        let mut r = sample();
        r.annot.action = Action::SwitchNode;
        r.annot.outcome = Outcome::Simulated;
        let mut line = String::new();
        let _ = write_record(&mut line, &r);
        assert!(line.contains("outcome=dryrun"));
        assert!(!line.contains("outcome=verified"));
    }

    #[test]
    fn old_records_without_optional_fields_still_load() -> Result<(), ParseError> {
        // A record written by the OLD layout (obs before class, verdict before
        // class, src/inject at the end) must still parse — order-tolerant.
        let old = "t=0 ts=1784336671 probed=1 wan=up:17992 sb=running tun=ok:223 dns=ok \
                   nodes=1:207,2:295 wanok=1 load=200 mem=242144 active=1 verdict=warming \
                   class=none state=healthy reason=nominal action=observe outcome=- src=live";
        let parsed = parse_record(old)?;
        assert_eq!(parsed.class, FailureClass::None);
        assert_eq!(parsed.obs.wan, Wan::Up { uptime_s: 17992 });
        assert_eq!(parsed.annot.action, Action::Observe);
        Ok(())
    }
}
