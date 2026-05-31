import assert from "node:assert/strict";
import test from "node:test";

import { AgentMailbox, createTestApp, MemoryStore } from "../dist/index.js";

const BASE = "https://hub.test";

test("health reports protocol and version", async () => {
  const app = createTestApp();
  const response = await app.fetch(new Request(`${BASE}/health`));
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.ok, true);
  assert.equal(body.protocol_version, "2025-03-26");
});

test("auth start plus exchange joins with one-time code and exact bounded shape", async () => {
  const app = createTestApp();
  const verifier = "v".repeat(64);
  const state = "state_join_contract_1";
  const loopback = "http://127.0.0.1:49152/callback";
  const start = await authStart(app, { verifier, state, loopback });

  assert.deepEqual(Object.keys(start).sort(), ["exchange_request_id", "expires_in_seconds", "login_url"]);
  assert.equal(start.expires_in_seconds, 300);
  assert.match(start.exchange_request_id, /^[0-9a-f-]{36}$/);
  assert.match(start.login_url, /^https:\/\/hub\.test\/login\?req=/);
  assert.doesNotMatch(JSON.stringify(start), /hub_agent_|hub_code_|127\.0\.0\.1|code_verifier/);

  const callback = await browserLogin(app, start.exchange_request_id, state, {
    mode: "register",
    username: "alice",
    namespace: "dongxu",
    password: "alice-password-123",
  });
  assert.equal(callback.origin + callback.pathname, loopback);
  assert.equal(callback.searchParams.get("state"), state);
  const code = callback.searchParams.get("code");
  assert.match(code, /^hub_code_/);

  const wrongVerifier = await exchangeCode(app, {
    exchange_request_id: start.exchange_request_id,
    code,
    state,
    code_verifier: "w".repeat(64),
  });
  assert.equal(wrongVerifier.status, 401);
  assert.doesNotMatch(JSON.stringify(await wrongVerifier.json()), /hub_agent_|hub_code_|state_join_contract_1|vvvv/);

  const exchange = await exchangeJson(app, {
    exchange_request_id: start.exchange_request_id,
    code,
    state,
    code_verifier: verifier,
  });
  assert.deepEqual(Object.keys(exchange).sort(), [
    "agent_id",
    "expires_at",
    "handle",
    "hub_token",
    "namespace",
    "profile",
    "visibility",
  ]);
  assert.match(exchange.agent_id, /^[0-9a-f-]{36}$/);
  assert.equal(exchange.handle, "alice");
  assert.equal(exchange.namespace, "dongxu");
  assert.match(exchange.hub_token, /^hub_agent_/);
  assert.equal(exchange.expires_at, null);
  assert.deepEqual(exchange.profile, { display_name: "alice", description: null, capabilities: [] });
  assert.deepEqual(exchange.visibility, { discoverable: "public", inbox: "namespace" });

  const reused = await exchangeCode(app, {
    exchange_request_id: start.exchange_request_id,
    code,
    state,
    code_verifier: verifier,
  });
  assert.equal(reused.status, 401);
  assert.doesNotMatch(JSON.stringify(await reused.json()), /hub_agent_|hub_code_/);
});

test("browser login page separates sign in from registration and explains namespace", async () => {
  const app = createTestApp();
  const start = await authStart(app, {
    verifier: "u".repeat(64),
    state: "state_login_page",
    loopback: "http://127.0.0.1:49160/callback",
  });

  const response = await app.fetch(new Request(start.login_url));

  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type"), /text\/html/);
  const body = await response.text();
  assert.match(body, /Join pie\.0xfefe\.me/);
  assert.match(body, /name@namespace/);
  assert.match(body, /name="mode" value="login"/);
  assert.match(body, /name="mode" value="register"/);
  assert.match(body, /name="namespace"/);
  assert.match(body, /autocomplete="current-password"/);
  assert.match(body, /autocomplete="new-password"/);
  assert.doesNotMatch(body, /hub_agent_|hub_hs_|hub_code_|code_verifier|Authorization|pie-hub:default/);
});

