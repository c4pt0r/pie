import assert from "node:assert/strict";
import test from "node:test";

import { AgentMailbox, createTestApp } from "../dist/index.js";

const BASE = "https://hub.test";
const FORBIDDEN = /hub_agent_|hub_hs_|hub_code_|code_verifier|Authorization|Bearer|raw MCP|raw Local|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;

test("health reports disabled tombstone metadata", async () => {
  const app = createTestApp("test-version");
  const response = await app.fetch(new Request(`${BASE}/health`));

  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "application/json; charset=utf-8");
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), {
    ok: true,
    service: "pie-hub",
    status: "disabled",
    version: "test-version",
    protocol_version: "2025-03-26",
  });
});

test("old hub entrypoints return bounded 410 tombstone responses", async () => {
  const app = createTestApp();
  const cases = [
    { method: "POST", path: "/auth/start" },
    { method: "POST", path: "/auth/exchange_code" },
    { method: "POST", path: "/auth/exchange_manual_code" },
    { method: "POST", path: "/auth/register" },
    { method: "POST", path: "/auth/login" },
    { method: "GET", path: "/login" },
    { method: "POST", path: "/login" },
    { method: "GET", path: "/chat", clearsCookie: true },
    { method: "POST", path: "/chat/login", clearsCookie: true },
    { method: "POST", path: "/chat/send", clearsCookie: true },
    { method: "GET", path: "/mcp", headers: { accept: "text/event-stream", authorization: "Bearer hub_agent_should_not_echo" } },
    { method: "POST", path: "/mcp", headers: { authorization: "Bearer hub_agent_should_not_echo" } },
  ];

  for (const testCase of cases) {
    const response = await app.fetch(
      new Request(`${BASE}${testCase.path}?code=hub_code_should_not_echo&state=state_should_not_echo`, {
        method: testCase.method,
        headers: testCase.headers,
        body: ["GET", "HEAD"].includes(testCase.method)
          ? undefined
          : JSON.stringify({
              code_verifier: "verifier_should_not_echo",
              payload: "raw Local payload should not echo",
              mcp: "raw MCP should not echo",
            }),
      }),
    );

    assert.equal(response.status, 410, `${testCase.method} ${testCase.path}`);
    assert.equal(response.headers.get("content-type"), "application/json; charset=utf-8");
    assert.notEqual(response.headers.get("content-type"), "text/event-stream; charset=utf-8");
    assert.equal(response.headers.get("cache-control"), "no-store");

    const setCookie = response.headers.get("set-cookie") ?? "";
    if (testCase.clearsCookie) {
      assert.match(setCookie, /^hub_session=;/);
      assert.match(setCookie, /Path=\/chat/);
      assert.match(setCookie, /Max-Age=0/);
    } else {
      assert.equal(setCookie, "");
    }

    const text = await response.text();
    assert.deepEqual(JSON.parse(text), {
      ok: false,
      error: "hub_removed",
      message: "pie hub has been removed from this build.",
    });
    assert.doesNotMatch(text, FORBIDDEN, `${testCase.method} ${testCase.path}`);
    assert.doesNotMatch(setCookie, /hub_hs_[A-Za-z0-9_-]+/);
  }
});

test("mcp durable object tombstone does not open an event stream", async () => {
  const mailbox = new AgentMailbox();
  const response = await mailbox.fetch(new Request(`${BASE}/mcp`, { headers: { accept: "text/event-stream" } }));

  assert.equal(response.status, 410);
  assert.notEqual(response.headers.get("content-type"), "text/event-stream; charset=utf-8");
  assert.deepEqual(await response.json(), {
    ok: false,
    error: "hub_removed",
    message: "pie hub has been removed from this build.",
  });
});

test("unknown routes remain bounded not found", async () => {
  const app = createTestApp();
  const response = await app.fetch(new Request(`${BASE}/not-hub?token=hub_agent_should_not_echo`));

  assert.equal(response.status, 404);
  const text = await response.text();
  assert.deepEqual(JSON.parse(text), { ok: false, error: "not_found" });
  assert.doesNotMatch(text, FORBIDDEN);
});
