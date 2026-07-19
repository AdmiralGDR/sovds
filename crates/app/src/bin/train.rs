//! `sovereign-train` — the bridge from the sovds dataset to a sovml model, with a
//! **cumulative** training loop.
//!
//! Features are the router's raw signals; the label is the diagnosed `class`. This
//! first model learns to reproduce the rules from raw telemetry (the honest v1: it
//! validates the whole train→infer→evaluate loop, and its errors reveal where raw
//! signals miss the adaptive-refined diagnosis). The same pipeline is ready to
//! train on ground-truth (`inject=`) labels as they accrue.
//!
//! With `--save <model>`, the advantage **accumulates**: each run trains a
//! candidate, evaluates it and the currently-saved model on the same held-out
//! split, and keeps whichever scores higher (macro-F1 — the imbalance-honest
//! metric, the principle). Run it daily (cron) and the saved model only ever improves as
//! the dataset grows; `--history <log>` records the trajectory so "better every
//! day" is a fact you can read.
//!
//! `--forest [N]` trains a random forest (N trees, default 64) instead of a single
//! tree; both kinds compete in the same cumulative loop on held-out macro-F1, so an
//! ensemble only replaces the saved tree if it genuinely scores higher.
//!
//! Features are temporal by default (each raw signal expands to raw value +
//! deviation-from-EWMA-baseline + rate-of-change), so the model sees the same
//! history the adaptive layer does. `--base` trains on raw per-record signals only.
//!
//! Usage:
//! `sovereign-train [log] [--depth N] [--forest [N]] [--base] [--save model] [--history hist.log]`.

