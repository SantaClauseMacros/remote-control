# Deploying the rendezvous relay

The relay lets a host with no public IP be reached from any network. The host
dials **out** to the relay and parks a connection; a client asks the relay for
that host by its device id and the relay splices the two sockets together. It
forwards only the end-to-end Noise-encrypted stream — it has no pairing code and
cannot read or inject session traffic.

## Run it

Any always-on box with a public IP or port-forward works (a $5 VPS, a
Raspberry Pi, a home server). Resident memory is a few MB.

### Docker

```bash
cd deploy
# optional: echo "RC_RELAY_KEY=$(openssl rand -hex 16)" > .env
docker compose up -d
```

### Bare binary

```bash
cargo build --release -p rc-relay
./target/release/rc-relay --bind 0.0.0.0:9878 [--key SECRET] [--max-conns 512]
```

Open **TCP 9878** to the box. No other ports.

## Point the pieces at it

**Host** — in `%APPDATA%\RemoteControl\RemoteControl\config\config.toml`:

```toml
[network]
signaling_url = "relay.example.com:9878"
relay_key = "SECRET"        # only if you used --key
```

Reload settings from the tray (or restart the host). The tray status shows
"relay parked" when it's registered.

**Client**:

```bash
# device id is shown in the host's tray menu and in `--list`
rc-desktop-client --relay relay.example.com:9878 ABCDE-FGHIJ-KLMNO-P 482193
# or:  RC_RELAY=relay.example.com:9878  RC_RELAY_KEY=SECRET  rc-desktop-client ABCDE-... 482193
```

## Security notes

* The relay authenticates nothing except the optional `--key` (an anti-abuse
  measure, not a session control). Session security is the end-to-end Noise
  handshake keyed by the 6-digit pairing code, which rotates after every
  session.
* A hostile relay can drop or delay traffic (denial of service) but cannot read
  the screen/input stream or impersonate either peer.
* Milestone 4b adds a WebRTC/ICE path so most sessions go directly
  peer-to-peer and only fall back to the relay when NAT prevents a direct hole
  punch.
