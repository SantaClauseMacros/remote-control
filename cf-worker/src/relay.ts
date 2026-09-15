// Durable Object implementing the same rendezvous-relay protocol as
// `server/src/main.rs`'s `handle_ws`, so a Cloudflare Worker can stand in for
// a self-hosted `rc-relay` with zero changes to the host or web client wire
// format: first WS binary message is a `Hello`, the reply is one ACK byte,
// and everything after is forwarded verbatim between the two peers. This
// Worker never sees plaintext — every forwarded byte is already Noise
// ciphertext produced by rc-transport / rc-web.
//
// One instance (name "global") handles every device — plenty for personal
// use, and it means routing needs no per-device Durable Object lookup at the
// Worker layer. State is a couple of small in-memory Maps, reset if the
// instance restarts (a parked host would just need to reconnect, same as a
// restarted self-hosted rc-relay).
import { ACK_BAD_KEY, ACK_HOST_OFFLINE, ACK_OK, decodeHello } from "./postcard";

interface Env {
  RELAY_KEY?: string;
}

export class RelayRouter implements DurableObject {
  // device_id -> the host's parked WebSocket, waiting for a client.
  private parked = new Map<string, WebSocket>();
  // Once spliced, each side maps to the other so messages just forward.
  private peers = new Map<WebSocket, WebSocket>();
  // Sockets that sent a Hello but haven't spliced yet, so a later message on
  // them (there shouldn't be one before the ACK) doesn't get misrouted.
  private helloSeen = new Set<WebSocket>();

  constructor(
    private state: DurableObjectState,
    private env: Env,
  ) {}

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade") !== "websocket") {
      return new Response("expected a WebSocket upgrade", { status: 426 });
    }
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    server.accept();
    this.wire(server);
    return new Response(null, { status: 101, webSocket: client });
  }

  private wire(ws: WebSocket): void {
    ws.addEventListener("message", (event) => {
      const data = event.data;
      if (typeof data === "string") {
        ws.close(1002, "expected binary frames only");
        return;
      }
      const bytes = new Uint8Array(data as ArrayBuffer);
      if (!this.helloSeen.has(ws)) {
        this.helloSeen.add(ws);
        this.onHello(ws, bytes);
        return;
      }
      const peer = this.peers.get(ws);
      if (peer) {
        try {
          peer.send(bytes);
        } catch {
          this.teardown(ws);
        }
      }
    });
    ws.addEventListener("close", () => this.teardown(ws));
    ws.addEventListener("error", () => this.teardown(ws));
  }

  private keyOk(provided: string | null): boolean {
    const want = this.env.RELAY_KEY;
    if (!want) return true;
    return provided === want;
  }

  private onHello(ws: WebSocket, bytes: Uint8Array): void {
    const hello = decodeHello(bytes);
    if (!hello) {
      ws.close(1002, "malformed hello");
      return;
    }
    const { kind, deviceId, key } = hello;

    if (!this.keyOk(key)) {
      this.sendAck(ws, ACK_BAD_KEY);
      ws.close();
      return;
    }

    if (kind === "host") {
      const existing = this.parked.get(deviceId);
      if (existing && existing !== ws) {
        try {
          existing.close(1000, "replaced by a new host connection");
        } catch {
          /* already gone */
        }
      }
      this.parked.set(deviceId, ws);
      // No ACK yet — the host blocks until a client shows up, exactly like
      // the raw-TCP path.
      return;
    }

    if (kind === "query") {
      this.sendAck(ws, this.parked.has(deviceId) ? ACK_OK : ACK_HOST_OFFLINE);
      ws.close();
      return;
    }

    // kind === "client"
    const host = this.parked.get(deviceId);
    if (!host) {
      this.sendAck(ws, ACK_HOST_OFFLINE);
      ws.close();
      return;
    }
    this.parked.delete(deviceId);

    if (!this.sendAck(host, ACK_OK)) {
      // The parked host was actually dead (e.g. TCP half-open) — tell the
      // client rather than splicing to a socket that will never answer.
      this.sendAck(ws, ACK_HOST_OFFLINE);
      ws.close();
      return;
    }
    this.sendAck(ws, ACK_OK);

    this.peers.set(host, ws);
    this.peers.set(ws, host);
  }

  private sendAck(ws: WebSocket, ack: number): boolean {
    try {
      ws.send(new Uint8Array([ack]));
      return true;
    } catch {
      return false;
    }
  }

  private teardown(ws: WebSocket): void {
    const peer = this.peers.get(ws);
    if (peer) {
      this.peers.delete(ws);
      this.peers.delete(peer);
      try {
        peer.close(1000, "peer disconnected");
      } catch {
        /* already gone */
      }
    }
    this.helloSeen.delete(ws);
    for (const [id, sock] of this.parked) {
      if (sock === ws) this.parked.delete(id);
    }
  }
}
