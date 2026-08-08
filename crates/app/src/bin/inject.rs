// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `sovereign-inject` — mark a window of records as a *deliberate* fault so the
//! dataset can tell "we induced this on purpose" from a genuine problem.
//!
//! The empty dataset classes (wan_down, singbox_dead, dns_fail) never occur on a
//! healthy network, so real samples of them require inducing the fault on purpose.
//! The one thing that must never happen is confusing such a sample with an organic
//! fault. This tool writes a small marker the telemetry probes read, so every
//! record taken during the window is stamped `inject=<class>` (ground truth) and
//! is permanently distinguishable from a real fault.
//!
//! Deliberately narrow (least privilege): it only writes and clears a
//! bounded marker file — it does **not** itself disrupt any service. Arming the
//! label and causing the disruption are kept separate on purpose: the operator
//! induces the actual fault by hand during the window (or runs a dry-run to prove
//! the labelling loop with zero disruption). That keeps a data-labelling tool from
//! doubling as a service-attack tool.
//!
//! Safety (bounded, self-healing):
//!   * the marker carries `until=<ts>`; probes stop labelling past it even if
//!     cleanup never runs, so a label can never latch;
//!   * a detached on-router timer (`setsid`) removes the marker after the window,
//!     independent of this tool, SSH, or the laptop;
//!   * the marker is in tmpfs — a router reboot clears it.
//!
//! Usage:
//!   sovereign-inject <class> [--secs N]   # arm a labelled window
//!   sovereign-inject --status             # show the current marker
//!   sovereign-inject --clear              # remove the marker now

use sovereign_adapters::os::SystemRunner;
use sovereign_adapters::ssh::SshExec;
use std::process::ExitCode;

#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
use config::AppConfig;

const MARKER: &str = "/tmp/sovereign-inject";
const DEFAULT_SECS: u64 = 120;
const MAX_SECS: u64 = 900;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = AppConfig::from_env();
    let runner = SystemRunner;
    let ssh = SshExec::new(&runner, cfg.ssh.clone(), cfg.actuator_deadline);

    if args.iter().any(|a| a == "--clear") {
        return run(
            &ssh,
            &format!("rm -f {MARKER}; echo cleared"),
            "cleared",
            |_| {
                println!("sovereign-inject: marker cleared.");
            },
        );
    }
    if args.iter().any(|a| a == "--status") {
        return run(
            &ssh,
            &format!("cat {MARKER} 2>/dev/null; echo status-done"),
            "status-done",
            |out| {
                let body = out.replace("status-done", "");
                let body = body.trim();
                if body.is_empty() {
                    println!("sovereign-inject: no active marker.");
                } else {
                    println!("sovereign-inject: active marker:\n{body}");
                }
            },
        );
    }

    let Some(raw) = args.first().filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!("usage: sovereign-inject <class> [--secs N] | --status | --clear");
        eprintln!("  classes: wan_down singbox_dead node_dead node_degraded dns_fail");
        return ExitCode::FAILURE;
    };
    let Some(class) = canonical_class(&raw) else {
        eprintln!("sovereign-inject: unknown class '{raw}'");
        return ExitCode::FAILURE;
    };
    let secs = flag_value(&args, "--secs")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS)
        .clamp(10, MAX_SECS);

    let script = format!(
        "NOW=$(date +%s); UNTIL=$((NOW+{secs})); \
         printf 'class={class}\\nuntil=%s\\n' \"$UNTIL\" > {MARKER}; \
         setsid sh -c 'sleep {secs}; rm -f {MARKER}' >/dev/null 2>&1 & echo armed"
    );
    run(&ssh, &script, "armed", |_| {
        println!("sovereign-inject: armed inject={class} for {secs}s.");
        println!("  Every record this window is stamped inject={class} (ground truth) and");
        println!("  auto-expires; induce the actual fault by hand now if collecting live data.");
        println!("  Inspect later: sovereign-episodes --list  (injected episodes are flagged).");
    })
}

/// Run a remote script, requiring `needle` in its output, then report via `ok`.
fn run(
    ssh: &SshExec<'_, SystemRunner>,
    script: &str,
    needle: &str,
    ok: impl FnOnce(&str),
) -> ExitCode {
    match ssh.exec(script) {
        Ok(out) => {
            let text = out.stdout_text();
            if text.contains(needle) {
                ok(&text);
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "sovereign-inject: unexpected router response: {}",
                    text.trim()
                );
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("sovereign-inject: router unreachable ({err:?})");
            ExitCode::FAILURE
        }
    }
}

/// Map a user-supplied class name to its canonical wire spelling.
fn canonical_class(arg: &str) -> Option<&'static str> {
    match arg {
        "wan_down" => Some("wan_down"),
        "singbox_dead" => Some("singbox_dead"),
        "node_dead" => Some("node_dead"),
        "node_degraded" => Some("node_degraded"),
        "dns_fail" => Some("dns_fail"),
        _ => None,
    }
}

/// The value following `flag` in the argument list, if present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i.saturating_add(1)))
        .map(String::as_str)
}
