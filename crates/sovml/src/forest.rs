// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A **random forest** — an ensemble of decision trees that vote — the sovereign
//! upgrade from a single tree.
//!
//! Each tree is grown on a *bootstrap* resample of the data (sampled with
//! replacement) and, at every split, considers only a random *subspace* of the
//! features ([`crate::tree::train_subspace`]). The two sources of randomness
//! decorrelate the trees, so their majority vote generalises better than any one
//! tree and is far steadier on noisy, imbalanced data — exactly the regime of rare
//! injected-fault classes. All randomness comes from a seeded [`Rng`], so a forest
//! is reproducible bit-for-bit (the internal charter the principle); depth, tree count and vote
//! arithmetic are all bounded (the principle).
//!
//! A forest keeps the tree's virtues: inference is `no_std`, and because it
//! serialises to the concatenation of its trees, it plugs straight into the same
//! cumulative save/compare loop as a lone tree.

use crate::rng::Rng;
use crate::tree::{train_subspace, DecisionTree, TreeConfig, MAX_CLASSES};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// Tuning for a forest. `mtry == 0` picks the classification default of
/// `floor(sqrt(n_features))` features per split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForestConfig {
    /// Number of trees in the ensemble (bounded — the principle).
    pub n_trees: usize,
    /// Per-tree growth limits (depth, min leaf).
    pub tree: TreeConfig,
    /// Features considered per split; `0` means `floor(sqrt(n_features))`.
    pub mtry: usize,
    /// Seed for the whole forest — fixes every bootstrap and subspace draw.
    pub seed: u64,
}

impl ForestConfig {
    /// Sensible defaults: 64 trees, tree defaults, auto `mtry`, seed 1.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            n_trees: 64,
            tree: TreeConfig::new(),
            mtry: 0,
            seed: 1,
        }
    }
}

impl Default for ForestConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A trained ensemble of decision trees that predicts by majority vote.
#[derive(Debug, Clone)]
pub struct RandomForest {
    trees: Vec<DecisionTree>,
    n_classes: usize,
}

impl RandomForest {
    /// Predict the class index for one feature row by majority vote of the trees
    /// (ties broken by lowest class index). Total and `no_std`.
    #[must_use]
    pub fn predict(&self, features: &[i64]) -> u32 {
        let mut votes = [0u64; MAX_CLASSES];
        for tree in &self.trees {
            let c = tree.predict(features) as usize;
            if let Some(slot) = votes.get_mut(c.min(MAX_CLASSES.saturating_sub(1))) {
                *slot = slot.saturating_add(1);
            }
        }
        let mut best = 0u32;
        let mut best_n = 0u64;
        for (c, &n) in votes.iter().enumerate() {
            if n > best_n {
                best_n = n;
                best = u32::try_from(c).unwrap_or(0);
            }
        }
        best
    }

    /// The number of trees in the ensemble.
    #[must_use]
    pub fn len(&self) -> usize {
        self.trees.len()
    }

    /// Whether the forest has no trees.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.trees.is_empty()
    }

    /// The class count this forest distinguishes.
    #[must_use]
    pub fn n_classes(&self) -> usize {
        self.n_classes
    }

    /// Total node count across all trees — the ensemble's complexity.
    #[must_use]
    pub fn total_nodes(&self) -> usize {
        self.trees
            .iter()
            .fold(0usize, |a, t| a.saturating_add(t.size()))
    }

    /// Serialize to a deterministic text form: a forest header followed by each
    /// tree's own serialization, concatenated. The inverse of [`Self::deserialize`].
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "sovml-forest 1 {} {}",
            self.n_classes,
            self.trees.len()
        );
        for tree in &self.trees {
            out.push_str(&tree.serialize());
        }
        out
    }

    /// Parse a forest back from [`Self::serialize`]. Any malformed or inconsistent
    /// input yields `None`, never a panic (the principle).
    #[must_use]
    pub fn deserialize(text: &str) -> Option<RandomForest> {
        let mut lines = text.lines();
        let mut header = lines.next()?.split_whitespace();
        if header.next()? != "sovml-forest" || header.next()? != "1" {
            return None;
        }
        let n_classes: usize = header.next()?.parse().ok()?;
        let count: usize = header.next()?.parse().ok()?;

        // Split the remaining lines into per-tree blocks, each starting at a
        // "sovml-tree" header line, and deserialize each block.
        let mut trees: Vec<DecisionTree> = Vec::new();
        let mut block = String::new();
        for line in lines {
            if line.starts_with("sovml-tree") && !block.is_empty() {
                trees.push(DecisionTree::deserialize(&block)?);
                block.clear();
            }
            block.push_str(line);
            block.push('\n');
        }
        if !block.trim().is_empty() {
            trees.push(DecisionTree::deserialize(&block)?);
        }
        if trees.len() != count {
            return None;
        }
        Some(RandomForest {
            trees,
            n_classes: n_classes.clamp(1, MAX_CLASSES),
        })
    }
}

