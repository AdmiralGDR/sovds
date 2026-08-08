#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# verify-integrity.sh — checks installed binaries against SHA256SUMS.
#
# IP-Shield principles call for self-integrity checks. On THIS system
# (laptop + a home router that also runs live self-heal with sudo and keeps
# the household on the internet) an unconditional "self-destruct on mismatch"
# is a new failure mode, not a defense — a false positive would take out
# infrastructure that has nothing to do with IP theft. So this checks and
# loudly ALERTS on mismatch; it never deletes, kills, or disables anything.
# Detection + alerting is the correct CIA-triad trade for this system:
# availability is prioritized, integrity violations are surfaced, not hidden.
#
# Usage: verify-integrity.sh   (checks ~/.local/bin/* against SHA256SUMS here)

set -u
cd "$(dirname "$0")/.."

FAIL=0
while IFS= read -r line; do
    case "$line" in \#*|"") continue ;; esac
    hash=$(printf '%s' "$line" | awk '{print $1}')
    name=$(printf '%s' "$line" | awk '{print $2}')
    bin="$HOME/.local/bin/$name"
    [ -f "$bin" ] || continue   # not installed locally (e.g. router-only script) — skip
    actual=$(sha256sum "$bin" 2>/dev/null | awk '{print $1}')
    if [ "$actual" != "$hash" ]; then
        echo "CRITICAL: $name does not match SHA256SUMS (expected $hash, got $actual)" >&2
        command -v logger >/dev/null && logger -t sovereign-integrity "CRITICAL: $name hash mismatch"
        FAIL=1
    fi
done < SHA256SUMS

if [ "$FAIL" -eq 0 ]; then
    echo "integrity OK — all installed binaries match SHA256SUMS"
fi
exit "$FAIL"
