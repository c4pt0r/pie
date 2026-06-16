import assert from "node:assert/strict";
import test from "node:test";

import {
  RelayCore,
  createTestApp,
  isValidToken,
  parseSessionPath,
  validateSetModel,
} from "../dist/index.js";

const TOKEN = "a".repeat(40);
const BASE = "https://pie.0xfefe.me";

function helloFrame(key = "agent-key-1") {
  return JSON.stringify({ type: "hello", agent_key: key });
}

test("relay core pins the agent key on first hello and rejects mismatches", () => {
  const core = new RelayCore();
  core.agentConnected();
  assert.deepEqual(core.handleAgentMessage(helloFrame("k1")), { kind: "accept" });

  // Reconnect with the same key: accepted.
  core.agentConnected();
  assert.deepEqual(core.handleAgentMessage(helloFrame("k1")), { kind: "accept" });

  // Reconnect with a different key: rejected — a view-token holder can't take over.
  core.agentConnected();
  assert.deepEqual(core.handleAgentMessage(helloFrame("k2")), {
    kind: "reject",
    reason: "bad_key",
  });
});

test("relay core requires hello before any other frame", () => {
  const core = new RelayCore();
  core.agentConnected();
  const effect = core.handleAgentMessage(JSON.stringify({ type: "snapshot", data: {} }));
  assert.deepEqual(effect, { kind: "reject", reason: "not_hello" });
});

test("relay core stores and broadcasts snapshots, then forgets on shutdown", () => {
  const core = new RelayCore();
  core.agentConnected();
  core.handleAgentMessage(helloFrame());

  const effect = core.handleAgentMessage(
    JSON.stringify({ type: "snapshot", data: { session_id: "s1" } }),
  );
  assert.equal(effect.kind, "broadcast");
  assert.match(effect.snapshot, /session_id/);
  assert.equal(core.latestSnapshot, effect.snapshot);

  assert.deepEqual(core.handleAgentMessage(JSON.stringify({ type: "shutdown" })), {
    kind: "shutdown",
  });
  assert.equal(core.latestSnapshot, null, "shutdown must purge the transcript snapshot");
  assert.equal(core.closed, true);
});

test("relay core rejects oversized and malformed frames", () => {
  const core = new RelayCore();
  core.agentConnected();
  core.handleAgentMessage(helloFrame());
  assert.deepEqual(core.handleAgentMessage("not json"), {
    kind: "reject",
    reason: "bad_frame",
  });
  const oversized = JSON.stringify({ type: "snapshot", data: "x".repeat(1_300_000) });
  assert.deepEqual(core.handleAgentMessage(oversized), {
    kind: "reject",
    reason: "oversized",
  });
});

test("session path parsing accepts hex tokens and rejects junk", () => {
  assert.deepEqual(parseSessionPath(`/session/${TOKEN}`), { token: TOKEN, rest: "" });
  assert.deepEqual(parseSessionPath(`/session/${TOKEN}/state`), {
    token: TOKEN,
    rest: "/state",
  });
  assert.equal(parseSessionPath("/session/UPPER"), null);
  assert.equal(parseSessionPath("/session/../etc"), null);
  assert.equal(parseSessionPath("/elsewhere"), null);
  assert.ok(isValidToken(TOKEN));
  assert.ok(!isValidToken("short"));
});

function fakeRelayNamespace(log) {
  return {
    idFromName(name) {
      return { toString: () => name };
    },
    get(id) {
      return {
        async fetch(request) {
          const url = new URL(request.url);
          log.push({ token: id.toString(), path: url.pathname });
          return new Response(JSON.stringify({ forwarded: url.pathname }), {
            headers: { "content-type": "application/json" },
          });
        },
      };
    },
  };
}

test("router redirects bare session URLs to trailing slash", async () => {
  const app = createTestApp("v", fakeRelayNamespace([]));
  const response = await app.fetch(new Request(`${BASE}/session/${TOKEN}`));
  assert.equal(response.status, 301);
  assert.equal(response.headers.get("location"), `${BASE}/session/${TOKEN}/`);
});

test("router forwards session subpaths to the durable object for the token", async () => {
  const log = [];
  const app = createTestApp("v", fakeRelayNamespace(log));
  for (const rest of ["/", "/state", "/events", "/prompt", "/model"]) {
    await app.fetch(new Request(`${BASE}/session/${TOKEN}${rest}`));
  }
  assert.deepEqual(
    log.map((entry) => entry.path),
    ["/", "/state", "/events", "/prompt", "/model"],
  );
  assert.ok(log.every((entry) => entry.token === TOKEN));
});

test("router rejects invalid tokens without touching durable objects", async () => {
  const log = [];
  const app = createTestApp("v", fakeRelayNamespace(log));
  const response = await app.fetch(new Request(`${BASE}/session/NOPE/state`));
  assert.equal(response.status, 400);
  assert.equal(log.length, 0);

  const agent = await app.fetch(new Request(`${BASE}/relay/agent?token=NOPE`));
  assert.equal(agent.status, 400);
});

test("relay agent route forwards to the durable object as /agent", async () => {
  const log = [];
  const app = createTestApp("v", fakeRelayNamespace(log));
  await app.fetch(new Request(`${BASE}/relay/agent?token=${TOKEN}`));
  assert.deepEqual(log, [{ token: TOKEN, path: "/agent" }]);
});

test("control-plane resolve validates input and reports agent_offline without a socket", async () => {
  const { SessionRelay } = await import("../dist/index.js");
  const relay = new SessionRelay();
  const bad = await relay.fetch(
    new Request(`${BASE}/control-plane/resolve`, { method: "POST", body: "not json" }),
  );
  assert.equal(bad.status, 400);
  const notBool = await relay.fetch(
    new Request(`${BASE}/control-plane/resolve`, {
      method: "POST",
      body: JSON.stringify({ approve: "yes" }),
    }),
  );
  assert.equal(notBool.status, 400);
  // Valid approval with no live agent socket: forwarded path reports offline, not 403 —
  // remote approval is first-class now.
  const offline = await relay.fetch(
    new Request(`${BASE}/control-plane/resolve`, {
      method: "POST",
      body: JSON.stringify({ approve: true }),
    }),
  );
  assert.equal(offline.status, 503);
  const body = await offline.json();
  assert.equal(body.error, "agent_offline");
});

test("set_model body validation accepts specs and rejects junk", () => {
  assert.equal(validateSetModel("anthropic:claude-haiku-4-5"), "anthropic:claude-haiku-4-5");
  assert.equal(validateSetModel("  ds4:deepseek-v4-flash  "), "ds4:deepseek-v4-flash");
  assert.equal(validateSetModel(""), null);
  assert.equal(validateSetModel(42), null);
  assert.equal(validateSetModel("x".repeat(300)), null);
});

test("legacy hub paths still return 410 with the relay enabled", async () => {
  const app = createTestApp("v", fakeRelayNamespace([]));
  for (const path of ["/auth/start", "/chat", "/mcp", "/login"]) {
    const response = await app.fetch(new Request(`${BASE}${path}`, { method: "POST" }));
    assert.equal(response.status, 410, path);
  }
});

test("health reports the relay as enabled when configured", async () => {
  const app = createTestApp("v", fakeRelayNamespace([]));
  const body = await (await app.fetch(new Request(`${BASE}/health`))).json();
  assert.equal(body.relay, "enabled");
  assert.equal(body.status, "disabled");
});
