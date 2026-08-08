// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Errors as values (no panic, no silent swallow).
//!
//! Every fallible boundary in the system returns one of these enums instead of
//! unwinding. Each variant names a distinct, reproducible failure condition so a
//! log line carrying it is enough to understand what happened without a stack
//! trace. They are `Copy` and carry no allocation, so they cross the `no_std`
//! boundary freely.

/// Why a telemetry probe could not produce a trustworthy sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProbeError {
    /// The transport (e.g. SSH) could not be established.
    Unreachable,
    /// A response arrived but did not parse into a valid domain value; the raw
    /// bytes were rejected at the boundary rather than trusted.
    Malformed,
    /// The probe exceeded its hard deadline (bounded execution).
    Timeout,
}

/// Why a remediation could not be applied or, having been applied, could not be
/// verified. An `Ok` from an actuator means the post-condition was checked, so
/// these variants are the only honest ways it may fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActError {
    /// The transport failed before the command could take effect.
    Unreachable,
    /// The command was dispatched but its post-condition did not hold — the
    /// change must be treated as not applied (no partial writes).
    Unverified,
    /// The actuation exceeded its hard deadline.
    Timeout,
}
