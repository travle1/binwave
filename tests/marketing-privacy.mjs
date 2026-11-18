import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { onRequestPost } from "../marketing/functions/api/early-access.js";

function context(options = {}) {
  const writes = [];
  const calls = [];
  const env = {
    TURNSTILE_VERIFY: {
      async fetch(url, init) {
        calls.push({ url, payload: JSON.parse(init.body) });
        if (options.fail) throw new Error("unavailable verifier");
        return Response.json({ success: options.success !== false });
      },
    },
    DB: { prepare() { return { bind(...values) { return { async run() { writes.push(values); return { meta: { changes: 0 } }; } }; } }; } },
  };
  if (options.missing) delete env.TURNSTILE_VERIFY;
  const request = new Request("https://example.com/api/early-access", {
    method: "POST", headers: { "content-type": "application/json", "CF-Connecting-IP": "192.0.2.1" },
    body: JSON.stringify({ email: "person@example.com", turnstileToken: "synthetic-token", ...(options.input || {}) }),
  });
  return { request, env, writes, calls };
}

test("a service binding verifies the request without a public account hostname", async () => {
  const c = context(); const response = await onRequestPost(c);
  assert.equal(response.status, 201); assert.equal(c.writes.length, 1);
  assert.deepEqual(c.calls[0], { url: "https://verification.internal/", payload: { token: "synthetic-token", remoteip: "192.0.2.1" } });
});

test("missing, failed and rejected verification cannot store a signup", async () => {
  for (const options of [{ missing: true }, { fail: true }, { success: false }, { input: { turnstileToken: "" } }]) {
    const c = context(options); const response = await onRequestPost(c);
    assert.equal(response.status, 400); assert.equal(c.writes.length, 0);
  }
});

test("the honeypot does not call either service", async () => {
  const c = context({ input: { company: "automated entry" } });
  assert.equal((await onRequestPost(c)).status, 201); assert.equal(c.calls.length, 0); assert.equal(c.writes.length, 0);
});

test("deployment configuration binds the verifier", async () => {
  const text = await readFile(new URL("../marketing/wrangler.jsonc", import.meta.url), "utf8");
  const config = JSON.parse(text.replace(/^\s*\/\/.*$/gm, ""));
  assert.deepEqual(config.services, [{ binding: "TURNSTILE_VERIFY", service: "turnstile-siteverify-buildincharge" }]);
  const source = await readFile(new URL("../marketing/functions/api/early-access.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /https:\/\/[^\s"']+\.workers\.dev/);
});
