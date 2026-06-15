// Session relay (issue #22 — docs/issues/22-web-relay.md).
//
// One Durable Object per view token. The local pie process connects an OUTBOUND
// WebSocket to `/relay/agent?token=…`, authenticates with an agent key (pinned on first
// connect — TOFU), and pushes WebSnapshot frames. Browsers hit
// `/session/<token>/{,state,events,prompt,abort}`; snapshots fan out over SSE. Memory
// only: nothing is written to D1/KV, and a `shutdown` frame (or `/web-disconnect`)
// makes the token 404 forever after.
//
// `RelayCore` is the pure, node-testable state machine; `SessionRelay` wraps it with
// the Workers-runtime pieces (WebSocketPair, SSE streams).

export interface AgentHello {
  type: "hello";
  agent_key: string;
}
export interface AgentSnapshot {
  type: "snapshot";
  data: unknown;
}
export interface AgentShutdown {
  type: "shutdown";
}
export type AgentFrame = AgentHello | AgentSnapshot | AgentShutdown;

/// Maximum accepted agent frame (mirrors the client-side 1 MiB cap, plus envelope slack).
export const MAX_AGENT_FRAME_BYTES = 1_200_000;

export type CoreEffect =
  | { kind: "accept" }
  | { kind: "reject"; reason: "bad_key" | "bad_frame" | "not_hello" | "oversized" }
  | { kind: "broadcast"; snapshot: string }
  | { kind: "shutdown" };

/**
 * Pure relay state machine. The first frame on a new agent socket must be `hello`;
 * the first hello ever pins the agent key, later hellos must match it.
 */
export class RelayCore {
  agentKey: string | null = null;
  helloSeen = false;
  latestSnapshot: string | null = null;
  closed = false;

  /** Reset per-connection state when a fresh agent socket attaches. */
  agentConnected(): void {
    this.helloSeen = false;
  }

  handleAgentMessage(raw: string): CoreEffect {
    if (this.closed) {
      return { kind: "shutdown" };
    }
    if (raw.length > MAX_AGENT_FRAME_BYTES) {
      return { kind: "reject", reason: "oversized" };
    }
    let frame: AgentFrame;
    try {
      frame = JSON.parse(raw) as AgentFrame;
    } catch {
      return { kind: "reject", reason: "bad_frame" };
    }
    if (!this.helloSeen) {
      if (frame.type !== "hello" || typeof frame.agent_key !== "string" || !frame.agent_key) {
        return { kind: "reject", reason: "not_hello" };
      }
      if (this.agentKey === null) {
        this.agentKey = frame.agent_key; // trust on first use
      } else if (this.agentKey !== frame.agent_key) {
        return { kind: "reject", reason: "bad_key" };
      }
      this.helloSeen = true;
      return { kind: "accept" };
    }
    switch (frame.type) {
      case "snapshot": {
        const snapshot = JSON.stringify(frame.data ?? null);
        this.latestSnapshot = snapshot;
        return { kind: "broadcast", snapshot };
      }
      case "shutdown": {
        this.closed = true;
        this.latestSnapshot = null;
        return { kind: "shutdown" };
      }
      default:
        return { kind: "reject", reason: "bad_frame" };
    }
  }
}

/** Validate a model spec string from a `set_model` request body. Returns the trimmed spec
 * or `null` if the value is not a non-empty string within the length limit. */
export function validateSetModel(model: unknown): string | null {
  if (typeof model !== "string") return null;
  const spec = model.trim();
  if (!spec || spec.length > 256) return null;
  return spec;
}

/** `/session/<token>` and below. `rest` is "" (no trailing slash), "/", or "/state" etc. */
export function parseSessionPath(pathname: string): { token: string; rest: string } | null {
  const match = /^\/session\/([^/]+)(\/.*)?$/.exec(pathname);
  if (!match) return null;
  const token = match[1];
  if (!isValidToken(token)) return null;
  return { token, rest: match[2] ?? "" };
}

/** Tokens are 40 lowercase hex chars from the pie client; accept a small range. */
export function isValidToken(token: string): boolean {
  return /^[0-9a-f]{32,64}$/.test(token);
}

// ── Workers-runtime wrapper ────────────────────────────────────────────────────────────

interface SseViewer {
  controller: ReadableStreamDefaultController<Uint8Array>;
}

export class SessionRelay {
  private core = new RelayCore();
  private agentSocket: WebSocket | null = null;
  private viewers = new Set<SseViewer>();
  private encoder = new TextEncoder();

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname; // rewritten by the router: /agent, /, /state, /events, …

    if (this.core.closed && path !== "/agent") {
      return json({ ok: false, error: "session_closed" }, 404);
    }

