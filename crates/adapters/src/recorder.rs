//! Durable, bounded recorder: append supervision records to a rotating log file.
//!
//! This is the I/O half of the audit/training trail; the record *format* is
//! sovereign core (`sovereign_core::record`). Two set of principles disciplines shape it:
//!
//! * **Atomicity, no partial writes (the principle):** a record is serialized in full,
//!   then written with a single `write_all` to an `O_APPEND` file. A whole line
//!   lands or nothing does — a crash mid-run can never leave a half-record.
//! * **Bounded (the principle + longevity):** the file is capped and rotated across a
//!   fixed number of generations, so the trail can grow forever in time without
//!   ever growing without bound on disk.
//!
//! Recording is a courtesy to the future, not a load-bearing dependency: failures
//! are returned as values for the caller to log and shrug off (the principle).

use sovereign_core::record::{write_record, Record};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Why a record could not be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordError {
    /// The record could not be serialized (a sink failure — practically unreachable).
    Format,
    /// The filesystem rejected the write.
    Io,
}

/// Appends records to `path`, rotating to `path.1 .. path.N` once `max_bytes` is
/// reached and keeping `generations` old files.
#[derive(Debug, Clone)]
pub struct FileRecorder {
    path: PathBuf,
    max_bytes: u64,
    generations: u32,
}

impl FileRecorder {
    /// `generations` is coerced to at least 1 so rotation always has somewhere to
    /// move the current file.
    #[must_use]
    pub fn new(path: PathBuf, max_bytes: u64, generations: u32) -> Self {
        Self {
            path,
            max_bytes,
            generations: generations.max(1),
        }
    }

    /// Serialize and durably append one record.
    ///
    /// # Errors
    /// Returns [`RecordError`] if serialization or the filesystem write fails.
    pub fn append(&self, record: &Record) -> Result<(), RecordError> {
        // Build the entire line first: the write below is all-or-nothing.
        let mut line = String::new();
        write_record(&mut line, record).map_err(|_| RecordError::Format)?;
        line.push('\n');

        self.rotate_if_full(line.len() as u64);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|_| RecordError::Io)?;
        // A single write_all to an O_APPEND file is the atomic-append primitive.
        file.write_all(line.as_bytes())
            .map_err(|_| RecordError::Io)?;
        file.flush().map_err(|_| RecordError::Io)?;
        Ok(())
    }

    /// Rotate if the incoming line would push the file past its cap. A rotation
    /// failure is deliberately swallowed: it is better to keep recording into an
    /// oversized file than to stop recording (graceful degradation).
    fn rotate_if_full(&self, incoming: u64) {
        let current = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if current.saturating_add(incoming) <= self.max_bytes {
            return;
        }
        // Shift generations from oldest to newest: path.(N-1) -> path.N, ...,
        // path -> path.1. `rename` overwrites, so the oldest is discarded.
        for generation in (1..=self.generations).rev() {
            let destination = suffixed(&self.path, generation);
            let source = match generation.checked_sub(1) {
                Some(0) | None => self.path.clone(),
                Some(previous) => suffixed(&self.path, previous),
            };
            if source.exists() {
                let _ = std::fs::rename(&source, &destination);
            }
        }
    }
}

/// `path` with a `.N` suffix appended.
pub(crate) fn suffixed(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::{FileRecorder, RecordError};
    use sovereign_core::domain::{Dns, LatencyMs, Proc, Telemetry, Tick, Tunnel, Wan, MAX_NODES};
    use sovereign_core::failure::FailureClass;
    use sovereign_core::fleet::NodeVerdict;
    use sovereign_core::fsm::{Action, Reason, State};
    use sovereign_core::record::{Outcome, Record};
    use std::path::PathBuf;

    fn record(tick: u64) -> Record {
        Record {
            tick: Tick(tick),
            wall_time: 1_700_000_000_u64.saturating_add(tick),
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

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sovereign-test-{}-{name}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(super::suffixed(&p, 1));
        p
    }

    #[test]
    fn appends_one_line_per_record() -> Result<(), RecordError> {
        let path = temp_path("append");
        let rec = FileRecorder::new(path.clone(), 1_000_000, 3);
        rec.append(&record(1))?;
        rec.append(&record(2))?;
        let body = std::fs::read_to_string(&path).map_err(|_| RecordError::Io)?;
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("t=1"));
        assert!(body.contains("t=2"));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn rotates_when_the_cap_is_exceeded() -> Result<(), RecordError> {
        let path = temp_path("rotate");
        // A tiny cap forces rotation after the first record.
        let rec = FileRecorder::new(path.clone(), 10, 2);
        rec.append(&record(1))?;
        rec.append(&record(2))?;
        let rotated = super::suffixed(&path, 1);
        assert!(
            rotated.exists(),
            "the first record should have rotated to .1"
        );
        let current = std::fs::read_to_string(&path).map_err(|_| RecordError::Io)?;
        assert!(
            current.contains("t=2"),
            "the newest record is in the live file"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
        Ok(())
    }
}
