#!/bin/sh
# Copyright (C) 2026 AdmiralGDR. Licensed under the GNU Affero General Public License v3.0 or later.
# SPDX-License-Identifier: AGPL-3.0-or-later
# sovereign telemetry — the SINGLE source of what telemetry is gathered and how
# each token is named. Emits one `key=value` token per line to stdout, which the
# brain's parser (`parse_snapshot`) validates.
#
# Used two ways from ONE definition (no duplicated logic):
#   * the laptop collector runs this verbatim over SSH each cycle;
#   * the router-side sovereign-probe sources it into the RAM fallback buffer.
# A failed sub-probe emits its own token (tunnel=dead etc.) and never hangs — every
# network call has a hard timeout (bounded, graceful degradation).
S=$(uci -q get podkop.settings.yacd_secret_key)
if ubus call network.interface.wan status 2>/dev/null | grep -q '"up": true'; then
  U=$(ubus call network.interface.wan status 2>/dev/null | sed -n 's/.*"uptime": \([0-9]*\).*/\1/p' | head -1)
  echo "wan=up:${U:-0}"
else
  echo "wan=down"
fi
if pidof sing-box >/dev/null 2>&1; then echo "singbox=running"; else echo "singbox=dead"; fi
D=$(curl -s -m 8 -H "Authorization: Bearer $S" "http://192.168.1.1:9090/proxies/vless_out-out/delay?url=http://cp.cloudflare.com/generate_204&timeout=6000" 2>/dev/null | sed -n 's/.*"delay":\([0-9]*\).*/\1/p')
if [ -n "$D" ]; then echo "tunnel=ok:$D"; else echo "tunnel=dead"; fi
G=$(curl -s -m 8 -H "Authorization: Bearer $S" "http://192.168.1.1:9090/group/vless_out-out/delay?url=http://cp.cloudflare.com/generate_204&timeout=6000" 2>/dev/null)
echo "$G" | tr ',{}' '\n' | sed -n 's/.*"vless_out-\([0-9][0-9]*\)-out": *\([0-9][0-9]*\).*/node=\1:\2/p'
if nslookup youtube.com 127.0.0.1 2>/dev/null | grep -qE 'Address[0-9]*: *198\.1[89]\.'; then echo "dns=ok"; else echo "dns=failing"; fi
if curl -s -o /dev/null -m 5 http://1.1.1.1 2>/dev/null || curl -s -o /dev/null -m 5 http://8.8.8.8 2>/dev/null; then echo "wanok=1"; else echo "wanok=0"; fi
awk '{ printf "load=%d\n", $1 * 100 }' /proc/loadavg 2>/dev/null
awk '/MemAvailable/ { printf "mem=%d\n", $2 }' /proc/meminfo 2>/dev/null
A=$(curl -s -m 8 -H "Authorization: Bearer $S" "http://192.168.1.1:9090/proxies/vless_out-urltest-out" 2>/dev/null | sed -n 's/.*"now": *"vless_out-\([0-9][0-9]*\)-out".*/\1/p')
[ -n "$A" ] && echo "active=$A"
# Injection label: if a deliberate-fault marker is active (unexpired), stamp the
# sample with the ground-truth class, so a record taken during an injection is
# distinguishable from a real fault (also works while the laptop is off).
if [ -f /tmp/sovereign-inject ]; then
  IC=$(sed -n 's/.*class=\([a-z_]*\).*/\1/p' /tmp/sovereign-inject)
  IU=$(sed -n 's/.*until=\([0-9]*\).*/\1/p' /tmp/sovereign-inject)
  [ -n "$IC" ] && [ -n "$IU" ] && [ "$(date +%s)" -lt "$IU" ] && echo "inject=$IC"
fi
