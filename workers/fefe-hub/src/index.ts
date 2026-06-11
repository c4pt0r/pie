import { isValidToken, parseSessionPath } from "./relay.js";

export { RelayCore, SessionRelay, isValidToken, parseSessionPath } from "./relay.js";

const PROTOCOL_VERSION = "2025-03-26";
const DEFAULT_VERSION = "0.1.0";
const WEB_SESSION_COOKIE = "hub_session";

interface Env {
  HUB_VERSION?: string;
  SESSION_RELAY?: DurableObjectNamespace;
}

type DurableObjectState = unknown;

const REMOVED_BODY = {
  ok: false,
  error: "hub_removed",
  message: "pie hub has been removed from this build.",
};

const REMOVED_PATHS = new Set([
  "/auth/start",
  "/auth/exchange_code",
  "/auth/exchange_manual_code",
  "/auth/register",
  "/auth/login",
  "/login",
  "/chat",
  "/chat/login",
  "/chat/send",
  "/mcp",
]);

const CHAT_PATHS = new Set(["/chat", "/chat/login", "/chat/send"]);

export class HubApp {
  constructor(
    private readonly version = DEFAULT_VERSION,
    private readonly relay?: DurableObjectNamespace,
  ) {}

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return json({
        ok: true,
        service: "pie-hub",
        status: "disabled",
        relay: this.relay ? "enabled" : "unconfigured",
        version: this.version,
        protocol_version: PROTOCOL_VERSION,
      });
    }

    // Session relay (issue #22) — the only live surface; legacy hub paths stay 410.
    if (url.pathname === "/relay/agent") {
      const token = url.searchParams.get("token") ?? "";
      if (!isValidToken(token)) {
        return json({ ok: false, error: "invalid_token" }, 400);
      }
      return this.relayStub(token, request, "/agent");
    }
    const session = parseSessionPath(url.pathname);
    if (session) {
      if (session.rest === "") {
        // The viewer HTML uses relative fetch paths; they only resolve under a
        // trailing-slash URL.
        return Response.redirect(`${url.origin}/session/${session.token}/`, 301);
      }
      const inner = session.rest === "/" ? "/" : session.rest;
      return this.relayStub(session.token, request, inner);
    }
    if (url.pathname.startsWith("/session/")) {
      return json({ ok: false, error: "invalid_token" }, 400);
    }

    if (REMOVED_PATHS.has(url.pathname)) {
      return removedResponse(url.pathname);
    }

    return json({ ok: false, error: "not_found" }, 404);
  }

  private relayStub(token: string, request: Request, innerPath: string): Promise<Response> {
    if (!this.relay) {
      return Promise.resolve(json({ ok: false, error: "relay_unconfigured" }, 503));
    }
    const stub = this.relay.get(this.relay.idFromName(token));
    const inner = new URL(request.url);
    inner.pathname = innerPath;
    return stub.fetch(new Request(inner.toString(), request));
  }
}

export class AgentMailbox {
  constructor(
    _state?: DurableObjectState,
    _env?: Env,
  ) {}

  fetch(): Response {
    return removedResponse("/mcp");
  }
}

export function createTestApp(
  version = DEFAULT_VERSION,
  relay?: DurableObjectNamespace,
): HubApp {
  return new HubApp(version, relay);
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return createApp(env).fetch(request);
  },
};

function createApp(env: Env): HubApp {
  return createTestApp(env.HUB_VERSION ?? DEFAULT_VERSION, env.SESSION_RELAY);
}

function removedResponse(pathname: string): Response {
  const response = json(REMOVED_BODY, 410);
  if (CHAT_PATHS.has(pathname)) {
    response.headers.append("set-cookie", clearWebSessionCookie());
  }
  return response;
}

function clearWebSessionCookie(): string {
  return `${WEB_SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/chat; Max-Age=0`;
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}
