# Deployment — continuous data collection

The brain runs on the **laptop/server, never on the router** (the router is only
polled over SSH; all records are written to the local filesystem). The reference
deployment is a hardened **systemd user service** that runs it in observe-only
mode and appends to a rotating log.

`sovereign-collector.service` is the live unit from this laptop; adjust the
absolute paths for another machine.

## Install

```sh
# 1. Build and install the binary
cargo build --release --locked
install -m 755 target/release/sovereign ~/.local/bin/sovereign

# 2. Data directory + askpass helper (prints the router root password; the
#    password itself lives only in a 0600 file you create yourself, OUTSIDE
#    this repo — never commit a password file, never hardcode its path).
mkdir -p ~/.local/share/sovereign
ROUTER_PASSWORD_FILE="${ROUTER_PASSWORD_FILE:-$HOME/.config/sovereign/router-password.txt}"
install -m 600 /dev/null "$ROUTER_PASSWORD_FILE"   # then put ONE line: password: <yours>
cat > ~/.local/share/sovereign/askpass.sh <<SH
#!/bin/bash
grep -E '^password:' '$ROUTER_PASSWORD_FILE' | sed 's/^password:[[:space:]]*//' | tr -d '\r\n'; echo
SH
chmod 700 ~/.local/share/sovereign/askpass.sh

# 3. Pin the router's SSH host key (REQUIRED — host-key checking is strict).
#    Read the key on the router itself (console / a session you already trust),
#    not over the network you are trying to secure:
#      dropbearkey -y -f /etc/dropbear/dropbear_ed25519_host_key | grep ^ssh-
#    Then EITHER pin it directly in the unit:
#      Environment=SOVEREIGN_HOST_KEY=ssh-ed25519 AAAA...
#    OR keep a dedicated known_hosts file:
mkdir -p ~/.config/sovereign
echo "192.168.1.1 ssh-ed25519 AAAA...from-router-console..." >> ~/.config/sovereign/known_hosts
#    After a reflash the router key changes and every SSH call fails with
#    "SSH host key ... was REJECTED" in the journal: re-pin the new key.

# 4. (Recommended) Key auth instead of the root password:
ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_sovereign_router
#    append ~/.ssh/id_sovereign_router.pub to /etc/dropbear/authorized_keys on
#    the router, set SOVEREIGN_SSH_IDENTITY, and drop SOVEREIGN_ASKPASS. With no
#    askpass configured, password auth is disabled entirely (BatchMode=yes).

# 5. Install and start the service
cp deploy/sovereign-collector.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sovereign-collector.service
loginctl enable-linger "$USER"   # keep collecting across logout/reboot
```

## Safety

- **Observe-only by default.** The unit does **not** set `SOVEREIGN_ACTUATE`, so
  the brain uses the `NullActuator` and cannot touch the router (safety by
  construction). Actuation is a separate, deliberate decision — enable it
  only once accumulated data has validated the decision rules.
- **Router identity is verified.** SSH runs with `StrictHostKeyChecking=yes`
  against `SOVEREIGN_KNOWN_HOSTS` (or `~/.ssh/known_hosts`), or against the
  single key pinned in `SOVEREIGN_HOST_KEY` (needs OpenSSH >= 8.5). Public-key
  auth is always tried first; the askpass password is only a fallback. The
  legacy accept-any-key mode exists only behind
  `SOVEREIGN_SSH_INSECURE_ACCEPT_ANY_HOST_KEY=1` and is announced at startup.
  In that mode the askpass password is never sent (key auth only), and a
  pinned key in `SOVEREIGN_HOST_KEY` always takes precedence over it.
- **Bounded on disk.** The log rotates at `SOVEREIGN_LOG_MAX_BYTES` across
  `SOVEREIGN_LOG_GENERATIONS` files (~200 MB total here), so the trail grows in
  time without growing without bound (plus longevity).
- **Graceful when off-network.** If the laptop leaves the router's LAN, cycles
  record `probed=0` and the service keeps running; collection resumes on return.

## Inspect

```sh
tail -f ~/.local/share/sovereign/records.log
journalctl --user -u sovereign-collector.service -f
```

## Fallback: router-side telemetry buffer (laptop off / asleep)

The dataset must not go blind when the laptop is away (a real 74.8 h gap
motivated this). Two halves:

- **`deploy/router/telemetry.sh`** — the **single source** of the telemetry
  contract (what is gathered, how each token is named). The laptop collector embeds
  it at compile time (`include_str!`) and the probe below sources it, so the two can
  never diverge. Deploy to `/usr/bin/sovereign-telemetry.sh`.
- **`deploy/router/sovereign-probe`** — busybox script on the router (cron,
  1/min): runs `sovereign-telemetry.sh` and folds its output into
  `/tmp/sovereign-buffer.log` (tmpfs = RAM only, ring-bounded ≈ 14 days; router
  flash and the "data lives on the laptop" rule are respected).
- **`sovereign-backfill`** (+ `sovereign-backfill.{service,timer}`, every 15 min
  and 2 min after boot): drains the buffer loss-free (leftover `.drain` is folded
  in; remote delete only after local append), validates each line through the
  same parser as live telemetry, dedupes against the on-disk dataset (±45 s), and
  replays survivors through the same decision pipeline. Records carry
  `src=backfill` so analysis can distinguish provenance; nothing is actuated
  (`outcome=-`).

Install (router): copy `deploy/router/telemetry.sh` to
`/usr/bin/sovereign-telemetry.sh` and `deploy/router/sovereign-probe` to
`/usr/bin/`, add cron `* * * * * /usr/bin/sovereign-probe`. Install (laptop): the
two units above, then `systemctl --user enable --now sovereign-backfill.timer`.
