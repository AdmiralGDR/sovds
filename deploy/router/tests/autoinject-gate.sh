#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
# Offline check of the sovereign-autoinject health gate. Every router command
# (curl, pidof, setsid, iptables) is a stub on PATH, the state paths point into
# a temporary directory, and nothing touches the network or /etc.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src="$here/../sovereign-autoinject"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin"
for cmd in pidof setsid iptables; do
    printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\nexit 0\n' "$cmd" "$work" > "$work/bin/$cmd"
done
# shellcheck disable=SC2016  # $* and CURL_RC belong to the generated stub
printf '#!/bin/sh\necho "curl $*" >> "%s/calls"\nexit "${CURL_RC:-0}"\n' "$work" > "$work/bin/curl"
chmod +x "$work/bin/"*
sed -e "s|^MARKER=.*|MARKER=$work/marker|" \
    -e "s|^STATE=.*|STATE=$work/state|" \
    -e "s|^KILL=.*|KILL=$work/noinject|" \
    -e "s|/etc/init.d/podkop|true|g" "$src" > "$work/autoinject"

fail=0
run_case() {
    name=$1 rc=$2 want=$3
    rm -f "$work/marker" "$work/state" "$work/calls"
    CURL_RC=$rc PATH="$work/bin:$PATH" sh "$work/autoinject" || true
    if [ -f "$work/state" ]; then got=inject; else got=skip; fi
    if [ "$got" = "$want" ]; then
        echo "ok   $name"
    else
        echo "FAIL $name: want $want, got $got"
        fail=1
    fi
}

run_case "curl ok"                0  inject
run_case "TLS error after connect" 35 inject
run_case "cert error after connect" 60 inject
run_case "connect refused"        7  skip
run_case "timeout"                28 skip
run_case "connection reset"       56 skip

# The gate must not probe plain HTTP to 1.1.1.1 or 8.8.8.8.
rm -f "$work/marker" "$work/state" "$work/calls"
CURL_RC=0 PATH="$work/bin:$PATH" sh "$work/autoinject" || true
if grep -Eq 'http://(1\.1\.1\.1|8\.8\.8\.8)' "$work/calls"; then
    echo "FAIL gate still probes plain HTTP"
    fail=1
else
    echo "ok   no plain HTTP probe"
fi
exit "$fail"
