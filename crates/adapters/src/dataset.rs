//! Dataset loader: read a rotating record log back into memory as a corpus.
//!
//! The inverse end of the recorder: it walks the rotated generations oldest-first
//! and then the live file, parsing each line through the sovereign boundary guard
//! ([`parse_record`]). Corrupt or truncated lines are counted and skipped, never
//! fatal (the principle: graceful degradation — a single bad line must not lose the
//! whole corpus). This is offline analysis on the laptop, so `std`/`Vec` are fine
//! here (the no-alloc discipline binds the on-device core, not dev tooling).

use crate::recorder::suffixed;
use sovereign_core::record::{parse_record, Record};
use std::path::{Path, PathBuf};

/// A loaded corpus of records plus provenance counts.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// Records in chronological order (oldest rotated file first).
    pub records: Vec<Record>,
    /// Non-empty lines that failed to parse and were skipped.
    pub skipped: usize,
    /// How many log files were actually read.
    pub files: usize,
}

/// Load `path` and up to `generations` rotated siblings (`path.N` .. `path.1`,
/// then `path`), in chronological order. Missing files are silently skipped.
#[must_use]
pub fn load(path: &Path, generations: u32) -> Corpus {
    let mut ordered: Vec<PathBuf> = (1..=generations).rev().map(|g| suffixed(path, g)).collect();
    ordered.push(path.to_path_buf());

    let mut records = Vec::new();
    let mut skipped = 0usize;
    let mut files = 0usize;

    for file in &ordered {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue; // missing/unreadable generation — not an error
        };
        files = files.saturating_add(1);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match parse_record(trimmed) {
                Ok(record) => records.push(record),
                Err(_) => skipped = skipped.saturating_add(1),
            }
        }
    }

    Corpus {
        records,
        skipped,
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::recorder::FileRecorder;
    use sovereign_core::domain::{Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use sovereign_core::failure::FailureClass;
    use sovereign_core::fleet::NodeVerdict;
    use sovereign_core::fsm::{Action, Reason, State};
    use sovereign_core::record::{Outcome, Record};
    use std::path::PathBuf;

    fn record(tick: u64) -> Record {
        Record {
            tick: Tick(tick),
            probed: true,
            telemetry: Telemetry {
                wan: Wan::Up { uptime_s: 1 },
                singbox: Proc::Running,
                tunnel: Tunnel::Healthy {
                    latency: LatencyMs(50),
                },
                dns: Dns::Ok,
                nodes: [None; MAX_NODES],
            },
            path_verdict: NodeVerdict::Healthy,
            failure: FailureClass::None,
            state: State::Healthy,
            reason: Reason::Nominal,
            action: Action::Observe,
            outcome: Outcome::NotActed,
        }
    }

    fn temp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sovereign-ds-{}-{name}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(crate::recorder::suffixed(&p, 1));
        p
    }

    #[test]
    fn loads_records_across_a_rotation_in_order() {
        let path = temp("rotate");
        // Tiny cap forces the first record into a rotated file.
        let rec = FileRecorder::new(path.clone(), 10, 2);
        let _ = rec.append(&record(1));
        let _ = rec.append(&record(2));

        let corpus = load(&path, 2);
        assert_eq!(corpus.records.len(), 2);
        // Oldest (rotated) first.
        assert_eq!(corpus.records.first().map(|r| r.tick), Some(Tick(1)));
        assert_eq!(corpus.records.last().map(|r| r.tick), Some(Tick(2)));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::recorder::suffixed(&path, 1));
    }

    #[test]
    fn skips_corrupt_lines_without_losing_the_rest() {
        let path = temp("corrupt");
        let rec = FileRecorder::new(path.clone(), 1_000_000, 1);
        let _ = rec.append(&record(1));
        // Manually append a garbage line.
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
            let _ = f.write_all(b"this is not a record\n");
        }
        let _ = rec.append(&record(2));

        let corpus = load(&path, 1);
        assert_eq!(corpus.records.len(), 2);
        assert_eq!(corpus.skipped, 1);

        let _ = std::fs::remove_file(&path);
    }
}
