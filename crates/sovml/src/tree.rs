//! A CART decision tree over integer features — the sovereign first model.
//!
//! Splits are `feature <= threshold` on integer values, chosen greedily to
//! maximise class purity (Gini) at each node. All the split arithmetic is integer
//! and fixed-point, so training is deterministic and bit-identical on any machine
//! (the principle) — no floats, no NaN. Depth and leaf size are capped, so a tree is
//! bounded (the principle). A trained tree is a flat arena of nodes; walking it to
//! [predict](DecisionTree::predict) needs nothing but the core (`no_std`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// Upper bound on the number of classes (bounded per-node state, the principle).
pub const MAX_CLASSES: usize = 16;

/// Fixed-point scale for the integer split score (keeps sub-unit precision without
/// floats). Internal to split selection; never surfaced.
const SCORE_SCALE: u64 = 1 << 20;

/// One node of the tree, stored in a flat arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    /// A terminal decision: predict `class`. `n` is how many training samples
    /// landed here (a crude confidence).
    Leaf { class: u32, n: usize },
    /// Send a sample left if `feature <= threshold`, else right.
    Split {
        feature: usize,
        threshold: i64,
        left: usize,
        right: usize,
    },
}

/// Tuning for training — the guards that keep the tree bounded and honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeConfig {
    /// Maximum tree depth (also bounds recursion — the principle).
    pub max_depth: u32,
    /// A node with fewer than twice this many samples will not be split, and no
    /// split leaving a child smaller than this is considered (avoids overfitting
    /// to a handful of rows).
    pub min_samples_leaf: usize,
}

impl TreeConfig {
    /// Sensible defaults: depth 12, at least 8 samples per leaf.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_depth: 12,
            min_samples_leaf: 8,
        }
    }
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// A trained decision tree. The arena's last node is the root.
#[derive(Debug, Clone)]
pub struct DecisionTree {
    nodes: Vec<Node>,
    root: usize,
    n_classes: usize,
}

impl DecisionTree {
    /// Predict the class index for one integer feature row. Total and `no_std`:
    /// a missing feature reads as 0, an out-of-range node yields class 0 — it can
    /// never panic or loop forever (the arena is acyclic and bounded).
    #[must_use]
    pub fn predict(&self, features: &[i64]) -> u32 {
        let mut cursor = self.root;
        // The arena has N nodes; a walk visits strictly-decreasing indices
        // (children were pushed before parents), so N steps is a hard ceiling.
        for _ in 0..self.nodes.len() {
            match self.nodes.get(cursor) {
                Some(Node::Leaf { class, .. }) => return *class,
                Some(Node::Split {
                    feature,
                    threshold,
                    left,
                    right,
                }) => {
                    let value = features.get(*feature).copied().unwrap_or(0);
                    cursor = if value <= *threshold { *left } else { *right };
                }
                None => return 0,
            }
        }
        0
    }

    /// Number of nodes in the tree (leaves + splits) — a size/complexity measure.
    #[must_use]
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// The class count this tree distinguishes.
    #[must_use]
    pub fn n_classes(&self) -> usize {
        self.n_classes
    }

