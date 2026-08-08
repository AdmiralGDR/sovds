// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A minimal example domain used across the toolkit's own tests — a stand-in for
//! a third-party integration. It models a service whose observation is a load
//! percentage, whose annotation is whether a remedy was applied, and whose faults
//! are `Ok` / `Overloaded` / `Unreachable`. Test-only.

use crate::domain::{Accept, Domain, ParseError};
use crate::record::{Record, Source, Tick};
use core::fmt;

/// The example fault vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Overloaded,
    Unreachable,
}

/// The example domain tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Svc;

/// Parse accumulator for [`Svc`].
#[derive(Default)]
pub struct Builder {
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
        &[Health::Ok, Health::Overloaded, Health::Unreachable]
    }
    fn fault_label(f: Health) -> &'static str {
        match f {
            Health::Ok => "ok",
            Health::Overloaded => "overloaded",
            Health::Unreachable => "unreachable",
        }
    }
    fn parse_fault(s: &str) -> Option<Health> {
        match s {
            "ok" => Some(Health::Ok),
            "overloaded" => Some(Health::Overloaded),
            "unreachable" => Some(Health::Unreachable),
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
    fn signal_names() -> &'static [&'static str] {
        &["load", "over"]
    }
    fn signals(obs: &u32, sink: &mut dyn FnMut(i64)) {
        sink(i64::from(*obs));
        sink(i64::from(*obs > 50));
    }
}

/// Build a test record of `class` at `tick`/`wall`, healthy load, organic.
pub fn rec(tick: u64, wall: u64, class: Health) -> Record<Svc> {
    Record {
        tick: Tick(tick),
        wall_time: wall,
        probed: true,
        obs: 10,
        class,
        annot: false,
        source: Source::Live,
        label: None,
    }
}
