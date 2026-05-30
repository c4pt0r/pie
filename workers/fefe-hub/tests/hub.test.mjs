import assert from "node:assert/strict";
import test from "node:test";

import { createTestApp, MemoryStore } from "../dist/index.js";

const BASE = "https://hub.test";

test("health reports protocol and version", async () => {
  const app = createTestApp();
  const response = await app.fetch(new Request(`${BASE}/health`));
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.ok, true);
  assert.equal(body.protocol_version, "2025-03-26");
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
  assert.equal(data.params.first_contact_required, true);
  assert.equal("payload" in data.params, false);

  const backlog = await store.listNotifications(receiver.agent.agent_id, 10, null);
  assert.equal(backlog.length, 1);
  assert.equal(backlog[0].payload_visibility, "Local");
  assert.equal(backlog[0].payload_json, null);
  assert.doesNotMatch(JSON.stringify(backlog), /kept-local/);
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