    switch (path) {
      case "/agent":
        return this.handleAgentUpgrade(request);
      case "/":
        return new Response(VIEWER_HTML_REF.html, {
          headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" },
        });
      case "/state":
        return this.core.latestSnapshot
          ? new Response(this.core.latestSnapshot, {
              headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
            })
          : json({ ok: false, error: "agent_offline" }, 503);
      case "/events":
        return this.handleEvents();
      case "/prompt":
        return this.forwardPrompt(request);
      case "/abort":
        return this.forward({ type: "abort" });
      case "/model":
        return this.forwardSetModel(request);
      case "/complete":
        return json({ completions: [] });
      case "/control-plane/resolve":
        return this.forwardResolve(request);
      default:
        return json({ ok: false, error: "not_found" }, 404);
    }
  }

  private handleAgentUpgrade(request: Request): Response {
    if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
      return json({ ok: false, error: "websocket_required" }, 426);
    }
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    server.accept();
    // Only one live agent socket; a reconnect replaces the previous one.
    if (this.agentSocket) {
      try {
        this.agentSocket.close(4000, "replaced");
      } catch {
        // already gone
      }
    }
    this.agentSocket = server;
    this.core.agentConnected();

    server.addEventListener("message", (event: MessageEvent) => {
      const raw = typeof event.data === "string" ? event.data : "";
      const effect = this.core.handleAgentMessage(raw);
      switch (effect.kind) {
        case "accept":
          this.sendViewersCount();
          this.broadcastStatus(true);
          break;
        case "broadcast":
          this.broadcastSse("snapshot", effect.snapshot);
          break;
        case "shutdown":
          this.closeAll();
          break;
        case "reject":
          try {
            server.close(4403, effect.reason);
          } catch {
            // already gone
          }
          if (this.agentSocket === server) {
            this.agentSocket = null;
          }
          break;
      }
    });
    const dropped = () => {
      if (this.agentSocket === server) {
        this.agentSocket = null;
        this.broadcastStatus(false);
      }
    };
    server.addEventListener("close", dropped);
    server.addEventListener("error", dropped);

    return new Response(null, { status: 101, webSocket: client });
  }

  private handleEvents(): Response {
    const viewer: SseViewer = { controller: null as unknown as ReadableStreamDefaultController<Uint8Array> };
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        viewer.controller = controller;
        this.viewers.add(viewer);
        if (this.core.latestSnapshot) {
          this.sendSse(viewer, "snapshot", this.core.latestSnapshot);
        }
        this.sendSse(
          viewer,
          "relay_status",
          JSON.stringify({ agent_online: this.agentSocket !== null }),
        );
        this.sendViewersCount();
      },
      cancel: () => {
        this.viewers.delete(viewer);
        this.sendViewersCount();
      },
    });
    return new Response(stream, {
      headers: {
        "content-type": "text/event-stream",
        "cache-control": "no-store",
        connection: "keep-alive",
      },
    });
  }

  private async forwardPrompt(request: Request): Promise<Response> {
    let body: { text?: unknown };
    try {
      body = (await request.json()) as { text?: unknown };
    } catch {
      return json({ ok: false, error: "invalid_json" }, 400);
    }
    const text = typeof body.text === "string" ? body.text.trim() : "";
    if (!text) {
      return json({ ok: false, error: "empty_prompt" }, 400);
    }
    if (text.length > 64_000) {
      return json({ ok: false, error: "prompt_too_long" }, 413);
    }
    return this.forward({ type: "prompt", text });
  }

  private async forwardResolve(request: Request): Promise<Response> {
    // First-class remote approval (owner decision 2026-06-11): the capability URL
    // grants the same confirmation power as the local TUI.
    let body: { approve?: unknown };
    try {
      body = (await request.json()) as { approve?: unknown };
    } catch {
      return json({ ok: false, error: "invalid_json" }, 400);
    }
    if (typeof body.approve !== "boolean") {
      return json({ ok: false, error: "approve_must_be_boolean" }, 400);
    }
    return this.forward({ type: "control_plane_resolve", approve: body.approve });
  }

  private async forwardSetModel(request: Request): Promise<Response> {
    let body: { model?: unknown };
    try {
      body = (await request.json()) as { model?: unknown };
    } catch {
      return json({ ok: false, error: "invalid_json" }, 400);
    }
    const spec = validateSetModel(body.model);
    if (!spec) {
      return json({ ok: false, error: "invalid_model" }, 400);
    }
    return this.forward({ type: "set_model", model: spec });
  }

  private forward(frame: { type: string; [k: string]: unknown }): Response {
    if (!this.agentSocket) {
      return json({ ok: false, error: "agent_offline" }, 503);
    }
    try {
      this.agentSocket.send(JSON.stringify(frame));
    } catch {
      return json({ ok: false, error: "agent_offline" }, 503);
    }
    return json({ ok: true, accepted: true });
  }

  private sendViewersCount(): void {
    if (!this.agentSocket) return;
    try {
      this.agentSocket.send(JSON.stringify({ type: "viewers", count: this.viewers.size }));
    } catch {
      // agent socket raced shut; close handler clears it
    }
  }

  private broadcastStatus(agentOnline: boolean): void {
    this.broadcastSse("relay_status", JSON.stringify({ agent_online: agentOnline }));
  }

  private broadcastSse(event: string, data: string): void {
    for (const viewer of [...this.viewers]) {
      this.sendSse(viewer, event, data);
    }
  }

  private sendSse(viewer: SseViewer, event: string, data: string): void {
    try {
      viewer.controller.enqueue(this.encoder.encode(`event: ${event}\ndata: ${data}\n\n`));
    } catch {
      this.viewers.delete(viewer);
    }
  }

  private closeAll(): void {
    for (const viewer of [...this.viewers]) {
      try {
        viewer.controller.close();
      } catch {
        // already closed
      }
    }
    this.viewers.clear();
    if (this.agentSocket) {
      try {
        this.agentSocket.close(1000, "shutdown");
      } catch {
        // already gone
      }
      this.agentSocket = null;
    }
  }
}

// The generated module is produced by scripts/embed-html.mjs before build/dev/deploy.
// Imported via an indirection object so node tests that never touch the viewer page
// don't need the generated file to exist at import time of this module's pure parts.
import { VIEWER_HTML } from "./viewer_html.generated.js";
const VIEWER_HTML_REF = { html: VIEWER_HTML };

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", "cache-control": "no-store" },
  });
}