    /// Serialize the tree to a self-describing, deterministic text form — one line
    /// per node — so a model can be **saved and improved over time** (the basis of
    /// cumulative learning). The inverse of [`DecisionTree::deserialize`].
    #[must_use]
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "sovml-tree 1 {} {} {}",
            self.n_classes,
            self.root,
            self.nodes.len()
        );
        for node in &self.nodes {
            match *node {
                Node::Leaf { class, n } => {
                    let _ = writeln!(out, "L {class} {n}");
                }
                Node::Split {
                    feature,
                    threshold,
                    left,
                    right,
                } => {
                    let _ = writeln!(out, "S {feature} {threshold} {left} {right}");
                }
            }
        }
        out
    }

    /// Parse a tree back from [`DecisionTree::serialize`]. A boundary guard: any
    /// malformed or inconsistent input yields `None`, never a panic (the principle).
    #[must_use]
    pub fn deserialize(text: &str) -> Option<DecisionTree> {
        let mut lines = text.lines();
        let mut header = lines.next()?.split_whitespace();
        if header.next()? != "sovml-tree" || header.next()? != "1" {
            return None;
        }
        let n_classes: usize = header.next()?.parse().ok()?;
        let root: usize = header.next()?.parse().ok()?;
        let count: usize = header.next()?.parse().ok()?;

        let mut nodes: Vec<Node> = Vec::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let mut tok = line.split_whitespace();
            let node = match tok.next()? {
                "L" => Node::Leaf {
                    class: tok.next()?.parse().ok()?,
                    n: tok.next()?.parse().ok()?,
                },
                "S" => Node::Split {
                    feature: tok.next()?.parse().ok()?,
                    threshold: tok.next()?.parse().ok()?,
                    left: tok.next()?.parse().ok()?,
                    right: tok.next()?.parse().ok()?,
                },
                _ => return None,
            };
            nodes.push(node);
        }
        // Structural validation: declared size and a reachable, in-range root.
        if nodes.len() != count || root >= nodes.len() {
            return None;
        }
        for node in &nodes {
            if let Node::Split { left, right, .. } = *node {
                if left >= nodes.len() || right >= nodes.len() {
                    return None;
                }
            }
        }
        Some(DecisionTree {
            nodes,
            root,
            n_classes: n_classes.clamp(1, MAX_CLASSES),
        })
    }

    /// Render the tree as human-readable nested rules — the interpretability that
    /// makes a decision tree kin to a hand-written rule engine. `feature_names` and
    /// `class_names` label the splits and leaves; missing names fall back to
    /// indices.
    #[must_use]
    pub fn render(&self, feature_names: &[&str], class_names: &[&str]) -> String {
        let mut out = String::new();
        self.render_node(self.root, 0, feature_names, class_names, &mut out);
        out
    }

    fn render_node(
        &self,
        node: usize,
        depth: usize,
        features: &[&str],
        classes: &[&str],
        out: &mut String,
    ) {
        let pad = "  ".repeat(depth);
        match self.nodes.get(node) {
            Some(&Node::Leaf { class, n }) => {
                let name = classes.get(class as usize).copied().unwrap_or("<class>");
                let _ = writeln!(out, "{pad}=> {name} (n={n})");
            }
            Some(&Node::Split {
                feature,
                threshold,
                left,
                right,
            }) => {
                let name = features.get(feature).copied().unwrap_or("<feat>");
                let _ = writeln!(out, "{pad}if {name} <= {threshold}:");
                self.render_node(left, depth.saturating_add(1), features, classes, out);
                let _ = writeln!(out, "{pad}else:");
                self.render_node(right, depth.saturating_add(1), features, classes, out);
            }
            None => {}
        }
    }
}

/// Train a decision tree on integer feature rows and their class labels.
///
/// `rows[i]` is the feature vector of sample `i`; `labels[i]` its class index in
/// `0..n_classes`. Deterministic. `n_classes` is clamped to [`MAX_CLASSES`].
#[must_use]
pub fn train(rows: &[Vec<i64>], labels: &[u32], n_classes: usize, cfg: TreeConfig) -> DecisionTree {
    let n_classes = n_classes.clamp(1, MAX_CLASSES);
    let n_features = rows.iter().map(Vec::len).max().unwrap_or(0);
    let indices: Vec<usize> = (0..rows.len().min(labels.len())).collect();

    let mut nodes: Vec<Node> = Vec::new();
    let root = build(
        &indices, 0, rows, labels, n_features, n_classes, cfg, &mut nodes,
    );
    DecisionTree {
        nodes,
        root,
        n_classes,
    }
}

