// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A unified **trained model** — a decision tree or a random forest behind one
//! type. This is the seam the cumulative learning loop turns on: it can save,
//! reload and compare "the best model so far" without caring which kind it is, and
//! a saved file upgrades from a tree to a forest (or back) transparently.
//!
//! [`Model::deserialize`] sniffs the header line, so the on-disk format is
//! self-describing and a third-party loop never has to know in advance what it
//! trained. Inference is `no_std`; a malformed file yields `None`, never a panic
//!.

use crate::forest::RandomForest;
use crate::tree::DecisionTree;
use alloc::string::String;

/// A trained classifier: either a single [`DecisionTree`] or a [`RandomForest`].
#[derive(Debug, Clone)]
pub enum Model {
    /// One interpretable decision tree.
    Tree(DecisionTree),
    /// A voting ensemble of trees.
    Forest(RandomForest),
}

impl Model {
    /// Predict the class index for one integer feature row. Total and `no_std`.
    #[must_use]
    pub fn predict(&self, features: &[i64]) -> u32 {
        match self {
            Model::Tree(t) => t.predict(features),
            Model::Forest(f) => f.predict(features),
        }
    }

    /// The class count this model distinguishes.
    #[must_use]
    pub fn n_classes(&self) -> usize {
        match self {
            Model::Tree(t) => t.n_classes(),
            Model::Forest(f) => f.n_classes(),
        }
    }

    /// Total node count — a size/complexity measure comparable across kinds.
    #[must_use]
    pub fn total_nodes(&self) -> usize {
        match self {
            Model::Tree(t) => t.size(),
            Model::Forest(f) => f.total_nodes(),
        }
    }

    /// A short human label for the kind of model (e.g. for logs).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Model::Tree(_) => "tree",
            Model::Forest(_) => "forest",
        }
    }

    /// Serialize to the underlying kind's self-describing text form.
    #[must_use]
    pub fn serialize(&self) -> String {
        match self {
            Model::Tree(t) => t.serialize(),
            Model::Forest(f) => f.serialize(),
        }
    }

    /// Parse a model back, sniffing the header to choose the kind. Any malformed
    /// input yields `None`.
    #[must_use]
    pub fn deserialize(text: &str) -> Option<Model> {
        let first = text.lines().next()?;
        if first.starts_with("sovml-forest") {
            RandomForest::deserialize(text).map(Model::Forest)
        } else if first.starts_with("sovml-tree") {
            DecisionTree::deserialize(text).map(Model::Tree)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Model;
    use crate::forest::{train_forest, ForestConfig};
    use crate::tree::{train, TreeConfig};
    use alloc::vec;
    use alloc::vec::Vec;

    fn data() -> (Vec<Vec<i64>>, Vec<u32>) {
        let mut rows: Vec<Vec<i64>> = Vec::new();
        let mut labels: Vec<u32> = Vec::new();
        for v in 0..40 {
            rows.push(vec![v, 7]);
            labels.push(0);
        }
        for v in 60..100 {
            rows.push(vec![v, 7]);
            labels.push(1);
        }
        (rows, labels)
    }

    #[test]
    fn tree_and_forest_round_trip_through_model() {
        let (rows, labels) = data();
        let tree = Model::Tree(train(&rows, &labels, 2, TreeConfig::new()));
        let forest = Model::Forest(train_forest(&rows, &labels, 2, ForestConfig::new()));

        for m in [tree, forest] {
            let kind = m.kind();
            let back = Model::deserialize(&m.serialize());
            assert!(back.is_some(), "{kind} must round-trip through Model");
            if let Some(back) = back {
                assert_eq!(back.kind(), kind, "kind is preserved by the sniff");
                for v in [5i64, 35, 65, 95] {
                    assert_eq!(m.predict(&[v, 7]), back.predict(&[v, 7]));
                }
            }
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Model::deserialize("garbage").is_none());
        assert!(Model::deserialize("").is_none());
    }
}
