//! Backfill: recover telemetry from the router's RAM buffer into the dataset.
//!
//! The laptop is the system of record, but it sleeps and powers off. While it is
//! away, `sovereign-probe` (cron on the router) appends one telemetry line per
//! minute to a bounded tmpfs buffer. This module is the laptop-side half: drain
//! that buffer, validate each line through the *existing* boundary guard
//! ([`parse_snapshot`]), drop anything the dataset already covers, and hand the
//! survivors back for replay + durable append.
//!
//! Loss-free by design (the principle): the drain protocol first folds any leftover
//! `.drain` file from a previously crashed backfill together with the live
//! buffer, and the remote `.drain` is deleted only after the local append
//! succeeded. That makes delivery at-least-once — safe, because dedup makes the
//! whole operation idempotent.

use crate::os::{CommandRunner, RunError};
use crate::ssh::SshExec;
use crate::telemetry::parse_snapshot;
use sovereign_core::domain::Telemetry;
use sovereign_core::error::ProbeError;
use sovereign_core::failure::FailureClass;

/// Router-side buffer path (tmpfs — RAM only, never flash).
pub const BUFFER: &str = "/tmp/sovereign-buffer.log";

/// Two samples closer than this (seconds) are considered the same observation:
/// the live collector and the router probe both sample about once a minute, so
/// anything within ±45 s of an existing record is a duplicate, not new signal.
pub const DEDUP_WINDOW_S: u64 = 45;

/// One recovered observation: when it was taken, what was seen, and whether it
/// was labelled as a deliberate injection at the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub ts: u64,
    pub telemetry: Telemetry,
    pub injected: Option<FailureClass>,
}

/// Parse one buffer line (`ts=<unix> <telemetry tokens...>`) into a [`Sample`].
///
/// The tokens after `ts=` are exactly the snapshot contract, space-joined; they
/// are re-joined with newlines and validated by the existing [`parse_snapshot`]
/// boundary guard — no second parser surface to maintain (DRY, the principle).
pub fn parse_buffer_line(line: &str) -> Result<Sample, ProbeError> {
    let mut ts: Option<u64> = None;
    let mut snapshot = String::new();
    for token in line.split_whitespace() {
        match token.strip_prefix("ts=") {
            Some(value) => {
                ts = Some(value.parse::<u64>().map_err(|_| ProbeError::Malformed)?);
            }
            None => {
                snapshot.push_str(token);
                snapshot.push('\n');
            }
        }
    }
    let ts = ts.ok_or(ProbeError::Malformed)?;
    let observation = parse_snapshot(&snapshot)?;
    Ok(Sample {
        ts,
        telemetry: observation.telemetry,
        injected: observation.injected,
    })
}

/// Keep only samples the dataset does not already cover: `existing` must be the
/// sorted wall-clock stamps of every record on disk. A sample within
/// [`DEDUP_WINDOW_S`] of any of them is discarded. This is what makes repeated
/// backfills (and overlap with the live collector) idempotent.
#[must_use]
pub fn dedupe(samples: Vec<Sample>, existing: &[u64]) -> Vec<Sample> {
    samples
        .into_iter()
        .filter(|sample| !covered(sample.ts, existing))
        .collect()
}

/// Whether `ts` falls within the dedup window of any timestamp in sorted `existing`.
fn covered(ts: u64, existing: &[u64]) -> bool {
    // partition_point gives the first index with value > ts; the nearest existing
    // stamps are at idx-1 and idx. Checking both neighbours suffices on sorted data.
    let idx = existing.partition_point(|&t| t <= ts);
    let after = existing.get(idx).copied();
    let before = idx.checked_sub(1).and_then(|i| existing.get(i).copied());
    let near = |other: Option<u64>| other.is_some_and(|t| ts.abs_diff(t) <= DEDUP_WINDOW_S);
    near(before) || near(after)
}