test("browser login form errors are bounded HTML and do not crash worker", async () => {
  const app = createTestApp();
  const start = await authStart(app, {
    verifier: "e".repeat(64),
    state: "state_form_error",
    loopback: "http://127.0.0.1:49161/callback",
  });

  const response = await browserLoginResponse(app, start.exchange_request_id, "state_form_error", {
    mode: "register",
    username: "formerror",
    namespace: "formerror",
    password: "short",
  });

  assert.equal(response.status, 400);
  assert.match(response.headers.get("content-type"), /text\/html/);
  const body = await response.text();
  assert.match(body, /Could not complete sign-in/);
  assert.match(body, /password must be at least 12 characters/);
  assert.doesNotMatch(body, /short|hub_agent_|hub_hs_|hub_code_|code_verifier/);
});

test("browser registration enforces unique namespaces with bounded HTML", async () => {
  const app = createTestApp();
  await registerUser(app, "takenname");
  const start = await authStart(app, {
    verifier: "n".repeat(64),
    state: "state_namespace_taken",
    loopback: "http://127.0.0.1:49164/callback",
  });

  const response = await browserLoginResponse(app, start.exchange_request_id, "state_namespace_taken", {
    mode: "register",
    username: "newperson",
    namespace: "takenname",
    password: "newperson-password-123",
  });

  assert.equal(response.status, 400);
  assert.match(response.headers.get("content-type"), /text\/html/);
  const body = await response.text();
  assert.match(body, /namespace already exists/);
  assert.match(body, /name@namespace/);
  assert.doesNotMatch(body, /newperson-password-123|hub_agent_|hub_hs_|hub_code_|code_verifier|Authorization/);
});

test("browser login without auth start shows bounded HTML error", async () => {
  const app = createTestApp();

  const response = await browserLoginResponse(app, "", "", {
    mode: "register",
    username: "directlogin",
    namespace: "directlogin",
    password: "directlogin-password-123",
  });

  assert.equal(response.status, 400);
  assert.match(response.headers.get("content-type"), /text\/html/);
  const body = await response.text();
  assert.match(body, /exchange_request_id must be a non-empty string/);
  assert.doesNotMatch(body, /directlogin-password-123|hub_agent_|hub_hs_|hub_code_|code_verifier/);
});

test("browser login form succeeds for existing user and redirects to loopback", async () => {
  const app = createTestApp();
  await registerUser(app, "browserlogin");
  const start = await authStart(app, {
    verifier: "l".repeat(64),
    state: "state_browser_login",
    loopback: "http://127.0.0.1:49162/callback",
  });

  const callback = await browserLogin(app, start.exchange_request_id, "state_browser_login", {
    mode: "login",
    username: "browserlogin",
    password: "browserlogin-password-123",
  });

  assert.equal(callback.origin + callback.pathname, "http://127.0.0.1:49162/callback");
  assert.equal(callback.searchParams.get("state"), "state_browser_login");
  assert.match(callback.searchParams.get("code"), /^hub_code_/);
}
);

test("browser login form wrong password returns bounded HTML error", async () => {
  const app = createTestApp();
  await registerUser(app, "browserwrong");
  const start = await authStart(app, {
    verifier: "p".repeat(64),
    state: "state_browser_wrong_password",
    loopback: "http://127.0.0.1:49163/callback",
  });

  const response = await browserLoginResponse(app, start.exchange_request_id, "state_browser_wrong_password", {
    mode: "login",
    username: "browserwrong",
    password: "wrong-password-123",
  });

  assert.equal(response.status, 401);
  assert.match(response.headers.get("content-type"), /text\/html/);
  const body = await response.text();
  assert.match(body, /Could not complete sign-in/);
  assert.match(body, /Invalid username or password/);
  assert.doesNotMatch(body, /wrong-password-123|browserwrong-password-123|hub_agent_|hub_hs_|hub_code_|code_verifier/);
});

