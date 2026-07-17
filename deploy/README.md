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
#    password itself lives only in the 0600 file it reads).
mkdir -p ~/.local/share/sovereign
cat > ~/.local/share/sovereign/askpass.sh <<'SH'
#!/bin/bash
grep -E '^password:' $ROUTER_PASSWORD_FILE | sed 's/^password:[[:space:]]*//' | tr -d '\r\n'; echo
SH
chmod 700 ~/.local/share/sovereign/askpass.sh

# 3. Install and start the service
cp deploy/sovereign-collector.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sovereign-collector.service
loginctl enable-linger "$USER"   # keep collecting across logout/reboot
```

## Safety

- **Observe-only by default.** The unit does **not** set `SOVEREIGN_ACTUATE`, so
  the brain uses the `NullActuator` and cannot touch the router (safety by
  construction, the principle). Actuation is a separate, deliberate decision — enable it
  only once accumulated data has validated the decision rules.
- **Bounded on disk.** The log rotates at `SOVEREIGN_LOG_MAX_BYTES` across
  `SOVEREIGN_LOG_GENERATIONS` files (~200 MB total here), so the trail grows in
  time without growing without bound (the principle + longevity).
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

- **`deploy/router/sovereign-probe`** — busybox script on the router (cron,
  1/min): samples the same telemetry contract into `/tmp/sovereign-buffer.log`
  (tmpfs = RAM only, ring-bounded at 4320 lines ≈ 3 days; router flash and the
  user's "data lives on the laptop" rule are respected).
- **`sovereign-backfill`** (+ `sovereign-backfill.{service,timer}`, every 15 min
  and 2 min after boot): drains the buffer loss-free (leftover `.drain` is folded
  in; remote delete only after local append), validates each line through the
  same parser as live telemetry, dedupes against the on-disk dataset (±45 s), and
  replays survivors through the same decision pipeline. Records carry
  `src=backfill` so analysis can distinguish provenance; nothing is actuated
  (`outcome=-`).

Install (router): copy `deploy/router/sovereign-probe` to `/usr/bin/`, add cron
`* * * * * /usr/bin/sovereign-probe`. Install (laptop): the two units above,
then `systemctl --user enable --now sovereign-backfill.timer`.