/// Recursively build a subtree over `indices`; push its nodes and return the index
/// of this subtree's root. Children are pushed before their parent.
#[allow(clippy::too_many_arguments)]
fn build(
    indices: &[usize],
    depth: u32,
    rows: &[Vec<i64>],
    labels: &[u32],
    n_features: usize,
    n_classes: usize,
    cfg: TreeConfig,
    nodes: &mut Vec<Node>,
) -> usize {
    let (counts, total) = class_counts(indices, labels, n_classes);
    let leaf = |nodes: &mut Vec<Node>| {
        nodes.push(Node::Leaf {
            class: majority(&counts),
            n: total,
        });
        nodes.len().saturating_sub(1)
    };

    // Stop: too deep, too small, or already pure.
    let pure = counts.iter().filter(|&&c| c > 0).count() <= 1;
    if depth >= cfg.max_depth || total < cfg.min_samples_leaf.saturating_mul(2) || pure {
        return leaf(nodes);
    }

    let Some((feature, threshold)) = best_split(
        indices,
        rows,
        labels,
        n_features,
        n_classes,
        cfg.min_samples_leaf,
    ) else {
        return leaf(nodes);
    };

    // Partition on the chosen split.
    let mut left: Vec<usize> = Vec::new();
    let mut right: Vec<usize> = Vec::new();
    for &i in indices {
        let value = rows
            .get(i)
            .and_then(|r| r.get(feature))
            .copied()
            .unwrap_or(0);
        if value <= threshold {
            left.push(i);
        } else {
            right.push(i);
        }
    }
    // A degenerate split (all one side) means no real separation — make a leaf.
    if left.is_empty() || right.is_empty() {
        return leaf(nodes);
    }

    let li = build(
        &left,
        depth.saturating_add(1),
        rows,
        labels,
        n_features,
        n_classes,
        cfg,
        nodes,
    );
    let ri = build(
        &right,
        depth.saturating_add(1),
        rows,
        labels,
        n_features,
        n_classes,
        cfg,
        nodes,
    );
    nodes.push(Node::Split {
        feature,
        threshold,
        left: li,
        right: ri,
    });
    nodes.len().saturating_sub(1)
}

/// Class histogram over `indices` plus the total count.
fn class_counts(
    indices: &[usize],
    labels: &[u32],
    n_classes: usize,
) -> ([u64; MAX_CLASSES], usize) {
    let mut counts = [0u64; MAX_CLASSES];
    let mut total = 0usize;
    for &i in indices {
        if let Some(&label) = labels.get(i) {
            let c = (label as usize).min(n_classes.saturating_sub(1));
            if let Some(slot) = counts.get_mut(c) {
                *slot = slot.saturating_add(1);
                total = total.saturating_add(1);
            }
        }
    }
    (counts, total)
}

/// The most common class in a histogram (ties broken by lowest index).
fn majority(counts: &[u64; MAX_CLASSES]) -> u32 {
    let mut best = 0u32;
    let mut best_n = 0u64;
    for (c, &n) in counts.iter().enumerate() {
        if n > best_n {
            best_n = n;
            best = u32::try_from(c).unwrap_or(0);
        }
    }
    best
}

/// Find the `(feature, threshold)` split maximising the integer purity score, or
/// `None` if no split beats keeping the node whole. Uses an incremental left/right
/// sweep over each feature's sorted values (O(n log n) per feature).
fn best_split(
    indices: &[usize],
    rows: &[Vec<i64>],
    labels: &[u32],
    n_features: usize,
    n_classes: usize,
    min_leaf: usize,
) -> Option<(usize, i64)> {
    let (counts, total) = class_counts(indices, labels, n_classes);
    // The "no split" baseline: all samples in one node.
    let base = sum_sq(&counts)
        .saturating_mul(SCORE_SCALE)
        .checked_div(total as u64)
        .unwrap_or(0);

    let mut best: Option<(usize, i64, u64)> = None;

    for feature in 0..n_features {
        // Sort the sample indices by this feature's value.
        let mut order: Vec<usize> = indices.to_vec();
        order.sort_by_key(|&i| feature_value(rows, i, feature));

        // Sweep: move samples one at a time from the right child to the left,
        // updating the sum-of-squared class counts incrementally.
        let mut left_counts = [0u64; MAX_CLASSES];
        let mut right_counts = counts;
        let mut sl = 0u64; // sum of squared left counts
        let mut sr = sum_sq(&counts); // sum of squared right counts
        let mut n_left = 0usize;

        for pos in 0..order.len() {
            let Some(&i) = order.get(pos) else { break };
            let label = labels.get(i).copied().unwrap_or(0) as usize;
            let c = label.min(n_classes.saturating_sub(1));

            // Move sample i into the left child.
            if let Some(lc) = left_counts.get_mut(c) {
                sl = sl.saturating_add(lc.saturating_mul(2).saturating_add(1));
                *lc = lc.saturating_add(1);
            }
            if let Some(rc) = right_counts.get_mut(c) {
                sr = sr.saturating_sub(rc.saturating_mul(2).saturating_sub(1));
                *rc = rc.saturating_sub(1);
            }
            n_left = n_left.saturating_add(1);
            let n_right = total.saturating_sub(n_left);

            // Only a real value boundary is a valid threshold, and both children
            // must be large enough.
            let value = feature_value(rows, i, feature);
            let next = order
                .get(pos.saturating_add(1))
                .map(|&j| feature_value(rows, j, feature));
            let boundary = next != Some(value);
            if !boundary || n_left < min_leaf || n_right < min_leaf {
                continue;
            }

            let score = sl
                .saturating_mul(SCORE_SCALE)
                .checked_div(n_left as u64)
                .unwrap_or(0)
                .saturating_add(
                    sr.saturating_mul(SCORE_SCALE)
                        .checked_div(n_right as u64)
                        .unwrap_or(0),
                );
            if score > best.map_or(base, |(_, _, s)| s) {
                best = Some((feature, value, score));
            }
        }
    }

    best.map(|(f, t, _)| (f, t))
}

