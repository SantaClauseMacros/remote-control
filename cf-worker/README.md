# Cloudflare Worker: relay + site, one deployment

Serves the PWA ([`../web`](../web)) as static assets and runs the rendezvous
relay in a Durable Object, both from one `*.workers.dev` URL. The host dials
out to `wss://<your-worker>/relay` over WebSocket instead of raw TCP to a
self-hosted `rc-relay`; the wire protocol (Hello, ACKs, then opaque
ciphertext) is byte-identical either way, so nothing else about pairing,
encryption, or the client apps changes.

## Deploy

```bash
npm install
npx wrangler login      # once — opens a Cloudflare OAuth page in your browser
npx wrangler deploy
```

Wrangler prints the live URL, e.g. `https://remote-control.<your-account>.workers.dev`.
That's the link to open on your phone, and (with `wss://` and `/relay`
appended) what to put in the host's `config.toml`:

```toml
[network]
signaling_url = "wss://remote-control.<your-account>.workers.dev/relay"
```

The web app defaults its relay to "this same site's `/relay`" when you leave
the relay field blank when adding a device — so as long as the PWA and the
relay are this one Worker (the normal setup), your phone needs zero relay
configuration.

**Durable Objects** may require a Workers paid plan on some accounts — if
`wrangler deploy` rejects the `[[durable_objects.bindings]]` binding, that's
a plan change at dash.cloudflare.com, not a code problem.

## Local dev (no Cloudflare account needed)

```bash
npm run dev    # wrangler dev --port 8787 by default
```

Point the host at `signaling_url = "ws://127.0.0.1:8787/relay"` and open
`http://127.0.0.1:8787` in a browser to test the whole loop without touching
your real Cloudflare account. `crates/transport/examples/ws_relay_check.rs`
also exercises this relay directly (host + client, full Noise handshake, no
UI needed) — handy for a quick sanity check after changing `relay.ts`.

## How the relay works

One Durable Object instance (`RELAY.idFromName("global")`) holds all state:
a `Map<device_id, WebSocket>` of parked hosts, and a
`Map<WebSocket, WebSocket>` pairing each spliced host↔client. A device's
first WebSocket message is a `Hello` (decoded in [`src/postcard.ts`](src/postcard.ts) —
a tiny hand-rolled decoder for that one `postcard`-encoded Rust enum, kept in
lockstep with `crates/protocol/src/lib.rs`), then one ACK byte, then every
further message is forwarded to the peer verbatim. The Worker never sees
plaintext — everything forwarded is already Noise ciphertext.

Not implemented: the [Hibernatable WebSockets API](https://developers.cloudflare.com/durable-objects/best-practices/websockets/).
A parked host currently keeps its Durable Object "active" (billed) for as
long as it's parked, which could be a while for a personal box that's mostly
idle waiting for a phone to connect. Worth revisiting if that shows up in
your Cloudflare bill — functionally nothing changes for the client either way.

## Optional: a relay access key

`wrangler.toml` documents `RELAY_KEY` (a secret, `npx wrangler secret put RELAY_KEY`) —
an extra gate so only requests carrying the same string in `Hello.key` can
park or connect. Every session is already end-to-end Noise-authenticated
regardless, so this only matters if you want to keep randoms from even being
*able* to ask "is device X online" or occupy a parking slot.
