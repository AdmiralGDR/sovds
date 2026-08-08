#!/bin/sh
# Reproducible-build gate.
#
# The build must be a pure function of the source: same commit -> byte-identical
# bytes, for EVERY shipped binary, not just one. We remove the two remaining
# sources of non-determinism —
#   1. build timestamps  -> pinned via SOURCE_DATE_EPOCH (the commit time),
#   2. absolute paths     -> normalized via --remap-path-prefix,
# and forbid dependency drift with --locked. Since the tree has zero third-party
# crates, there is nothing else to vary.
#
# This is a zero-touch gate: it builds the workspace TWICE into two
# isolated target dirs and asserts the hashes match, exiting non-zero on any
# mismatch. Isolated dirs mean it never disturbs the working `target/` dev cache.
# Pass `--print` to also emit the canonical per-binary hashes.
set -eu

cd "$(dirname "$0")/.."

SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct 2>/dev/null || echo 0)"
export SOURCE_DATE_EPOCH

# Map this checkout's absolute path to a fixed prefix so debug strings do not
# embed a machine-specific path.
RUSTFLAGS="--remap-path-prefix=$PWD=/sovereign ${RUSTFLAGS:-}"
export RUSTFLAGS

# Every binary the user actually runs — the sovereignty guarantee must cover the
# whole toolset, not only the supervisor.
BINS="sovereign sovereign-inspect sovereign-replay sovereign-backfill sovereign-scenario sovereign-episodes sovereign-inject sovereign-export sovereign-anomaly sovereign-train"

sha() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}

# Hash every shipped binary under target dir $1, as "hash  name" lines in a stable
# order (so the two runs are directly comparable).
hash_all() {
    for b in $BINS; do
        h=$(sha "$1/release/$b" | awk '{print $1}')
        echo "$h  $b"
    done
}

D1=$(mktemp -d)
D2=$(mktemp -d)
trap 'rm -rf "$D1" "$D2"' EXIT

echo "reproducible-build: SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH; building twice (isolated)..."
CARGO_TARGET_DIR="$D1" cargo build --release --locked --bins
CARGO_TARGET_DIR="$D2" cargo build --release --locked --bins

A=$(hash_all "$D1")
B=$(hash_all "$D2")

if [ "$A" = "$B" ]; then
    echo "REPRODUCIBLE ✓ — all $(echo "$BINS" | wc -w | tr -d ' ') binaries byte-identical across clean builds"
    if [ "${1:-}" = "--print" ]; then echo "$A"; fi
    exit 0
else
    echo "MISMATCH ✗ — reproducibility regression; the build is not a pure function of the source" >&2
    # Show which binaries differ.
    printf '%s\n' "$A" > "$D1/a.txt"
    printf '%s\n' "$B" > "$D2/b.txt"
    diff "$D1/a.txt" "$D2/b.txt" >&2 || true
    exit 1
fi
