// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable, bounded recorder: append records to a rotating log file (behind the
//! `std` feature).
//!
//! Two disciplines shape it (the internal engineering charter):
//! * **Atomicity, no partial writes (the principle):** a record is serialized in full,
//!   then written with a single `write_all` to an `O_APPEND` file. A whole line
//!   lands or nothing does — a crash mid-run can never leave a half-record.
//! * **Bounded (the principle):** the file is capped and rotated across a fixed number
//!   of generations, so the trail can grow forever in time without ever growing
//!   without bound on disk.
//!
//! Recording is a courtesy to the future, not a load-bearing dependency: failures
//! are returned as values for the caller to log and shrug off (the principle).

use crate::domain::Domain;
use crate::record::{write_record, Record};
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
/// reached and keeping `generations` old files. One recorder serves any [`Domain`].
#[derive(Debug, Clone)]
pub struct Recorder {
    path: PathBuf,
    max_bytes: u64,
    generations: u32,
}

impl Recorder {
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
    pub fn append<D: Domain>(&self, record: &Record<D>) -> Result<(), RecordError> {
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
#[must_use]
pub fn suffixed(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::{suffixed, Recorder};
    use crate::record::Record;
    use crate::testkit::{rec, Health, Svc};

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sovds-rec-{}-{name}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(suffixed(&p, 1));
        p
    }

    #[test]
    fn appends_one_line_per_record() {
        let path = temp("append");
        let r = Recorder::new(path.clone(), 1_000_000, 3);
        let a: Record<Svc> = rec(1, 100, Health::Ok);
        let b: Record<Svc> = rec(2, 160, Health::Overloaded);
        assert!(r.append(&a).is_ok());
        assert!(r.append(&b).is_ok());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("t=1"));
        assert!(body.contains("class=overloaded"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotates_when_the_cap_is_exceeded() {
        let path = temp("rotate");
        let r = Recorder::new(path.clone(), 10, 2); // tiny cap forces rotation
        let _ = r.append::<Svc>(&rec(1, 100, Health::Ok));
        let _ = r.append::<Svc>(&rec(2, 160, Health::Ok));
        let rotated = suffixed(&path, 1);
        assert!(
            rotated.exists(),
            "the first record should have rotated to .1"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }
}
