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
