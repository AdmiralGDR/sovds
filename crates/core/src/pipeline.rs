//! One cycle of pure decision logic.
//!
//! Folds a telemetry observation into the adaptive models, lets the learned
//! verdict refine the raw signal, classifies the result, and decides — with no
//! I/O and no side effect beyond advancing the passed-in [`Fleet`]. The live
//! supervisor and the offline replay tool both call this, so they reason
//! *identically* (one source of decision truth — DRY).

use crate::domain::{Telemetry, Tick, Tunnel};
use crate::failure::{classify, FailureClass};
use crate::fleet::{refine_tunnel, Fleet, NodeVerdict};
use crate::fsm::{decide, Action, Config, Reason, State};

/// The pure result of one supervision cycle (no actions applied yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleOutcome {
    pub next_state: State,
    pub reason: Reason,
    pub action: Action,
    /// The adaptive verdict on the active path this cycle.
    pub verdict: NodeVerdict,
    /// The fault the refined observation classified to.
    pub failure: FailureClass,
}

/// Run one cycle against `observed`: update `fleet`, refine the tunnel signal by
/// the path's learned baseline, classify, and decide. Deterministic; the only
/// effect is advancing `fleet`.
#[must_use]
pub fn decide_cycle(
    state: State,
    fleet: &mut Fleet,
    observed: &Telemetry,
    cfg: &Config,
    now: Tick,
) -> CycleOutcome {
    let path_latency = match observed.tunnel {
        Tunnel::Healthy { latency } | Tunnel::Degraded { latency } => Some(latency),
        Tunnel::Dead | Tunnel::Unknown => None,
    };
    let verdict = fleet.observe_path(path_latency);
    fleet.observe_nodes(&observed.nodes);
    let refined = Telemetry {
        tunnel: refine_tunnel(observed.tunnel, verdict),
        ..*observed
    };

    let failure = classify(&refined, cfg.degraded_above);
    let transition = decide(state, &refined, cfg, now);

    CycleOutcome {
        next_state: transition.next,
        reason: transition.reason,
        action: transition.action,
        verdict,
        failure,
    }
}
