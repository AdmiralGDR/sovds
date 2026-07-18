//! I/O bridges for the sovereign brain.
//!
//! This crate is the only place allowed to touch the outside world, and it does
//! so exclusively through the operating system's own tools — `ssh` to reach the
//! router, `curl` to publish notifications — never through a third-party crate
//! (the internal engineering charter, the principle & I: dependency quarantine, code longevity). Each module
//! implements one of the contracts declared in [`sovereign_core::ports`].
//!
//! The design keeps the hostile edges thin and pure where it matters: command
//! lines are built by pure functions ([`ssh::build_remote_argv`],
//! [`notify::build_ntfy_argv`]) and untrusted bytes are validated by a pure
//! parser ([`telemetry::parse_snapshot`]) before ever reaching the core.

pub mod actuator;
pub mod backfill;
pub mod dataset;
pub mod episodes;
pub mod notify;
pub mod os;
pub mod recorder;
pub mod ssh;
pub mod summary;
pub mod telemetry;
