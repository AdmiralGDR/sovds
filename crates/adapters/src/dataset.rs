// Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Dataset loading for the router domain — a thin specialisation of the generic
//! [`sovds::dataset`] to [`RouterDomain`]. The loading logic lives in the toolkit;
//! this just fixes the domain so callers keep saying `dataset::load(path, gens)`.

use sovereign_core::routerdomain::RouterDomain;
use std::path::Path;

/// A loaded corpus of router records.
pub type Corpus = sovds::dataset::Corpus<RouterDomain>;

/// Load `path` and up to `generations` rotated siblings into a [`Corpus`].
#[must_use]
pub fn load(path: &Path, generations: u32) -> Corpus {
    sovds::dataset::load::<RouterDomain>(path, generations)
}
