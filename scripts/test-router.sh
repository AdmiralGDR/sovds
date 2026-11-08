#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
# Run the on-router script checks in deploy/router/tests without network.
#
# The checks already stub every router command; a network namespace with no
# interfaces is a second fence, so a missed stub fails instead of reaching a
# real host. Order of attempts:
#   1. unshare -rn            (unprivileged user + network namespace)
#   2. sudo -n unshare -n     (CI runners where AppArmor blocks step 1),
#                             dropping back to the calling user with setpriv
# If neither works the run fails rather than going ahead with the network up.
set -eu
cd "$(dirname "$0")/.."

run_all() {
    status=0
    for t in deploy/router/tests/*.sh; do
        echo "== $t"
        sh "$t" || status=1
    done
    return "$status"
}

if [ "${1:-}" = --inside ]; then
    run_all
    exit $?
fi

if unshare -rn true 2>/dev/null; then
    exec unshare -rn sh "$0" --inside
fi

if command -v sudo >/dev/null 2>&1 && sudo -n unshare -n true 2>/dev/null; then
    exec sudo -n unshare -n setpriv --reuid="$(id -u)" --regid="$(id -g)" --init-groups \
        env HOME="$HOME" PATH="$PATH" sh "$0" --inside
fi

echo "test-router: cannot create a network namespace; refusing to run without one" >&2
exit 2