test("auth exchange rejects swapped state before issuing a token", async () => {
  const app = createTestApp();
  const verifierA = "a".repeat(64);
  const verifierB = "b".repeat(64);
  const startA = await authStart(app, {
    verifier: verifierA,
    state: "state_swap_a",
    loopback: "http://127.0.0.1:49153/callback",
  });
  const startB = await authStart(app, {
    verifier: verifierB,
    state: "state_swap_b",
    loopback: "http://127.0.0.1:49154/callback",
  });
  const callbackA = await browserLogin(app, startA.exchange_request_id, "state_swap_a", {
    mode: "register",
    username: "statea",
    password: "statea-password-123",
  });
  const codeA = callbackA.searchParams.get("code");

  const swapped = await exchangeCode(app, {
    exchange_request_id: startA.exchange_request_id,
    code: codeA,
    state: "state_swap_b",
    code_verifier: verifierA,
  });
  assert.equal(swapped.status, 401);
  assert.doesNotMatch(JSON.stringify(await swapped.json()), /hub_agent_|hub_code_|state_swap_a|state_swap_b/);

  const callbackB = await browserLogin(app, startB.exchange_request_id, "state_swap_b", {
    mode: "register",
    username: "stateb",
    password: "stateb-password-123",
  });
  const exchangeB = await exchangeJson(app, {
    exchange_request_id: startB.exchange_request_id,
    code: callbackB.searchParams.get("code"),
    state: "state_swap_b",
    code_verifier: verifierB,
  });
  assert.match(exchangeB.hub_token, /^hub_agent_/);
});

test("auth exchange atomically consumes one-time code under replay race", async () => {
  const app = createTestApp();
  const verifier = "r".repeat(64);
  const state = "state_replay_race";
  const start = await authStart(app, {
    verifier,
    state,
    loopback: "http://127.0.0.1:49155/callback",
  });
  const callback = await browserLogin(app, start.exchange_request_id, state, {
    mode: "register",
    username: "replay",
    password: "replay-password-123",
  });
  const code = callback.searchParams.get("code");

  const attempts = await Promise.all([
    exchangeCode(app, {
      exchange_request_id: start.exchange_request_id,
      code,
      state,
      code_verifier: verifier,
    }),
    exchangeCode(app, {
      exchange_request_id: start.exchange_request_id,
      code,
      state,
      code_verifier: verifier,
    }),
  ]);

  assert.equal(attempts.filter((response) => response.status === 200).length, 1);
  assert.equal(attempts.filter((response) => response.status === 401).length, 1);
  for (const response of attempts) {
    const body = await response.json();
    if (response.status === 200) {
      assert.match(body.hub_token, /^hub_agent_/);
    } else {
      assert.doesNotMatch(JSON.stringify(body), /hub_agent_|hub_code_|state_replay_race/);
    }
  }
});

test("registers users and agents without storing token material in list output", async () => {
  const app = createTestApp();
  const alice = await registerUser(app, "alice");
  const agent = await callTool(app, alice.session_token, "register_agent", {
    handle: "tools-lead",
    display_name: "Tools Lead",
    description: "MCP integration owner",
    capabilities: ["mcp", "worker"],
    discoverable: "public",
    inbox: "open",
  });

  assert.match(agent.hub_token, /^hub_agent_/);
  assert.match(agent.agent.agent_id, /^[0-9a-f-]{36}$/);

  const list = await callTool(app, alice.session_token, "list_my_agents", {});
  assert.equal(list.items.length, 1);
  assert.equal(list.items[0].handle, "tools-lead");
  assert.equal(JSON.stringify(list), JSON.stringify(list).replace(agent.hub_token, ""));
});

test("sends cross-namespace notification over SSE with canonical MCP metadata", async () => {
  const store = new MemoryStore();
  const app = createTestApp(store);
  const alice = await registerUser(app, "alice");
  const bob = await registerUser(app, "bob");
  const sender = await callTool(app, alice.session_token, "register_agent", {
    handle: "sender",
    display_name: "Sender",
    description: "Sends bounded notices",
    capabilities: ["notify"],
    discoverable: "public",
    inbox: "namespace",
  });
  const receiver = await callTool(app, bob.session_token, "register_agent", {
    handle: "receiver",
    display_name: "Receiver",
    description: "Receives bounded notices",
    capabilities: ["inbox"],
    discoverable: "public",
    inbox: "invited",
  });

  const streamResponse = await app.fetch(
    new Request(`${BASE}/mcp`, {
      headers: { authorization: `Bearer ${receiver.hub_token}`, accept: "text/event-stream" },
    }),
  );
  assert.equal(streamResponse.status, 200);
  assert.ok(streamResponse.body);
  const reader = streamResponse.body.getReader();
  await reader.read();

  const send = await callTool(app, sender.hub_token, "send_notification", {
    target_agent_id: receiver.agent.agent_id,
    summary: "Build finished",
    payload: { secret: "kept-local" },
  });
  assert.equal(send.first_contact_required, true);
  assert.match(send.notification_id, /^[0-9a-f-]{36}$/);

  const event = await readChunk(reader);
  assert.match(event, /^id: /m);
  assert.match(event, /notifications\/agent_message/);
  const data = JSON.parse(event.match(/^data: (.+)$/m)[1]);
  assert.equal(data.params.agent_id, sender.agent.agent_id);
  assert.equal(data.params.sender, "@sender@alice");
  assert.equal(data.params._meta.pie_dedup_key, send.notification_id);
  assert.equal(data.params._meta.pie_summary, "Build finished");
  assert.equal(data.params._meta.receiver_agent_id, receiver.agent.agent_id);
  assert.equal(data.params._meta.sender_agent_id, sender.agent.agent_id);
  assert.equal(data.params._meta.action_class, "notification");
  assert.equal(data.params.first_contact_required, true);
  assert.equal("payload" in data.params, false);

  const backlog = await store.listNotifications(receiver.agent.agent_id, 10, null);
  assert.equal(backlog.length, 1);
  assert.equal(backlog[0].payload_visibility, "Local");
  assert.equal(backlog[0].payload_json, null);
  assert.doesNotMatch(JSON.stringify(backlog), /kept-local/);
});

