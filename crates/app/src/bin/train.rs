//! `sovereign-train` — the bridge from the sovds dataset to a sovml model.
//!
//! Trains an interpretable decision tree on the collected corpus and evaluates it
//! against a held-out slice. The features are the router's raw signals; the label
//! is the diagnosed `class`. So this first model learns to **reproduce the rules
//! from raw telemetry** — the honest v1: its accuracy validates the whole
//! train→infer→evaluate pipeline, its errors reveal where raw signals do not
//! capture the adaptive-refined diagnosis, and the same pipeline is ready to train
//! on ground-truth (`inject=`) labels as they accrue. It prints the learned tree
//! (interpretable, kin to the hand-written rules).
//!
//! Usage: `sovereign-train [log] [--depth N]`.

use sovds::domain::Domain;
use sovds::record::Record;
use sovereign_adapters::dataset;
use sovereign_core::failure::FailureClass;
use sovereign_core::routerdomain::RouterDomain;
use sovml::tree::{train, TreeConfig};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(default_log_path);
    let depth = flag_value(&args, "--depth")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(8);

    let generations = std::env::var("SOVEREIGN_LOG_GENERATIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(10);
    let corpus = dataset::load(&log, generations);
    if corpus.records.len() < 50 {
        eprintln!(
            "sovereign-train: too few records to train ({}).",
            corpus.records.len()
        );
        return ExitCode::FAILURE;
    }

    // Bridge sovds -> sovml: each record becomes (raw signals, class index).
    let faults = RouterDomain::faults();
    let n_classes = faults.len();
    let rows: Vec<Vec<i64>> = corpus.records.iter().map(features).collect();
    let labels: Vec<u32> = corpus
        .records
        .iter()
        .map(|r| class_index(r.class, faults))
        .collect();

    // Temporal split: train on the older 70%, test on the newer 30% (no leakage of
    // the future into the past).
    let n = rows.len();
    let cut = n.saturating_mul(7).checked_div(10).unwrap_or(0);
    let (train_rows, test_rows) = rows.split_at(cut.min(n));
    let (train_labels, test_labels) = labels.split_at(cut.min(n));

    let cfg = TreeConfig {
        max_depth: depth,
        min_samples_leaf: 8,
    };
    let tree = train(train_rows, train_labels, n_classes, cfg);

    // Evaluate on the held-out slice: overall accuracy + per-class recall.
    let mut correct = 0usize;
    let mut per_class_total = vec![0usize; n_classes];
    let mut per_class_hit = vec![0usize; n_classes];
    for (row, &label) in test_rows.iter().zip(test_labels.iter()) {
        let pred = tree.predict(row);
        let li = label as usize;
        if let Some(t) = per_class_total.get_mut(li) {
            *t = t.saturating_add(1);
        }
        if pred == label {
            correct = correct.saturating_add(1);
            if let Some(h) = per_class_hit.get_mut(li) {
                *h = h.saturating_add(1);
            }
        }
    }
    let tested = test_rows.len();
    let acc = pct(correct, tested);

    println!(
        "Trained on {} samples, tested on {} (temporal 70/30 split).",
        train_rows.len(),
        tested
    );
    println!(
        "Tree: {} nodes, depth<= {depth}. Overall agreement with the rules on \
         held-out data: {acc}%.\n",
        tree.size()
    );
    println!("Per-class recall on held-out (where <100% the raw signals miss the");
    println!("adaptive-refined diagnosis — an honest map of what telemetry captures):");
    for (i, &f) in faults.iter().enumerate() {
        let total = per_class_total.get(i).copied().unwrap_or(0);
        if total == 0 {
            continue;
        }
        let hit = per_class_hit.get(i).copied().unwrap_or(0);
        println!(
            "  {:<14} {:>5}/{:<5} ({}%)",
            label_of(f),
            hit,
            total,
            pct(hit, total)
        );
    }

    let names = RouterDomain::signal_names();
    let class_names: Vec<&str> = faults.iter().map(|&f| label_of(f)).collect();
    println!("\nLearned tree (the rules, re-derived from data):\n");
    print!("{}", tree.render(names, &class_names));

    eprintln!(
        "\n({} records; {} skipped)",
        corpus.records.len(),
        corpus.skipped
    );
    ExitCode::SUCCESS
}

/// A record's raw signals as an integer feature row.
fn features(r: &Record<RouterDomain>) -> Vec<i64> {
    let mut row = Vec::new();
    RouterDomain::signals(&r.obs, &mut |v| row.push(v));
    row
}

fn class_index(c: FailureClass, faults: &[FailureClass]) -> u32 {
    faults
        .iter()
        .position(|&f| f == c)
        .and_then(|i| u32::try_from(i).ok())
        .unwrap_or(0)
}

fn label_of(f: FailureClass) -> &'static str {
    match f {
        FailureClass::None => "none",
        FailureClass::WanDown => "wan_down",
        FailureClass::SingboxDead => "singbox_dead",
        FailureClass::NodeDead => "node_dead",
        FailureClass::NodeDegraded => "node_degraded",
        FailureClass::DnsFail => "dns_fail",
    }
}

fn pct(hit: usize, total: usize) -> usize {
    hit.saturating_mul(100).checked_div(total).unwrap_or(0)
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i.saturating_add(1)))
        .map(String::as_str)
}

fn default_log_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut path = PathBuf::from(home);
    path.push(".local/share/sovereign/records.log");
    path
}