fn feature_value(rows: &[Vec<i64>], i: usize, feature: usize) -> i64 {
    rows.get(i)
        .and_then(|r| r.get(feature))
        .copied()
        .unwrap_or(0)
}

fn sum_sq(counts: &[u64; MAX_CLASSES]) -> u64 {
    counts
        .iter()
        .fold(0u64, |acc, &c| acc.saturating_add(c.saturating_mul(c)))
}

#[cfg(test)]
mod tests {
    use super::{train, DecisionTree, TreeConfig};
    use alloc::vec;

    #[test]
    fn serialize_round_trips_and_predicts_identically() {
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
        let tree = train(&rows, &labels, 2, TreeConfig::new());
        let text = tree.serialize();
        let back = DecisionTree::deserialize(&text);
        assert!(back.is_some(), "a serialized tree must round-trip");
        if let Some(back) = back {
            for v in [5i64, 35, 65, 95] {
                assert_eq!(tree.predict(&[v, 7]), back.predict(&[v, 7]));
            }
            assert_eq!(back.size(), tree.size());
        }
        // Hostile input is rejected, not trusted.
        assert!(DecisionTree::deserialize("garbage").is_none());
        assert!(DecisionTree::deserialize("sovml-tree 1 2 99 1\nL 0 5").is_none());
    }

    #[test]
    fn learns_a_clean_axis_aligned_split() {
        // Class 0 when feature0 < 50, class 1 otherwise — trivially separable.
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
        let tree = train(&rows, &labels, 2, TreeConfig::new());
        // Points clearly inside each labelled region (0..40 vs 60..100); the
        // learned boundary sits somewhere in the unlabelled 40..60 gap.
        assert_eq!(tree.predict(&[10, 7]), 0);
        assert_eq!(tree.predict(&[35, 7]), 0);
        assert_eq!(tree.predict(&[65, 0]), 1);
        assert_eq!(tree.predict(&[90, 0]), 1);
    }

    #[test]
    fn a_pure_dataset_is_a_single_leaf() {
        let rows: Vec<Vec<i64>> = (0..30).map(|v| vec![v]).collect();
        let labels = vec![1u32; 30];
        let tree = train(&rows, &labels, 3, TreeConfig::new());
        assert_eq!(tree.size(), 1, "no split needed");
        assert_eq!(tree.predict(&[123]), 1);
    }

    #[test]
    fn learns_a_two_feature_rule() {
        // class 1 iff feature0 > 50 AND feature1 > 50, else class 0.
        let mut rows: Vec<Vec<i64>> = Vec::new();
        let mut labels: Vec<u32> = Vec::new();
        for a in [10i64, 90] {
            for b in [10i64, 90] {
                for _ in 0..20 {
                    rows.push(vec![a, b]);
                    labels.push(u32::from(a > 50 && b > 50));
                }
            }
        }
        let tree = train(&rows, &labels, 2, TreeConfig::new());
        assert_eq!(tree.predict(&[90, 90]), 1);
        assert_eq!(tree.predict(&[90, 10]), 0);
        assert_eq!(tree.predict(&[10, 90]), 0);
        assert_eq!(tree.predict(&[10, 10]), 0);
    }
}