test("durable mailbox opens SSE before the heartbeat is drained", async () => {
  const mailbox = new AgentMailbox({ blockConcurrencyWhile: async (callback) => callback() });
  const response = await withTimeout(mailbox.fetch(new Request(`${BASE}/connect`)), 100);
  assert.equal(response.status, 200);
  assert.ok(response.body);

  const reader = response.body.getReader();
  const heartbeat = await withTimeout(reader.read(), 1000);
  assert.match(new TextDecoder().decode(heartbeat.value), /: connected/);

  const notification = {
    notification_id: crypto.randomUUID(),
    receiver_agent_id: "receiver-agent",
    sender_agent_id: "sender-agent",
    sender_handle: "sender",
    sender_namespace: "alice",
    summary: "Durable delivery",
    payload_json: null,
    payload_visibility: "Local",
    status: "pending",
    first_contact_required: 0,
    created_at: new Date().toISOString(),
    delivered_at: null,
    acked_at: null,
  };
  const push = await mailbox.fetch(
    new Request(`${BASE}/push`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ notification }),
    }),
  );
  assert.equal(push.status, 200);
  assert.deepEqual(await push.json(), { delivered: true });

  const event = await readChunk(reader);
  assert.match(event, /notifications\/agent_message/);
  await reader.cancel();
});

test("shared notification payload is explicit and remains bounded", async () => {
  const store = new MemoryStore();
  const app = createTestApp(store);
  const alice = await registerUser(app, "alice");
  const bob = await registerUser(app, "bob");
  const sender = await callTool(app, alice.session_token, "register_agent", {
    handle: "sender",
    display_name: "Sender",
    description: "Sends bounded notices",
    capabilities: ["notify"],
    discoverable: "public",
    inbox: "namespace",
  });
  const receiver = await callTool(app, bob.session_token, "register_agent", {
    handle: "receiver",
    display_name: "Receiver",
    description: "Receives bounded notices",
    capabilities: ["inbox"],
    discoverable: "public",
    inbox: "open",
  });

  await callTool(app, sender.hub_token, "send_notification", {
    target_agent_id: receiver.agent.agent_id,
    summary: "Shared detail",
    payload: { ticket: "P1" },
    payload_visibility: "Shared",
  });

  const backlog = await store.listNotifications(receiver.agent.agent_id, 10, null);
  assert.equal(backlog.length, 1);
  assert.equal(backlog[0].payload_visibility, "Shared");
  assert.deepEqual(JSON.parse(backlog[0].payload_json), { ticket: "P1" });
});

