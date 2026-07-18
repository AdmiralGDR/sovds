//! The [`Domain`] trait — how a caller plugs their own system into the toolkit.
//!
//! `sovds` knows nothing about routers, services, or any specific system. To use
//! it, a caller implements [`Domain`] once for their world, declaring three types
//! and how they serialise:
//!
//! * [`Domain::Obs`] — one observation of the system (its telemetry).
//! * [`Domain::Fault`] — the vocabulary of faults (the label space); one value is
//!   [`Domain::healthy`], meaning "no fault".
//! * [`Domain::Annot`] — any domain-specific extras to carry per record (e.g. a
//!   supervisor's decision), serialised alongside the observation.
//!
//! Serialisation is token-based and allocation-free: a record line is a sequence
//! of ` key=value` tokens. The toolkit owns the *framing* tokens (time, provenance,
//! the diagnosed fault, the injection label); the domain owns its `Obs`/`Annot`
//! tokens, writing them (each with a leading space) and parsing them back through a
//! small [`Domain::Builder`] accumulator. This keeps the whole record format
//! self-describing and order-tolerant (the principle: honest, hostile-input-safe), while
//! the domain stays the single source of truth for what its own fields mean.

use core::fmt;

/// Why a serialized record token could not be parsed. Kind plus the offending line
/// (the caller's context) is enough to reproduce (the principle: errors are never
/// silent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// A value did not parse, or a `key=value` token was malformed.
    BadField,
    /// A required field was absent when finalising.
    MissingField,
    /// A token key belonged to no one — neither the toolkit nor the domain.
    UnknownKey,
}

/// Whether a domain consumed a token offered to it during parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    /// The domain recognised the key and folded the value into its builder.
    Consumed,
    /// The key is not one of the domain's — the toolkit will reject it.
    Unknown,
}

/// A system plugged into the toolkit: its observation type, its fault vocabulary,
/// and how both serialise into the record line.
///
/// Implementations are pure and allocation-free — no clock, no I/O (the principle). The
/// toolkit drives them; they never reach outward themselves.
///
/// The domain type itself is a zero-size tag; the supertraits (`Copy + Eq +
/// Debug`) are what let [`crate::Record<D>`](crate::Record) derive its own — trivial
/// to satisfy with `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` on the marker.
pub trait Domain: Copy + Eq + core::fmt::Debug {
    /// One observation of the system (its telemetry). `Copy` and heap-free so a
    /// record stays a plain value.
    type Obs: Copy + PartialEq;
    /// The fault vocabulary — the label space a record classifies into.
    type Fault: Copy + PartialEq;
    /// Domain-specific extras carried per record (e.g. a supervisor's decision).
    type Annot: Copy + PartialEq;
    /// A zero-alloc accumulator that folds parsed tokens into an `Obs`+`Annot`.
    type Builder: Default;

    /// The "no fault" value of [`Self::Fault`].
    fn healthy() -> Self::Fault;

    /// The full fault vocabulary, in a stable order — used to iterate classes when
    /// scoring per-class metrics.
    fn faults() -> &'static [Self::Fault];

    /// The stable wire spelling of a fault (also what appears after `class=` and
    /// `inject=`).
    fn fault_label(fault: Self::Fault) -> &'static str;

    /// Parse a fault back from its wire spelling; `None` if unrecognised.
    fn parse_fault(s: &str) -> Option<Self::Fault>;

    /// Write the observation's tokens, each as ` key=value` (leading space).
    ///
    /// # Errors
    /// Propagates any error from the sink `w`.
    fn write_obs<W: fmt::Write>(w: &mut W, obs: &Self::Obs) -> fmt::Result;

    /// Write the annotation's tokens, each as ` key=value` (leading space).
    ///
    /// # Errors
    /// Propagates any error from the sink `w`.
    fn write_annot<W: fmt::Write>(w: &mut W, annot: &Self::Annot) -> fmt::Result;