/// Drain the router buffer: fold any leftover from a crashed previous drain
/// together with the live buffer into `.drain`, truncate the live buffer, and
/// return the raw lines. The `.drain` file is left on the router until
/// [`confirm_drained`] — so a failure after this point loses nothing.
pub fn drain<R: CommandRunner>(ssh: &SshExec<'_, R>) -> Result<Vec<String>, RunError> {
    let script = format!(
        "touch {b}; cat {b}.drain {b} 2>/dev/null > {b}.all; \
         mv {b}.all {b}.drain; : > {b}; cat {b}.drain",
        b = BUFFER
    );
    let output = ssh.exec(&script)?;
    Ok(output
        .stdout_text()
        .lines()
        .map(str::to_string)
        .filter(|l| !l.trim().is_empty())
        .collect())
}

/// Delete the router-side `.drain` file — call only after the drained samples
/// have been durably appended to the local dataset (the principle: the hand-off
/// completes atomically or the data stays on the router for the next attempt).
pub fn confirm_drained<R: CommandRunner>(ssh: &SshExec<'_, R>) -> Result<(), RunError> {
    ssh.exec(&format!("rm -f {b}.drain", b = BUFFER))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{dedupe, parse_buffer_line, Sample, DEDUP_WINDOW_S};
    use sovereign_core::domain::{Aux, Dns, LatencyMs, Proc, Telemetry, Tunnel, Wan, MAX_NODES};
    use sovereign_core::error::ProbeError;

    const LINE: &str =
        "ts=1784318527 wan=up:36478 singbox=running tunnel=ok:163 dns=ok node=1:164 node=2:252";

    fn sample_at(ts: u64) -> Sample {
        Sample {
            ts,
            telemetry: Telemetry {
                wan: Wan::Up { uptime_s: 1 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(50),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
                aux: Aux::UNKNOWN,
            },
            injected: None,
        }
    }

    #[test]
    fn parses_a_full_buffer_line() -> Result<(), ProbeError> {
        let s = parse_buffer_line(LINE)?;
        assert_eq!(s.ts, 1_784_318_527);
        assert_eq!(s.telemetry.wan, Wan::Up { uptime_s: 36478 });
        assert_eq!(
            s.telemetry.tunnel,
            Tunnel::Healthy {
                latency: LatencyMs(163)
            }
        );
        assert_eq!(s.telemetry.nodes.iter().flatten().count(), 2);
        Ok(())
    }

    #[test]
    fn rejects_a_line_without_a_timestamp() {
        let no_ts = "wan=up:1 singbox=running tunnel=ok:1 dns=ok";
        assert_eq!(parse_buffer_line(no_ts), Err(ProbeError::Malformed));
    }

    #[test]
    fn rejects_garbled_telemetry_tokens() {
        let bad = "ts=5 wan=sideways singbox=running tunnel=ok:1 dns=ok";
        assert_eq!(parse_buffer_line(bad), Err(ProbeError::Malformed));
    }

    #[test]
    fn dedupe_drops_covered_and_keeps_gap_samples() {
        // Existing dataset covers minutes 1000, 1060, 1120.
        let existing = vec![1000, 1060, 1120];
        let samples = vec![
            sample_at(1010),                      // within 45s of 1000 -> dropped
            sample_at(1060),                      // exact duplicate -> dropped
            sample_at(1200),                      // 80s past the last -> kept
            sample_at(5000),                      // deep gap -> kept
            sample_at(1120 + DEDUP_WINDOW_S),     // boundary: exactly window -> dropped
            sample_at(1120 + DEDUP_WINDOW_S + 1), // just outside -> kept
        ];
        let kept: Vec<u64> = dedupe(samples, &existing).iter().map(|s| s.ts).collect();
        assert_eq!(kept, vec![1200, 5000, 1120 + DEDUP_WINDOW_S + 1]);
    }

    #[test]
    fn dedupe_with_empty_dataset_keeps_everything() {
        let samples = vec![sample_at(10), sample_at(20)];
        assert_eq!(dedupe(samples, &[]).len(), 2);
    }
}
