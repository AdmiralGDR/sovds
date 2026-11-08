#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
# Offline checks of wan-selfheal branches. Every router command is a stub on
# PATH, /sys, /proc, /etc, /usr/bin and /tmp are rewritten into a temporary
# root, and `sleep` returns at once. Run it through ../../../scripts/test-router.sh,
# which also cuts the network.
# shellcheck disable=SC2016,SC2317  # stub bodies are single-quoted on purpose; helpers run via check
# A short real sleep stands in for `sleep`: the scripts time-guard nslookup with a
# background `sleep; kill`, and an instant sleep would kill the stub before it answers.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
src="$here/../wan-selfheal"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
R="$work/root"
fail=0

stub() { # name [extra shell]
    printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\n%s\nexit 0\n' "$1" "$work" "${2:-}" > "$work/bin/$1"
    chmod +x "$work/bin/$1"
}

setup() { # carrier ppp singbox curl_seq uptime state_last
    rm -rf "$R" "$work/calls" "${work:?}/bin"
    mkdir -p "$work/bin" "$R/sys/class/net/eth1" "$R/proc" "$R/etc/init.d" "$R/usr/bin" "$R/tmp"
    : > "$work/calls"
    echo "$1" > "$R/sys/class/net/eth1/carrier"
    [ "$2" = 1 ] && mkdir -p "$R/sys/class/net/pppoe-wan"
    echo "$5.00 1.00" > "$R/proc/uptime"
    [ -n "$6" ] && printf 'last=%s\nlevel=0\n' "$6" > "$R/tmp/wan-selfheal.state"
    echo "$4" > "$work/curl_seq"
    stub sleep "/bin/sleep 0.2"; stub logger; stub nft; stub ifdown
    stub ip
    stub ifup "[ \"\${PPP_AFTER_IFUP:-0}\" = 1 ] && mkdir -p '$R/sys/class/net/pppoe-wan'"
    stub uci 'echo eth1'
    stub nslookup 'printf "Server: x\nAddress: x\n\nName: example.com\nAddress: 1.2.3.4\n"'
    if [ "$3" = 1 ]; then stub pidof; else stub pidof 'exit 1'; fi
    # curl pops the next exit code from curl_seq; the last one repeats.
    cat > "$work/bin/curl" <<C
#!/bin/sh
echo "curl \$*" >> "$work/calls"
set -- \$(cat "$work/curl_seq")
rc=\$1
[ \$# -gt 1 ] && shift
echo "\$*" > "$work/curl_seq"
exit "\$rc"
C
    chmod +x "$work/bin/curl"
    printf '#!/bin/sh\necho "sing-box-init $*" >> "%s/calls"\n' "$work" > "$R/etc/init.d/sing-box"
    printf '#!/bin/sh\necho "ruleset-local" >> "%s/calls"\n' "$work" > "$R/usr/bin/sovereign-ruleset-local"
    chmod +x "$R/etc/init.d/sing-box" "$R/usr/bin/sovereign-ruleset-local"
    # /tmp first: the temporary root itself lives under /tmp.
    sed -e "s|/tmp/|$R/tmp/|g" -e "s|/sys/class/net|$R/sys/class/net|g" \
        -e "s|/proc/uptime|$R/proc/uptime|g" -e "s|/etc/|$R/etc/|g" \
        -e "s|/usr/bin/|$R/usr/bin/|g" "$src" > "$work/wan-selfheal"
}

run() { # mode
    set +e
    PATH="$work/bin:$PATH" sh "$work/wan-selfheal" "$1" > "$work/out" 2>&1
    rc=$?
    set -e
}

check() { # name condition...
    name=$1; shift
    if "$@"; then echo "ok   $name"; else echo "FAIL $name"; fail=1; fi
}
called() { grep -q "^$1" "$work/calls"; }
not_called() { ! grep -q "^$1" "$work/calls"; }
rc_is() { [ "$rc" = "$1" ]; }
never_reboots() { ! grep -Eq '^(reboot|poweroff)' "$work/calls"; }

setup 1 1 1 "0" 1000 ""; run run
check "healthy: exit 0"                    rc_is 0
check "healthy: no bounce"                 not_called ifdown
check "healthy: sing-box left alone"       not_called sing-box-init

setup 1 1 0 "35" 1000 ""; run run
check "TLS after connect counts as up"     rc_is 0
check "healthy, sing-box dead: started"    called "sing-box-init start"
check "healthy, sing-box dead: no bounce"  not_called ifdown

setup 0 1 1 "7" 5000 ""; run run
check "no carrier: exit 1"                 rc_is 1
check "no carrier: no bounce"              not_called ifdown

setup 1 1 1 "7" 5000 "4800"; run run
check "rate limit: exit 0"                 rc_is 0
check "rate limit: no bounce"              not_called ifdown

setup 1 1 1 "7 7 7 0" 5000 ""; run run
check "session up, TCP dead: PHY bounce"   called "ip link set eth1 down"
check "PHY bounce brings link back up"     called "ip link set eth1 up"
check "recovered after bounce: exit 0"     rc_is 0
check "rule-sets pinned"                   called ruleset-local

setup 1 0 1 "7 7 7 0" 5000 ""; PPP_AFTER_IFUP=1; export PPP_AFTER_IFUP; run run; unset PPP_AFTER_IFUP
check "no session: PPPoE bounce"           called "ifdown wan"
check "no session: no PHY bounce"          not_called "ip link"
check "no session, recovered: exit 0"      rc_is 0

setup 1 1 1 "7" 5000 ""; run run
check "still dead: exit 1"                 rc_is 1
check "still dead: final log written"      grep -q "TCP still dead" "$work/calls"
check "still dead: no unset-variable abort" eval '! grep -q "parameter not set" "$work/out"'
check "never reboots"                      never_reboots

setup 1 1 1 "7" 5000 ""; run dry
check "dry: exit 0"                        rc_is 0
check "dry: report printed"                grep -q "carrier=1 ppp=session udp=ok tcp=fail" "$work/out"
check "dry: no actuation"                  eval 'not_called ifdown && not_called ip && not_called sing-box-init && not_called ruleset-local'

exit "$fail"