    /// Offer one `key=value` token to the domain during parsing. Return
    /// [`Accept::Consumed`] if it is one of the domain's fields (and fold it into
    /// `builder`), or [`Accept::Unknown`] to let the toolkit reject it.
    ///
    /// # Errors
    /// Returns [`ParseError::BadField`] if the key is the domain's but the value
    /// does not parse.
    fn accept(builder: &mut Self::Builder, key: &str, value: &str) -> Result<Accept, ParseError>;

    /// Finalise the builder into a validated `(Obs, Annot)`.
    ///
    /// # Errors
    /// Returns [`ParseError::MissingField`] if a required observation field was
    /// never seen.
    fn finish(builder: Self::Builder) -> Result<(Self::Obs, Self::Annot), ParseError>;
}

#[cfg(test)]
mod tests {
    use super::{Accept, Domain, ParseError};
    use core::fmt;

    // A minimal example domain: a service whose observation is a load percentage,
    // whose annotation is whether we acted, and whose faults are Ok/Overloaded.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Health {
        Ok,
        Overloaded,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Svc;

    #[derive(Default)]
    struct Builder {
        load: Option<u32>,
        acted: bool,
    }

    impl Domain for Svc {
        type Obs = u32; // load percent
        type Fault = Health;
        type Annot = bool; // acted?
        type Builder = Builder;

        fn healthy() -> Health {
            Health::Ok
        }
        fn faults() -> &'static [Health] {
            &[Health::Ok, Health::Overloaded]
        }
        fn fault_label(f: Health) -> &'static str {
            match f {
                Health::Ok => "ok",
                Health::Overloaded => "overloaded",
            }
        }
        fn parse_fault(s: &str) -> Option<Health> {
            match s {
                "ok" => Some(Health::Ok),
                "overloaded" => Some(Health::Overloaded),
                _ => None,
            }
        }
        fn write_obs<W: fmt::Write>(w: &mut W, obs: &u32) -> fmt::Result {
            write!(w, " load={obs}")
        }
        fn write_annot<W: fmt::Write>(w: &mut W, annot: &bool) -> fmt::Result {
            write!(w, " acted={}", u8::from(*annot))
        }
        fn accept(b: &mut Builder, key: &str, value: &str) -> Result<Accept, ParseError> {
            match key {
                "load" => {
                    b.load = Some(value.parse().map_err(|_| ParseError::BadField)?);
                    Ok(Accept::Consumed)
                }
                "acted" => {
                    b.acted = value == "1";
                    Ok(Accept::Consumed)
                }
                _ => Ok(Accept::Unknown),
            }
        }
        fn finish(b: Builder) -> Result<(u32, bool), ParseError> {
            Ok((b.load.ok_or(ParseError::MissingField)?, b.acted))
        }
    }

    #[test]
    fn a_domain_round_trips_its_obs_and_annot() -> Result<(), ParseError> {
        // Serialise obs+annot, then parse the tokens back through the builder.
        let mut line = String::new();
        // Writing into a String cannot fail.
        let _ = Svc::write_obs(&mut line, &73);
        let _ = Svc::write_annot(&mut line, &true);
        assert_eq!(line, " load=73 acted=1");

        let mut b = Builder::default();
        for tok in line.split_whitespace() {
            let (k, v) = tok.split_once('=').ok_or(ParseError::BadField)?;
            assert_eq!(Svc::accept(&mut b, k, v)?, Accept::Consumed);
        }
        assert_eq!(Svc::finish(b)?, (73, true));
        Ok(())
    }

    #[test]
    fn an_unknown_key_is_reported_not_consumed() -> Result<(), ParseError> {
        let mut b = Builder::default();
        assert_eq!(Svc::accept(&mut b, "mystery", "9")?, Accept::Unknown);
        Ok(())
    }

    #[test]
    fn faults_round_trip_through_their_labels() {
        for &f in Svc::faults() {
            assert_eq!(Svc::parse_fault(Svc::fault_label(f)), Some(f));
        }
        assert_eq!(Svc::parse_fault("bogus"), None);
        assert_eq!(Svc::healthy(), Health::Ok);
    }
}
