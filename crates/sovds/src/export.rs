//! Flat tabular export — turn a corpus into a TSV feature table any tool can read.
//!
//! The record format is self-describing but not rectangular (variable `nodes=`,
//! domain-specific tokens), which is awkward for pandas / DuckDB / a training
//! pipeline. This module flattens each record into a fixed set of columns:
//!
//! ```text
//! t  ts  probed  class  inject  <the domain's scalar signals…>
//! ```
//!
//! Columns are the framing fields (time, provenance-agnostic id, the diagnosed
//! class, the ground-truth injection label) followed by the observation's raw
//! numeric signals ([`Domain::signal_names`]). No feature engineering happens here
//! — only raw values are exposed; deriving features is the model's job, not the
//! toolkit's (the principle). The row writers are `no_std` (`core::fmt::Write`); the
//! `String`/`Vec` convenience sits behind the `std` feature.

use crate::domain::Domain;
use crate::record::Record;
use core::fmt::Write;

/// Write the tab-separated header row (no trailing newline handling beyond the
/// single `\n` it appends) for domain `D`.
///
/// # Errors
/// Propagates any error from the sink `w`.
pub fn write_header<D: Domain, W: Write>(w: &mut W) -> core::fmt::Result {
    w.write_str("t\tts\tprobed\tclass\tinject")?;
    for name in D::signal_names() {
        write!(w, "\t{name}")?;
    }
    w.write_char('\n')
}

/// Write one record as a tab-separated row (framing columns, then the domain's
/// signal values in `signal_names` order).
///
/// # Errors
/// Propagates any error from the sink `w`.
pub fn write_row<D: Domain, W: Write>(w: &mut W, r: &Record<D>) -> core::fmt::Result {
    write!(
        w,
        "{}\t{}\t{}\t{}\t{}",
        r.tick.0,
        r.wall_time,
        u8::from(r.probed),
        D::fault_label(r.class),
        r.label.map_or("-", |l| D::fault_label(l)),
    )?;
    // Signals are emitted last, so the closure's mutable borrow of `w` ends before
    // the trailing newline. Any write error is captured and surfaced.
    let mut err: Option<core::fmt::Error> = None;
    D::signals(&r.obs, &mut |v| {
        if err.is_none() {
            if let Err(e) = write!(w, "\t{v}") {
                err = Some(e);
            }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    w.write_char('\n')
}

/// Render a whole corpus as a TSV string (header + one row per record).
#[cfg(feature = "std")]
#[must_use]
pub fn to_tsv<D: Domain>(records: &[Record<D>]) -> String {
    let mut out = String::new();
    let _ = write_header::<D, _>(&mut out); // writing to a String cannot fail
    for r in records {
        let _ = write_row(&mut out, r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{to_tsv, write_header};
    use crate::record::Record;
    use crate::testkit::{rec, Health, Svc};

    #[test]
    fn header_lists_framing_then_signal_columns() {
        let mut h = String::new();
        let _ = write_header::<Svc, _>(&mut h);
        // testkit's Svc exposes signals `load` and `over`.
        assert_eq!(h, "t\tts\tprobed\tclass\tinject\tload\tover\n");
    }

    #[test]
    fn a_record_flattens_to_framing_plus_signal_values() {
        // rec() builds obs=10 (load), class from `class` arg; give it an injection
        // label to check that column too.
        let mut r: Record<Svc> = rec(3, 200, Health::Overloaded);
        r.label = Some(Health::Overloaded);
        let rows = to_tsv::<Svc>(&[r]);
        let mut lines = rows.lines();
        assert_eq!(
            lines.next(),
            Some("t\tts\tprobed\tclass\tinject\tload\tover")
        );
        // obs load=10 -> signal load=10, over=(10>50)=0.
        assert_eq!(
            lines.next(),
            Some("3\t200\t1\toverloaded\toverloaded\t10\t0")
        );
    }
}
