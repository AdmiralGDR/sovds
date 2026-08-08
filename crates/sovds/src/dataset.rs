// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dataset loader: read a rotating record log back into memory as a corpus (behind
//! the `std` feature).
//!
//! The inverse of [`crate::recorder`]: it walks the rotated generations oldest-first
//! then the live file, parsing each line through the boundary guard
//! ([`crate::record::parse_record`]). Corrupt or truncated lines are counted and
//! skipped, never fatal (a single bad line must not lose the corpus).

use crate::domain::Domain;
use crate::record::{parse_record, Record};
use crate::recorder::suffixed;
use std::path::{Path, PathBuf};

/// A loaded corpus plus provenance counts. (Not `derive`d `Debug`/`Clone`: the
/// bounds would not propagate through `Vec<Record<D>>` for an arbitrary domain;
/// callers work with the fields directly.)
pub struct Corpus<D: Domain> {
    /// Records in chronological order (oldest rotated file first).
    pub records: Vec<Record<D>>,
    /// Non-empty lines that failed to parse and were skipped.
    pub skipped: usize,
    /// How many log files were actually read.
    pub files: usize,
}

/// Load `path` and up to `generations` rotated siblings (`path.N` .. `path.1`, then
/// `path`), in chronological order. Missing files are silently skipped.
#[must_use]
pub fn load<D: Domain>(path: &Path, generations: u32) -> Corpus<D> {
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
            match parse_record::<D>(trimmed) {
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
    use crate::record::Tick;
    use crate::recorder::{suffixed, Recorder};
    use crate::testkit::{rec, Health, Svc};

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sovds-ds-{}-{name}.log", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(suffixed(&p, 1));
        p
    }

    #[test]
    fn loads_records_across_a_rotation_in_order() {
        let path = temp("rotate");
        let r = Recorder::new(path.clone(), 10, 2); // tiny cap forces rotation
        let _ = r.append::<Svc>(&rec(1, 100, Health::Ok));
        let _ = r.append::<Svc>(&rec(2, 160, Health::Ok));

        let corpus = load::<Svc>(&path, 2);
        assert_eq!(corpus.records.len(), 2);
        assert_eq!(corpus.records.first().map(|r| r.tick), Some(Tick(1)));
        assert_eq!(corpus.records.last().map(|r| r.tick), Some(Tick(2)));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(suffixed(&path, 1));
    }

    #[test]
    fn skips_corrupt_lines_without_losing_the_rest() {
        let path = temp("corrupt");
        let r = Recorder::new(path.clone(), 1_000_000, 1);
        let _ = r.append::<Svc>(&rec(1, 100, Health::Ok));
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
            let _ = f.write_all(b"this is not a record\n");
        }
        let _ = r.append::<Svc>(&rec(2, 160, Health::Ok));

        let corpus = load::<Svc>(&path, 1);
        assert_eq!(corpus.records.len(), 2);
        assert_eq!(corpus.skipped, 1);
        let _ = std::fs::remove_file(&path);
    }
}
