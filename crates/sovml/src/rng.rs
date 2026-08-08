// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A deterministic, seedable integer pseudo-random generator — the sovereign
//! source of randomness for ensemble methods (bagging, feature subsampling).
//!
//! It is **splitmix64**: a tiny, well-studied generator whose whole state is one
//! `u64`. There is no system entropy and no float anywhere — the same seed yields
//! the same stream on every machine, so a model trained with it is reproducible
//! bit-for-bit. All arithmetic is wrapping or checked, so it can
//! never panic or overflow-trap.

/// A seedable splitmix64 generator. Clone it to fork an independent stream.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Create a generator from a seed. Any seed is valid (including 0).
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 bits of the stream (the splitmix64 mixing function).
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ z.wrapping_shr(31)
    }

    /// A value in `0..bound` (uniform enough for sampling; `0` if `bound` is 0).
    pub fn below(&mut self, bound: usize) -> usize {
        let b = bound as u64;
        let r = self.next_u64().checked_rem(b).unwrap_or(0);
        usize::try_from(r).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;
    use alloc::vec::Vec;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        // Overwhelmingly likely to differ within a few draws.
        let diff = (0..8).any(|_| a.next_u64() != b.next_u64());
        assert!(diff, "distinct seeds must produce distinct streams");
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            assert!(r.below(6) < 6);
        }
        assert_eq!(r.below(0), 0, "a zero bound is safe");
    }

    #[test]
    fn below_covers_the_range() {
        // Every bucket in 0..6 should be hit over enough draws (no dead slot).
        let mut r = Rng::new(99);
        let mut seen: Vec<bool> = (0..6).map(|_| false).collect();
        for _ in 0..1000 {
            let v = r.below(6);
            if let Some(slot) = seen.get_mut(v) {
                *slot = true;
            }
        }
        assert!(seen.iter().all(|&s| s), "every bucket must be reachable");
    }
}
