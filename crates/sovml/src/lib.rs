// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! # sovml — Sovereign Machine Learning
//!
//! From-scratch, **zero-dependency**, integer machine learning. No ML framework
//! underneath — no `ndarray`, no `linfa`, no `torch`; the math is ours, integer,
//! and reproducible (the principle & I & V).
//!
//! The first model is a **decision tree** ([`tree`]): the sovereign choice because
//! it is *interpretable* (it prints as a set of rules — kin to a hand-written rule
//! engine), *integer* (splits are thresholds on integer features, so training and
//! inference are bit-identical on every machine — no floats, no NaN), and *bounded*
//! (depth and node count are capped — the principle). It learns rules from data and can
//! be compared head-to-head against hand-written ones (the principle: metrics before
//! theories).
//!
//! The second model is a **random forest** ([`forest`]): an ensemble of trees, each
//! grown on a bootstrap resample and a random feature subspace, that predicts by
//! majority vote. It keeps the tree's integer, bounded, reproducible nature (all
//! randomness comes from a seeded [`rng`], the principle) while generalising far
//! better on noisy, imbalanced data. A [`model::Model`] wraps either kind behind one
//! type so a training loop can save, reload and compare "the best model so far"
//! without knowing which it is — the seam that makes the advantage *cumulative*.
//!
//! Training links `std` (it sorts and grows a `Vec`); **inference is `no_std`** — a
//! trained tree is a flat arena of nodes you can walk on any target, including the
//! device the data came from.
//!
//! ## Relationship to `sovds`
//! `sovml` knows nothing about `sovds` or any dataset format — it takes plain
//! integer feature rows and class indices, so it is reusable anywhere. The bridge
//! that turns a `sovds` corpus into training rows (via a domain's signals) lives at
//! the edge, in the application, keeping both crates independent.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

// Training and a trained tree need a growable arena, so the crate needs an
// allocator — but never `std`. `no_std + alloc` runs on any target with a heap.
extern crate alloc;

pub mod forest;
pub mod metrics;
pub mod model;
pub mod rng;
pub mod tree;

pub use forest::{train_forest, ForestConfig, RandomForest};
pub use metrics::Metrics;
pub use model::Model;
pub use rng::Rng;
pub use tree::{train, train_subspace, DecisionTree, TreeConfig};
