// Minimal ambient declarations for the Cloudflare Workers runtime pieces the relay
// uses. The repo deliberately avoids @cloudflare/workers-types (the tombstoned hub
// never needed them); extend here if the surface grows.

declare class WebSocketPair {
  0: WebSocket;
  1: WebSocket;
}

interface WebSocket {
  accept(): void;
}

interface ResponseInit {
  webSocket?: WebSocket | null;
}

interface DurableObjectId {
  toString(): string;
}

interface DurableObjectStub {
  fetch(request: Request): Promise<Response>;
}

interface DurableObjectNamespace {
  idFromName(name: string): DurableObjectId;
  get(id: DurableObjectId): DurableObjectStub;
}
