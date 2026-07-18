//! Sovereign supervision core.
//!
//! This crate is the sovereign core described in `the internal engineering charter`: pure decision
//! logic with **zero dependencies**, **zero I/O**, and **zero allocation**. It
//! cannot reach the network, the disk, or a clock — all of that is injected at
//! the edges through the contracts in [`ports`]. It compiles `no_std`, so it is
//! agnostic to where it runs (the principle: agnostic core).
//!
//! The single entry point is [`fsm::decide`]: a total, deterministic function
//! from `(state, observation)` to `(next state, action, reason)`. It never
//! panics and never blocks. Everything else in the crate is the vocabulary it
//! reasons over ([`domain`]), how it collapses an observation into a fault
//! ([`failure`]), and the errors the outside world may report ([`error`]).
//!
//! Tests link `std` (the test harness needs it); the shipped library never does.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod domain;
pub mod episode;
pub mod error;
pub mod failure;
pub mod fleet;
pub mod fsm;
pub mod pipeline;
pub mod ports;
pub mod record;
pub mod scenario;
pub mod stats;
