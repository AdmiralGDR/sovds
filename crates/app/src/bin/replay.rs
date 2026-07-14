//! `sovereign-replay` — replay the collected telemetry through the decision
//! pipeline under different adaptive configs and count what each *would* have
//! done. This turns tuning from a guess into a measurement (the principle): run it,
//! read how many disruptive actions each config produces on the same real data.
//!
//! It reuses the exact live decision logic ([`decide_cycle`]), so the replay is
//! faithful. The fleet and FSM state are reset at each run boundary (`tick == 0`)
//! to mirror how the collector actually restarted.

use sovereign_adapters::dataset;
use sovereign_core::fleet::{Fleet, FleetConfig, NodeVerdict};
use sovereign_core::fsm::{Action, Config, State};
use sovereign_core::pipeline::decide_cycle;
use sovereign_core::record::Record;
use std::path::PathBuf;
use std::process::ExitCode;

/// Counts produced by replaying a corpus under one config.
struct Tally {
    total: usize,
    actions: [(Action, usize); 6],
    degraded_cycles: usize,
}

impl Tally {
    fn new() -> Self {
        Self {
            total: 0,
            actions: [
                (Action::Observe, 0),
                (Action::SwitchNode, 0),
                (Action::RestartSingbox, 0),
                (Action::BounceWan, 0),
                (Action::FailoverBackup, 0),
                (Action::Reboot, 0),
            ],
            degraded_cycles: 0,
        }
    }

    fn record_action(&mut self, action: Action) {
        for entry in &mut self.actions {
            if entry.0 == action {
                entry.1 = entry.1.saturating_add(1);
            }
        }
    }

    fn count(&self, action: Action) -> usize {
        self.actions
            .iter()
            .find(|(a, _)| *a == action)
            .map_or(0, |(_, n)| *n)
    }

    /// Non-`Observe` actions — the disruptive ones we want to minimise.
    fn disruptive(&self) -> usize {
        self.actions
            .iter()
            .filter(|(a, _)| !matches!(a, Action::Observe))
            .fold(0usize, |acc, (_, n)| acc.saturating_add(*n))
    }
}

fn replay(records: &[Record], core: Config, fleet_cfg: FleetConfig) -> Tally {
    let mut tally = Tally::new();
    let mut fleet = Fleet::new(fleet_cfg);
    let mut state = State::Healthy;

    for record in records {
        // A tick of 0 marks a fresh collector run: reset to match reality.
        if record.tick.0 == 0 {
            fleet = Fleet::new(fleet_cfg);
            state = State::Healthy;
        }
        let cycle = decide_cycle(state, &mut fleet, &record.telemetry, &core, record.tick);
        state = cycle.next_state;
        tally.total = tally.total.saturating_add(1);
        tally.record_action(cycle.action);
        if matches!(cycle.verdict, NodeVerdict::Slow | NodeVerdict::Throttled) {
            tally.degraded_cycles = tally.degraded_cycles.saturating_add(1);
        }
    }
    tally
}

fn print_tally(name: &str, t: &Tally) {
    println!("--- {name} ---");
    println!("  cycles:            {}", t.total);
    println!("  degraded verdicts: {}", t.degraded_cycles);
    println!("  switch_node:       {}", t.count(Action::SwitchNode));
    println!("  restart_singbox:   {}", t.count(Action::RestartSingbox));
    println!("  bounce_wan:        {}", t.count(Action::BounceWan));
    println!("  failover_backup:   {}", t.count(Action::FailoverBackup));
    println!("  reboot:            {}", t.count(Action::Reboot));
    println!("  DISRUPTIVE total:  {}", t.disruptive());
}

fn main() -> ExitCode {
    let path = log_path();
    let corpus = dataset::load(&path, 10);
    if corpus.records.is_empty() {
        eprintln!("sovereign-replay: no records at {}", path.display());
        return ExitCode::FAILURE;
    }

    let core = |confirm_threshold: u8| Config {
        degraded_above: sovereign_core::domain::LatencyMs(800),
        confirm_threshold,
        cooldown_ticks: 3,
    };

    let legacy = replay(&corpus.records, core(3), FleetConfig::legacy());
    let tuned3 = replay(&corpus.records, core(3), FleetConfig::tuned());
    let tuned5 = replay(&corpus.records, core(5), FleetConfig::tuned());

    println!("== replay over {} real records ==", corpus.records.len());
    print_tally("legacy fleet, confirm=3", &legacy);
    print_tally("tuned fleet, confirm=3", &tuned3);
    print_tally("tuned fleet, confirm=5", &tuned5);
    println!(
        "\ndisruptive actions: legacy {} -> tuned/confirm3 {} -> tuned/confirm5 {}",
        legacy.disruptive(),
        tuned3.disruptive(),
        tuned5.disruptive()
    );
    ExitCode::SUCCESS
}

fn log_path() -> PathBuf {
    if let Some(arg) = std::env::args().nth(1) {
        return PathBuf::from(arg);
    }
    if let Ok(env) = std::env::var("SOVEREIGN_LOG") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}
