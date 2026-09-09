import assert from "node:assert/strict";
import test from "node:test";
import { pathToFileURL } from "node:url";
import { fileURLToPath } from "node:url";
import path from "node:path";
import fs from "node:fs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const workerPath = path.join(root, "ops", "cloudflare-notary-webhook", "worker.mjs");

async function loadWorker() {
  assert.equal(fs.existsSync(workerPath), true, "Cloudflare Worker module must exist");
  return import(`${pathToFileURL(workerPath).href}?test=${Date.now()}-${Math.random()}`);
}

function environment() {
  return {
    WEBHOOK_SECRET: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    GITHUB_TOKEN: "github-token-placeholder",
    GITHUB_REPOSITORY: "devnoname120/codexify",
  };
}

function request(secret, options = {}) {
  return new Request(`https://worker.example/apple-notary/${secret}`, {
    method: options.method ?? "POST",
    body: options.body,
    headers: options.headers,
  });
}

test("rejects methods other than POST without calling GitHub", async () => {
  const { handleRequest } = await loadWorker();
  let calls = 0;
  const response = await handleRequest(
    request(environment().WEBHOOK_SECRET, { method: "GET" }),
    environment(),
    async () => {
      calls += 1;
      return new Response(null, { status: 204 });
    },
  );
  assert.equal(response.status, 405);
  assert.equal(response.headers.get("allow"), "POST");
  assert.equal(calls, 0);
});

test("rejects a wrong secret path", async () => {
  const { handleRequest } = await loadWorker();
  let calls = 0;
  const response = await handleRequest(
    request("wrong-secret"),
    environment(),
    async () => {
      calls += 1;
      return new Response(null, { status: 204 });
    },
  );
  assert.equal(response.status, 404);
  assert.equal(calls, 0);
});

test("rejects callback bodies above the bound", async () => {
  const { handleRequest, MAX_BODY_BYTES } = await loadWorker();
  let calls = 0;
  const response = await handleRequest(
    request(environment().WEBHOOK_SECRET, { body: "x".repeat(MAX_BODY_BYTES + 1) }),
    environment(),
    async () => {
      calls += 1;
      return new Response(null, { status: 204 });
    },
  );
  assert.equal(response.status, 413);
  assert.equal(calls, 0);
});

test("translates a valid Apple wake-up into one repository dispatch", async () => {
  const { handleRequest } = await loadWorker();
  const calls = [];
  const response = await handleRequest(
    request(environment().WEBHOOK_SECRET, { body: JSON.stringify({ ignored: true }) }),
    environment(),
    async (url, init) => {
      calls.push({ url, init });
      return new Response(null, { status: 204 });
    },
  );
  assert.equal(response.status, 202);
  assert.equal(calls.length, 1);
  assert.equal(
    calls[0].url,
    "https://api.github.com/repos/devnoname120/codexify/dispatches",
  );
  assert.equal(calls[0].init.method, "POST");
  assert.equal(calls[0].init.headers.Authorization, "Bearer github-token-placeholder");
  assert.equal(calls[0].init.headers["X-GitHub-Api-Version"], "2022-11-28");
  const payload = JSON.parse(calls[0].init.body);
  assert.equal(payload.event_type, "apple-notarization-complete");
  assert.equal(payload.client_payload.source, "apple-notary-webhook");
  assert.equal(typeof payload.client_payload.received_at, "string");
  assert.equal("ignored" in payload.client_payload, false);
});

test("returns a generic gateway failure without exposing GitHub response data", async () => {
  const { handleRequest } = await loadWorker();
  const response = await handleRequest(
    request(environment().WEBHOOK_SECRET),
    environment(),
    async () => new Response("upstream-secret-detail", { status: 500 }),
  );
  assert.equal(response.status, 502);
  assert.equal((await response.text()).includes("upstream-secret-detail"), false);
});

test("fails closed when required Worker settings are absent", async () => {
  const { handleRequest } = await loadWorker();
  const response = await handleRequest(
    new Request("https://worker.example/apple-notary/value", {
      method: "POST",
      body: "{}",
    }),
    {},
    async () => new Response(null, { status: 204 }),
  );
  assert.equal(response.status, 500);
});