use sovds::domain::Domain;
use sovds::record::Record;
use sovds::window::{self, WindowConfig, WindowFeatures};
use sovereign_adapters::dataset;
use sovereign_core::failure::FailureClass;
use sovereign_core::routerdomain::RouterDomain;
use sovml::forest::{train_forest, ForestConfig};
use sovml::metrics::Metrics;
use sovml::model::Model;
use sovml::tree::{train, TreeConfig};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let log = positional_log(&args).unwrap_or_else(default_log_path);
    let depth = flag_value(&args, "--depth")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(8);
    let save = flag_value(&args, "--save").map(PathBuf::from);
    let history = flag_value(&args, "--history").map(PathBuf::from);
    // --forest [N] trains a random forest of N trees (default 64) instead of a
    // single tree; the two compete on the same held-out macro-F1.
    let forest_trees = if args.iter().any(|a| a == "--forest") {
        Some(
            flag_value(&args, "--forest")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(64),
        )
    } else {
        None
    };
    // Temporal features (EWMA baseline + deviation + rate per channel) are ON by
    // default — they give the model the history the adaptive layer sees. --base
    // trains on raw per-record signals only, for a head-to-head comparison.
    let base_features = args.iter().any(|a| a == "--base");

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

    let faults = RouterDomain::faults();
    let n_classes = faults.len();
    let rows: Vec<Vec<i64>> = if base_features {
        corpus.records.iter().map(features).collect()
    } else {
        // Stateful, causal transform over the chronological stream; reset at each
        // run boundary (tick 0) so one run's baseline never leaks into the next.
        let mut wf = WindowFeatures::new(WindowConfig::new());
        corpus
            .records
            .iter()
            .map(|r| {
                if r.tick.0 == 0 {
                    wf.reset();
                }
                wf.transform(&features(r))
            })
            .collect()
    };
    let feat_names: Vec<String> = if base_features {
        RouterDomain::signal_names()
            .iter()
            .map(|s| String::from(*s))
            .collect()
    } else {
        window::feature_names(RouterDomain::signal_names())
    };
    let labels: Vec<u32> = corpus
        .records
        .iter()
        .map(|r| class_index(r.class, faults))
        .collect();

    // Temporal split: train on the older 70%, test on the newer 30%.
    let n = rows.len();
    let cut = n.saturating_mul(7).checked_div(10).unwrap_or(0).min(n);
    let (train_rows, test_rows) = rows.split_at(cut);
    let (train_labels, test_labels) = labels.split_at(cut);

    let cfg = TreeConfig {
        max_depth: depth,
        min_samples_leaf: 8,
    };
    let candidate = match forest_trees {
        Some(n_trees) => Model::Forest(train_forest(
            train_rows,
            train_labels,
            n_classes,
            ForestConfig {
                n_trees,
                tree: cfg,
                mtry: 0,
                seed: 1,
            },
        )),
        None => Model::Tree(train(train_rows, train_labels, n_classes, cfg)),
    };
    let cand_metrics = evaluate(&candidate, test_rows, test_labels, n_classes);
    let cand_f1 = cand_metrics.macro_f1_pm();

    println!(
        "Trained on {} samples, tested on {} (temporal 70/30 split); {} features ({}).",
        train_rows.len(),
        test_rows.len(),
        feat_names.len(),
        if base_features {
            "raw signals"
        } else {
            "raw + EWMA-deviation + rate"
        },
    );
    println!(
        "Candidate: {} ({} nodes); accuracy {}‰, macro-F1 {}‰ over {} classes present.",
        candidate.kind(),
        candidate.total_nodes(),
        cand_metrics.accuracy_pm(),
        cand_f1,
        cand_metrics.classes_present(),
    );
    print_per_class(&cand_metrics, faults);

    // Cumulative loop: keep the better of {saved model, candidate}. Two models are
    // only comparable if they share a feature schema — an incumbent trained on a
    // different feature recipe would score meaningless garbage on this run's test
    // set (its feature indices point elsewhere). We stamp the schema in a sidecar
    // and only compare within a matching schema; a changed recipe re-baselines the
    // lineage honestly (the principle: no invalid comparison masquerading as progress).
    if let Some(save_path) = &save {
        let tag = schema_tag(base_features, feat_names.len());
        let meta_path = schema_sidecar(save_path);
        let prior_tag = std::fs::read_to_string(&meta_path)
            .ok()
            .map(|s| s.trim().to_string());
        let model_existed = std::fs::metadata(save_path).is_ok();
        let schema_matches = prior_tag.as_deref() == Some(tag.as_str());

        // Only load the incumbent when it is comparable on this run's features.
        let incumbent = if schema_matches {
            std::fs::read_to_string(save_path)
                .ok()
                .and_then(|t| Model::deserialize(&t))
        } else {
            None
        };
        let inc_f1 = incumbent
            .as_ref()
            .map(|m| evaluate(m, test_rows, test_labels, n_classes).macro_f1_pm());

        let keep_candidate = inc_f1.is_none_or(|prev| cand_f1 >= prev);
        let best_f1 = if keep_candidate {
            if let Err(e) = std::fs::write(save_path, candidate.serialize().as_bytes()) {
                eprintln!("sovereign-train: could not save model: {e}");
            }
            if let Err(e) = std::fs::write(&meta_path, tag.as_bytes()) {
                eprintln!("sovereign-train: could not save model schema: {e}");
            }
            cand_f1
        } else {
            inc_f1.unwrap_or(0)
        };

        match inc_f1 {
            None if model_existed && !schema_matches => println!(
                "\nFeature schema changed ({} → {tag}) → re-baselining the lineage with the \
                 candidate ({cand_f1}‰); the old model is not comparable on the new features.",
                prior_tag.as_deref().unwrap_or("unknown"),
            ),
            None => println!(
                "\nNo saved model yet → saved the candidate as the first best ({cand_f1}‰)."
            ),
            Some(prev) if keep_candidate => println!(
                "\nCandidate macro-F1 {cand_f1}‰ >= saved {prev}‰ → replaced the saved model."
            ),
            Some(prev) => println!(
                "\nCandidate macro-F1 {cand_f1}‰ < saved {prev}‰ → kept the saved model (no regression)."
            ),
        }

        // Record the trajectory so "better every day" is a fact you can read.
        if let Some(hist) = &history {
            let last_ts = corpus
                .records
                .iter()
                .map(|r| r.wall_time)
                .max()
                .unwrap_or(0);
            let line = format!(
                "n={} ts={} cand_f1={} best_f1={} acc={}\n",
                corpus.records.len(),
                last_ts,
                cand_f1,
                best_f1,
                cand_metrics.accuracy_pm(),
            );
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(hist)
            {
                let _ = f.write_all(line.as_bytes());
            }
            print_trajectory(hist);
        }
    } else {
        match &candidate {
            Model::Tree(tree) => {
                let names: Vec<&str> = feat_names.iter().map(String::as_str).collect();
                let class_names: Vec<&str> = faults.iter().map(|&f| label_of(f)).collect();
                println!("\nLearned tree (the rules, re-derived from data):\n");
                print!("{}", tree.render(&names, &class_names));
            }
            Model::Forest(forest) => {
                println!(
                    "\nRandom forest of {} trees ({} nodes total) — vote-based, not a single \
                     printable rule set. Use --save to keep it and track its macro-F1.",
                    forest.len(),
                    forest.total_nodes(),
                );
            }
        }
    }

    eprintln!(
        "\n({} records; {} skipped)",
        corpus.records.len(),
        corpus.skipped
    );
    ExitCode::SUCCESS
}

