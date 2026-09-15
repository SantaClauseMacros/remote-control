// Cloudflare Worker entry point: serves the PWA (web/) as static assets, and
// routes `/relay` WebSocket upgrades to the Durable Object that implements
// the rendezvous relay (see relay.ts). One process, one deployment, one URL
// for both "the site my phone opens" and "the relay my PC dials out to".
export { RelayRouter } from "./relay";

interface Env {
  RELAY: DurableObjectNamespace;
  ASSETS: Fetcher;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/relay") {
      const id = env.RELAY.idFromName("global");
      const stub = env.RELAY.get(id);
      return stub.fetch(request);
    }

    return env.ASSETS.fetch(request);
  },
};
