#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
# Offline checks of the changed branches in dns-selfheal, podkop-selfheal and
# telemetry.sh. Router commands are stubs, absolute paths point into a
# temporary root, `sleep` returns at once.
# shellcheck disable=SC2016,SC2317  # stub bodies are single-quoted on purpose; helpers run via check
# A short real sleep stands in for `sleep`: the scripts time-guard nslookup with a
# background `sleep; kill`, and an instant sleep would kill the stub before it answers.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
R="$work/root"
fail=0

stub() {
    printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\n%s\nexit 0\n' "$1" "$work" "${2:-}" > "$work/bin/$1"
    chmod +x "$work/bin/$1"
}

setup() { # script ppp nslookup_ok
    rm -rf "$R" "${work:?}/bin"
    mkdir -p "$work/bin" "$R/sys/class/net/eth1" "$R/etc/init.d" "$R/usr/bin" "$R/tmp"
    : > "$work/calls"
    echo 1 > "$R/sys/class/net/eth1/carrier"
    echo 1000 > "$R/sys/class/net/eth1/speed"
    [ "$2" = 1 ] && mkdir -p "$R/sys/class/net/pppoe-wan"
    stub sleep "/bin/sleep 0.2"; stub logger; stub ubus
    if [ "${PPPD:-0}" = 1 ]; then stub pidof; else stub pidof "exit 1"; fi
    stub uci 'case "$*" in *lan.ipaddr*) echo 10.9.8.7;; *wan.device*) echo eth1;; *) echo s;; esac'
    if [ "$3" = 1 ]; then
        stub nslookup 'printf "Server: x\nAddress: x\n\nName: d\nAddress: 1.2.3.4\n"'
    else
        stub nslookup 'exit 1'
    fi
    stub curl 'case "$*" in *https://77.88.8.8/*) exit "${CURL_RC:-0}";; esac; exit 7'
    for f in sing-box podkop; do
        printf '#!/bin/sh\necho "%s-init $*" >> "%s/calls"\n' "$f" "$work" > "$R/etc/init.d/$f"
    done
    printf '#!/bin/sh\necho "ruleset-local" >> "%s/calls"\n' "$work" > "$R/usr/bin/sovereign-ruleset-local"
    chmod +x "$R/etc/init.d/"* "$R/usr/bin/sovereign-ruleset-local"
    sed -e "s|/tmp/|$R/tmp/|g" -e "s|/sys/class/net|$R/sys/class/net|g" \
        -e "s|/etc/|$R/etc/|g" -e "s|/usr/bin/|$R/usr/bin/|g" \
        "$here/../$1" > "$work/script"
}

run() {
    set +e
    PATH="$work/bin:$PATH" sh "$work/script" "$@" > "$work/out" 2>&1
    rc=$?
    set -e
}

check() {
    name=$1; shift
    if "$@"; then echo "ok   $name"; else echo "FAIL $name"; fail=1; fi
}
called() { grep -q "^$1" "$work/calls"; }
not_called() { ! grep -q "^$1" "$work/calls"; }
out_has() { grep -qx "$1" "$work/out"; }
rc_is() { [ "$rc" = "$1" ]; }
before() { # $1 appears before $2 in calls
    a=$(grep -n "^$1" "$work/calls" | head -1 | cut -d: -f1)
    b=$(grep -n "^$2" "$work/calls" | head -1 | cut -d: -f1)
    [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]
}

# dns-selfheal
setup dns-selfheal 1 1; run force
check "dns: resolving: exit 0"                 rc_is 0
check "dns: resolving: no restart"             not_called sing-box-init
setup dns-selfheal 0 0; run force
check "dns: PPPoE down: exit 0"                rc_is 0
check "dns: PPPoE down: no restart"            not_called sing-box-init
setup dns-selfheal 1 0; run force
check "dns: failing: rule-sets before restart" before ruleset-local "sing-box-init restart"

# podkop-selfheal
setup podkop-selfheal 1 1; : > "$R/etc/sovereign-failopen"; run run
check "podkop: fail-open: exit 0"              rc_is 0
check "podkop: fail-open: no API or uci write" eval 'not_called curl && ! grep -q "^uci set\|^uci commit" "$work/calls"'
check "podkop: fail-open: podkop untouched"    not_called podkop-init

# telemetry.sh
setup telemetry.sh 1 1; CURL_RC=35 run
check "telemetry: link=1"                      out_has link=1
check "telemetry: linkmbps=1000"               out_has linkmbps=1000
check "telemetry: ppp=up"                      out_has ppp=up
check "telemetry: TLS error after connect is wanok=1" out_has wanok=1
check "telemetry: Clash API on the UCI address" grep -q "http://10.9.8.7:9090/" "$work/calls"
check "telemetry: no fixed LAN address"        eval '! grep -q "192\.168\." "$work/calls"'
check "telemetry: no plain HTTP WAN probe"     eval '! grep -Eq "http://(1\.1\.1\.1|8\.8\.8\.8)" "$work/calls"'
setup telemetry.sh 0 1; CURL_RC=56 run
check "telemetry: reset is wanok=0"            out_has wanok=0
check "telemetry: no session, no pppd: ppp=down" out_has ppp=down
exit "$fail"