/// Evaluate a model on a held-out slice into a confusion-matrix [`Metrics`].
fn evaluate(model: &Model, rows: &[Vec<i64>], labels: &[u32], n_classes: usize) -> Metrics {
    let mut m = Metrics::new(n_classes);
    for (row, &label) in rows.iter().zip(labels.iter()) {
        m.record(label, model.predict(row));
    }
    m
}

fn print_per_class(m: &Metrics, faults: &[FailureClass]) {
    println!("Per class (held-out): recall / F1 (‰) — where <1000 raw signals miss the diagnosis:");
    for (i, &f) in faults.iter().enumerate() {
        let r = m.recall_pm(i);
        let f1 = m.f1_pm(i);
        // Only show classes that occurred in the test slice.
        if r > 0 || f1 > 0 || m.precision_pm(i) > 0 {
            println!("  {:<14} recall {:>4}  F1 {:>4}", label_of(f), r, f1);
        }
    }
}

/// Print the best-macro-F1 trajectory from the history log — the cumulative record.
fn print_trajectory(hist: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(hist) else {
        return;
    };
    println!("\nCumulative trajectory (best macro-F1 as the dataset grows):");
    for line in text.lines() {
        let n = field(line, "n=");
        let best = field(line, "best_f1=");
        if let (Some(n), Some(best)) = (n, best) {
            println!("  n={n:<6} best-F1={best}‰");
        }
    }
}

fn field(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(key))
        .map(str::to_string)
}

/// A short identifier for the feature representation a model was trained on. Two
/// models are only comparable if these match (same recipe, same column count).
fn schema_tag(base_features: bool, n_features: usize) -> String {
    let mode = if base_features { "base" } else { "temporal" };
    format!("{mode}:{n_features}")
}

/// The sidecar path holding a saved model's [`schema_tag`] (e.g. `best.model.meta`).
fn schema_sidecar(save_path: &std::path::Path) -> PathBuf {
    let mut s = save_path.to_path_buf().into_os_string();
    s.push(".meta");
    PathBuf::from(s)
}

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

/// The first positional argument (the log path), skipping value-taking flags and
/// their arguments so `--save X` does not get mistaken for the log.
fn positional_log(args: &[String]) -> Option<PathBuf> {
    let value_flags = ["--depth", "--save", "--history", "--forest"];
    let mut prev_is_value_flag = false;
    for a in args {
        if prev_is_value_flag {
            prev_is_value_flag = false;
            // The token after a value-flag is its value only if it is not itself a
            // flag (so `--forest --save X` treats `--save` as a flag, not a count).
            if !a.starts_with("--") {
                continue;
            }
        }
        if a.starts_with("--") {
            if value_flags.contains(&a.as_str()) {
                prev_is_value_flag = true;
            }
            continue;
        }
        return Some(PathBuf::from(a));
    }
    None
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