/// Train a random forest on integer feature rows and their class labels.
///
/// `rows[i]` is the feature vector of sample `i`; `labels[i]` its class index in
/// `0..n_classes`. Deterministic for a given `cfg.seed`. `n_classes` is clamped to
/// [`MAX_CLASSES`].
#[must_use]
pub fn train_forest(
    rows: &[Vec<i64>],
    labels: &[u32],
    n_classes: usize,
    cfg: ForestConfig,
) -> RandomForest {
    let n_classes = n_classes.clamp(1, MAX_CLASSES);
    let n = rows.len().min(labels.len());
    let n_features = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mtry = if cfg.mtry == 0 {
        isqrt(n_features).max(1)
    } else {
        cfg.mtry
    };

    let mut master = Rng::new(cfg.seed);
    let mut trees: Vec<DecisionTree> = Vec::new();
    for _ in 0..cfg.n_trees {
        // Each tree gets its own stream, derived from the master so the whole
        // forest is fixed by one seed.
        let mut rng = Rng::new(master.next_u64());

        // Bootstrap: resample n rows with replacement.
        let mut boot_rows: Vec<Vec<i64>> = Vec::new();
        let mut boot_labels: Vec<u32> = Vec::new();
        for _ in 0..n {
            let pick = rng.below(n);
            if let (Some(r), Some(&l)) = (rows.get(pick), labels.get(pick)) {
                boot_rows.push(r.clone());
                boot_labels.push(l);
            }
        }
        let tree = train_subspace(
            &boot_rows,
            &boot_labels,
            n_classes,
            cfg.tree,
            mtry,
            &mut rng,
        );
        trees.push(tree);
    }

    RandomForest { trees, n_classes }
}

/// Integer square root (floor) — the classification default for `mtry`, with no
/// floats (the principle). Bounded and panic-free (the principle).
fn isqrt(n: usize) -> usize {
    if n < 2 {
        return n;
    }
    let mut x = n;
    // Newton's method on integers; converges in a handful of steps and is bounded
    // by the loop guard.
    let mut prev = 0usize;
    for _ in 0..64 {
        if x == prev {
            break;
        }
        prev = x;
        let div = n.checked_div(x).unwrap_or(0);
        x = x.saturating_add(div).checked_div(2).unwrap_or(0).max(1);
    }
    // x is now floor(sqrt(n)) or one above it; correct downward if needed.
    while x.saturating_mul(x) > n {
        x = x.saturating_sub(1);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::{isqrt, train_forest, ForestConfig, RandomForest};
    use crate::tree::TreeConfig;
    use alloc::vec;
    use alloc::vec::Vec;

    fn cfg(n_trees: usize) -> ForestConfig {
        ForestConfig {
            n_trees,
            tree: TreeConfig {
                max_depth: 8,
                min_samples_leaf: 4,
            },
            mtry: 0,
            seed: 1,
        }
    }

    #[test]
    fn isqrt_is_floor_sqrt() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(3), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(99), 9);
        assert_eq!(isqrt(100), 10);
    }

    fn two_feature_and() -> (Vec<Vec<i64>>, Vec<u32>) {
        let mut rows: Vec<Vec<i64>> = Vec::new();
        let mut labels: Vec<u32> = Vec::new();
        for a in [10i64, 90] {
            for b in [10i64, 90] {
                for _ in 0..25 {
                    rows.push(vec![a, b, 0, 1]);
                    labels.push(u32::from(a > 50 && b > 50));
                }
            }
        }
        (rows, labels)
    }

    #[test]
    fn forest_learns_a_two_feature_rule() {
        let (rows, labels) = two_feature_and();
        let forest = train_forest(&rows, &labels, 2, cfg(32));
        assert_eq!(forest.predict(&[90, 90, 0, 1]), 1);
        assert_eq!(forest.predict(&[90, 10, 0, 1]), 0);
        assert_eq!(forest.predict(&[10, 90, 0, 1]), 0);
        assert_eq!(forest.predict(&[10, 10, 0, 1]), 0);
        assert_eq!(forest.len(), 32);
        assert!(!forest.is_empty());
    }

    #[test]
    fn same_seed_is_reproducible() {
        let (rows, labels) = two_feature_and();
        let a = train_forest(&rows, &labels, 2, cfg(16));
        let b = train_forest(&rows, &labels, 2, cfg(16));
        assert_eq!(
            a.serialize(),
            b.serialize(),
            "a seed fixes the whole forest"
        );
    }

    #[test]
    fn serialize_round_trips_and_predicts_identically() {
        let (rows, labels) = two_feature_and();
        let forest = train_forest(&rows, &labels, 2, cfg(16));
        let text = forest.serialize();
        let back = RandomForest::deserialize(&text);
        assert!(back.is_some(), "a serialized forest must round-trip");
        if let Some(back) = back {
            assert_eq!(back.len(), forest.len());
            for row in [[90, 90, 0, 1], [10, 10, 0, 1], [90, 10, 0, 1]] {
                assert_eq!(forest.predict(&row), back.predict(&row));
            }
        }
        assert!(RandomForest::deserialize("garbage").is_none());
        assert!(RandomForest::deserialize("sovml-forest 1 2 5\nL 0 1").is_none());
    }
}