test("cross-namespace namespace-only inbox denies with bounded recovery error", async () => {
  const app = createTestApp();
  const alice = await registerUser(app, "alice");
  const bob = await registerUser(app, "bob");
  const sender = await callTool(app, alice.session_token, "register_agent", {
    handle: "sender",
    display_name: "Sender",
    description: "Sends notices",
    capabilities: ["notify"],
    inbox: "namespace",
  });
  const receiver = await callTool(app, bob.session_token, "register_agent", {
    handle: "receiver",
    display_name: "Receiver",
    description: "Receives notices",
    capabilities: ["inbox"],
    inbox: "namespace",
  });

  const response = await rpc(app, sender.hub_token, "tools/call", {
    name: "send_notification",
    arguments: {
      target_agent_id: receiver.agent.agent_id,
      summary: "Should not route",
    },
  });

  assert.equal(response.error.data.name, "permission_denied");
  assert.doesNotMatch(JSON.stringify(response), /hub_agent_|CF_API_KEY|Should not route/);
});

test("oversize summary is rejected before delivery", async () => {
  const app = createTestApp();
  const alice = await registerUser(app, "alice");
  const bob = await registerUser(app, "bob");
  const sender = await callTool(app, alice.session_token, "register_agent", {
    handle: "sender",
    display_name: "Sender",
    description: "Sends notices",
    capabilities: ["notify"],
    inbox: "namespace",
  });
  const receiver = await callTool(app, bob.session_token, "register_agent", {
    handle: "receiver",
    display_name: "Receiver",
    description: "Receives notices",
    capabilities: ["inbox"],
    inbox: "open",
  });

  const response = await rpc(app, sender.hub_token, "tools/call", {
    name: "send_notification",
    arguments: {
      target_agent_id: receiver.agent.agent_id,
      summary: "x".repeat(241),
    },
  });

  assert.equal(response.error.data.name, "schema_invalid");
  assert.match(response.error.data.violations[0], /summary/);
});

async function registerUser(app, username) {
  const response = await app.fetch(
    new Request(`${BASE}/auth/register`, {
      method: "POST",
      body: JSON.stringify({
        username,
        password: `${username}-password-123`,
      }),
    }),
  );
  assert.equal(response.status, 200);
  return response.json();
}

async function authStart(app, { verifier, state, loopback }) {
  const response = await app.fetch(
    new Request(`${BASE}/auth/start`, {
      method: "POST",
      body: JSON.stringify({
        client_kind: "pie-cli",
        client_version: "test",
        loopback_redirect_uri: loopback,
        code_challenge: await pkceChallenge(verifier),
        code_challenge_method: "S256",
        state,
      }),
    }),
  );
  assert.equal(response.status, 200);
  return response.json();
}

async function browserLogin(app, exchangeRequestId, state, { mode, username, namespace, password }) {
  const response = await browserLoginResponse(app, exchangeRequestId, state, { mode, username, namespace, password });
  assert.equal(response.status, 302);
  return new URL(response.headers.get("location"));
}

async function browserLoginResponse(app, exchangeRequestId, state, { mode, username, namespace, password }) {
  const form = new URLSearchParams({
    mode,
    username,
    password,
    exchange_request_id: exchangeRequestId,
    state,
  });
  if (namespace) {
    form.set("namespace", namespace);
  }
  const response = await app.fetch(
    new Request(`${BASE}/login`, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: form.toString(),
    }),
  );
  return response;
}

async function exchangeJson(app, body) {
  const response = await exchangeCode(app, body);
  assert.equal(response.status, 200);
  return response.json();
}

async function exchangeCode(app, body) {
  return app.fetch(
    new Request(`${BASE}/auth/exchange_code`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  );
}

async function pkceChallenge(verifier) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return Buffer.from(digest).toString("base64url");
}

async function callTool(app, token, name, args) {
  const response = await rpc(app, token, "tools/call", { name, arguments: args });
  assert.ifError(response.error);
  return JSON.parse(response.result.content[0].text);
}

async function rpc(app, token, method, params) {
  const response = await app.fetch(
    new Request(`${BASE}/mcp`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: crypto.randomUUID(), method, params }),
    }),
  );
  assert.equal(response.status, 200);
  return response.json();
}

async function readChunk(reader) {
  const decoder = new TextDecoder();
  const deadline = Date.now() + 1000;
  while (Date.now() < deadline) {
    const { value } = await reader.read();
    if (value) {
      const chunk = decoder.decode(value);
      if (chunk.includes("data: ")) {
        return chunk;
      }
    }
  }
  throw new Error("timed out waiting for SSE event");
}

async function withTimeout(promise, ms) {
  let timeout;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timeout = setTimeout(() => reject(new Error(`timed out after ${ms}ms`)), ms);
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
}
