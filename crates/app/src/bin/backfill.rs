//! `sovereign-backfill` — drain the router's RAM telemetry buffer into the
//! laptop dataset.
//!
//! Run periodically (systemd timer) and at boot: whatever the router observed
//! while the laptop was off or asleep is validated, deduplicated against the
//! records already on disk, replayed through the SAME pure decision pipeline the
//! live brain uses, and appended with `src=backfill`. At-least-once delivery +
//! dedup = idempotent and loss-free (the principle); every skip is counted and
//! reported, never silent (the principle).

use sovereign_adapters::backfill::{confirm_drained, dedupe, drain, parse_buffer_line, Sample};
use sovereign_adapters::dataset;
use sovereign_adapters::os::SystemRunner;
use sovereign_adapters::recorder::FileRecorder;
use sovereign_adapters::ssh::SshExec;
use sovereign_core::domain::Tick;
use sovereign_core::fleet::Fleet;
use sovereign_core::fsm::State;
use sovereign_core::pipeline::decide_cycle;
use sovereign_core::record::{Outcome, Record, RouterAnnot, Source};
use std::process::ExitCode;

// Reuse the main binary's configuration module (one source of env truth — DRY).
// dead_code is allowed here deliberately: this binary uses a subset of the
// shared config; the unused fields belong to the collector binary.
#[allow(dead_code)]
#[path = "../config.rs"]
mod config;
use config::AppConfig;

fn main() -> ExitCode {
    let cfg = AppConfig::from_env();
    let runner = SystemRunner;
    let ssh = SshExec::new(&runner, cfg.ssh.clone(), cfg.telemetry_deadline);

    // 1. Drain the buffer (leaves .drain on the router until we confirm).
    let lines = match drain(&ssh) {
        Ok(lines) => lines,
        Err(err) => {
            eprintln!("sovereign-backfill: router unreachable ({err:?}); nothing drained");
            // Not an error state: the buffer keeps accumulating for next time.
            return ExitCode::SUCCESS;
        }
    };
    if lines.is_empty() {
        eprintln!("sovereign-backfill: buffer empty; nothing to do");
        let _ = confirm_drained(&ssh);
        return ExitCode::SUCCESS;
    }

    // 2. Validate every line at the boundary; count rejects, never trust them.
    let mut samples: Vec<Sample> = Vec::new();
    let mut malformed = 0usize;
    for line in &lines {
        match parse_buffer_line(line) {
            Ok(sample) => samples.push(sample),
            Err(_) => malformed = malformed.saturating_add(1),
        }
    }
    samples.sort_by_key(|s| s.ts);

    // 3. Dedup against everything already on disk.
    let corpus = dataset::load(&cfg.log_path, cfg.log_generations);
    let mut existing: Vec<u64> = corpus
        .records
        .iter()
        .map(|r| r.wall_time)
        .filter(|&t| t > 0)
        .collect();
    existing.sort_unstable();
    let drained_total = samples.len();
    let fresh = dedupe(samples, &existing);

    // 4. Replay the survivors through the same pure pipeline the live brain
    //    runs, as one fresh "run" (tick from 0 marks the boundary). Decisions
    //    here are what the brain WOULD have done; nothing is actuated, so the
    //    outcome is honestly NotActed and the provenance is Backfill.
    let recorder = FileRecorder::new(cfg.log_path.clone(), cfg.log_max_bytes, cfg.log_generations);
    let mut fleet = Fleet::new(cfg.fleet);
    let mut state = State::Healthy;
    let mut appended = 0usize;
    let mut dropped_io = 0usize;
    for (i, sample) in fresh.iter().enumerate() {
        let tick = Tick(u64::try_from(i).unwrap_or(u64::MAX));
        let cycle = decide_cycle(state, &mut fleet, &sample.telemetry, &cfg.core, tick);
        state = cycle.next_state;
        let record = Record {
            tick,
            wall_time: sample.ts,
            probed: true,
            obs: sample.telemetry,
            class: cycle.failure,
            annot: RouterAnnot {
                verdict: cycle.verdict,
                state: cycle.next_state,
                reason: cycle.reason,
                action: cycle.action,
                outcome: Outcome::NotActed,
            },
            source: Source::Backfill,
            // Preserve the ground-truth injection label recovered from the buffer.
            label: sample.injected,
        };
        match recorder.append(&record) {
            Ok(()) => appended = appended.saturating_add(1),
            Err(_) => dropped_io = dropped_io.saturating_add(1),
        }
    }

    // 5. Confirm the hand-off only if nothing was lost locally; otherwise keep
    //    the .drain on the router so the next run retries (dedup absorbs the
    //    resulting duplicates).
    if dropped_io == 0 {
        if let Err(err) = confirm_drained(&ssh) {
            eprintln!("sovereign-backfill: confirm failed ({err:?}); will re-drain next run");
        }
    }

    eprintln!(
        "sovereign-backfill: drained {} line(s) -> {} sample(s), {} malformed, \
         {} duplicate(s) skipped, {} appended, {} append-failure(s)",
        lines.len(),
        drained_total,
        malformed,
        drained_total.saturating_sub(fresh.len()),
        appended,
        dropped_io,
    );
    ExitCode::SUCCESS
}
