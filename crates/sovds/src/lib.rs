//! # sovds — Sovereign Dataset Toolkit
//!
//! A domain-agnostic toolkit for building **labelled, ground-truth datasets from
//! live systems**: collect telemetry, deliberately and safely induce faults with
//! provenance labels, segment the stream into fault *episodes*, and measure
//! detection and rollback-safety — so a monitoring rule set (or a future model)
//! can be validated on real system behaviour, not guesswork.
//!
//! It was extracted from a router self-healing supervisor (the reference
//! implementation lives alongside this crate), and generalised: the machinery
//! here knows nothing about routers, VPNs, or any specific system. A caller plugs
//! in their own world through the [`Domain`] trait — their observation type, their
//! fault-class vocabulary, and how both serialise into the record line — and gets
//! the whole dataset pipeline for free.
//!
//! ## set of principles
//!
//! Built to the "this project's internal engineering principles" (`the internal engineering charter`):
//! * **Zero dependencies, agnostic core (the principle & I).** This crate has no
//!   third-party dependencies and compiles `no_std` by default; the on-disk edges
//!   live behind the `std` feature. Embed it without dragging a dependency tree.
//! * **Invalid states unrepresentable (the principle).** The record model separates
//!   provenance ([`Source`]), the diagnosed fault, and the ground-truth injection
//!   label so "we broke it on purpose" can never be confused with a real fault.
//! * **Bounded, no-panic (the principle).** `#![forbid(unsafe_code)]`; the workspace
//!   lints deny `unwrap`/`expect`/`panic`/indexing/arithmetic-overflow, so nothing
//!   here can panic or overflow on hostile input.
//!
//! ## Modules
//! * [`domain`] — the [`Domain`] trait a caller implements to plug in their system.
//! * [`record`] — the generic dataset record and its serialisation.
//! * [`stats`] — integer fixed-point online estimators (EWMA, one-sided CUSUM).
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

pub mod domain;
pub mod record;
pub mod stats;

pub use domain::Domain;
pub use record::{Record, Source, Tick};
