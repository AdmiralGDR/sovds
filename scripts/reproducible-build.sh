#!/bin/sh
# Reproducible release build (the internal engineering charter, the principle).
#
# The build is meant to be a pure function of the source: same commit -> same
# bytes. We remove the two remaining sources of non-determinism —
#   1. build timestamps  -> pinned via SOURCE_DATE_EPOCH (the commit time),
#   2. absolute paths     -> normalized via --remap-path-prefix,
# and forbid dependency drift with --locked. Since the tree has zero third-party
# crates, there is nothing else to vary.
#
# Run it twice, compare the sha256; they must match.
set -eu

cd "$(dirname "$0")/.."

SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct 2>/dev/null || echo 0)"
export SOURCE_DATE_EPOCH

# Map this checkout's absolute path (and the registry, though we use none) to a
# fixed prefix so debug strings do not embed a machine-specific path.
RUSTFLAGS="--remap-path-prefix=$PWD=/sovereign ${RUSTFLAGS:-}"
export RUSTFLAGS

cargo build --release --locked

BIN="target/release/sovereign"
echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$BIN"
else
    shasum -a 256 "$BIN"
fi
