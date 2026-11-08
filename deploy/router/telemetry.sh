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
LAN=$(uci -q get network.lan.ipaddr)
CLASH="http://${LAN}:9090"
if ubus call network.interface.wan status 2>/dev/null | grep -q '"up": true'; then
  U=$(ubus call network.interface.wan status 2>/dev/null | sed -n 's/.*"uptime": \([0-9]*\).*/\1/p' | head -1)
  echo "wan=up:${U:-0}"
else
  echo "wan=down"
fi
# L1 is independent of PPPoE: a live NIC with failing Discovery is not "no cable".
WAN_NIC=$(uci -q get network.wan.device)
WAN_NIC=${WAN_NIC:-eth1}
if [ -r "/sys/class/net/${WAN_NIC}/carrier" ]; then
  LINK=$(cat "/sys/class/net/${WAN_NIC}/carrier" 2>/dev/null || echo 0)
  echo "link=${LINK}"
  if [ "$LINK" = 1 ]; then
    SPD=$(cat "/sys/class/net/${WAN_NIC}/speed" 2>/dev/null || echo)
    case "$SPD" in
      ''|-1) ;;
      *) echo "linkmbps=$SPD" ;;
    esac
  fi
fi
if [ -d /sys/class/net/pppoe-wan ]; then
  echo "ppp=up"
elif pidof pppd >/dev/null 2>&1; then
  echo "ppp=discovery"
else
  echo "ppp=down"
fi
if pidof sing-box >/dev/null 2>&1; then echo "singbox=running"; else echo "singbox=dead"; fi
D=$(curl -s -m 8 -H "Authorization: Bearer $S" "$CLASH/proxies/vless_out-out/delay?url=http://cp.cloudflare.com/generate_204&timeout=6000" 2>/dev/null | sed -n 's/.*"delay":\([0-9]*\).*/\1/p')
if [ -n "$D" ]; then echo "tunnel=ok:$D"; else echo "tunnel=dead"; fi
G=$(curl -s -m 8 -H "Authorization: Bearer $S" "$CLASH/group/vless_out-out/delay?url=http://cp.cloudflare.com/generate_204&timeout=6000" 2>/dev/null)
echo "$G" | tr ',{}' '\n' | sed -n 's/.*"vless_out-\([0-9][0-9]*\)-out": *\([0-9][0-9]*\).*/node=\1:\2/p'
# nslookup has no timeout flag on this busybox and every other network call
# here is `curl -m`-bounded — guard this one the same way (background + kill),
# so a hung resolver can never hang the whole probe cycle. Found live 2026-08-09
# via a related bug in dns-selfheal: nslookup CAN take longer than assumed when
# the resolver is degraded, and an unguarded call has no upper bound at all.
DNS_OUT=/tmp/.telemetry-dns-out.$$
nslookup youtube.com 127.0.0.1 > "$DNS_OUT" 2>&1 &
dns_pid=$!
( sleep 5; kill -9 "$dns_pid" 2>/dev/null ) &
dns_watcher=$!
wait "$dns_pid" 2>/dev/null
dns_rc=$?
kill "$dns_watcher" 2>/dev/null
wait "$dns_watcher" 2>/dev/null
if [ "$dns_rc" -eq 0 ] && grep -qE 'Address[0-9]*: *198\.1[89]\.' "$DNS_OUT" 2>/dev/null; then
  echo "dns=ok"
else
  echo "dns=failing"
fi
rm -f "$DNS_OUT"
# WAN reachability must not use 1.1.1.1 / 8.8.8.8 HTTP — TSPU RSTs those
# on a working line, which is why classify ignored wanok for a year.
# A domestic resolver on 443: curl 0, or a TLS failure after the TCP handshake (35/51/60),
# means the TCP handshake to the probe completed. It does not prove TLS or any
# service works; a reset during the TLS handshake also lands in 35.
# Connect failure, timeout or a reset before TLS is reported as wanok=0.
curl -s -o /dev/null -m 5 -k https://77.88.8.8/ >/dev/null 2>&1
wan_rc=$?
case "$wan_rc" in
  0|35|51|60) echo "wanok=1" ;;
  *) echo "wanok=0" ;;
esac
awk '{ printf "load=%d\n", $1 * 100 }' /proc/loadavg 2>/dev/null
awk '/MemAvailable/ { printf "mem=%d\n", $2 }' /proc/meminfo 2>/dev/null
A=$(curl -s -m 8 -H "Authorization: Bearer $S" "$CLASH/proxies/vless_out-urltest-out" 2>/dev/null | sed -n 's/.*"now": *"vless_out-\([0-9][0-9]*\)-out".*/\1/p')
[ -n "$A" ] && echo "active=$A"
# Injection label: if a deliberate-fault marker is active (unexpired), stamp the
# sample with the ground-truth class, so a record taken during an injection is
# distinguishable from a real fault (also works while the laptop is off).
if [ -f /tmp/sovereign-inject ]; then
  IC=$(sed -n 's/.*class=\([a-z_]*\).*/\1/p' /tmp/sovereign-inject)
  IU=$(sed -n 's/.*until=\([0-9]*\).*/\1/p' /tmp/sovereign-inject)
  [ -n "$IC" ] && [ -n "$IU" ] && [ "$(date +%s)" -lt "$IU" ] && echo "inject=$IC"
fi
