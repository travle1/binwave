const MAX_BODY_BYTES = 4_096;

function json(body, status = 200) {
  return Response.json(body, {
    status,
    headers: { "Cache-Control": "no-store" },
  });
}

function text(value, maxLength) {
  return typeof value === "string" ? value.trim().slice(0, maxLength) : "";
}

async function hasValidTurnstileToken(request, input, env) {
  const token = text(input.turnstileToken, 2_048);
  if (!token || typeof env.TURNSTILE_VERIFY?.fetch !== "function") return false;

  try {
    const response = await env.TURNSTILE_VERIFY.fetch("https://verification.internal/", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        token,
        remoteip: request.headers.get("CF-Connecting-IP") || undefined,
      }),
    });
    const result = response.ok ? await response.json() : null;
    return result && result.success === true;
  } catch (error) {
    console.error(JSON.stringify({ event: "binwave_turnstile_verification_failed", message: String(error) }));
    return false;
  }
}

export async function onRequestPost({ request, env }) {
  try {
    const contentLength = Number(request.headers.get("content-length") || 0);
    if (contentLength > MAX_BODY_BYTES) return json({ error: "Request is too large." }, 413);

    const input = await request.json();
    if (text(input.company, 200)) return json({ ok: true }, 201);
    if (!(await hasValidTurnstileToken(request, input, env))) {
      return json({ error: "Please complete the security check and try again." }, 400);
    }

    const email = text(input.email, 254).toLowerCase();
    const context = text(input.context, 1_000);
    if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
      return json({ error: "Please provide a valid email address." }, 400);
    }

    await env.DB.prepare(
      "INSERT INTO binwave_early_access_signups (id, email, context, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(email) DO NOTHING"
    ).bind(crypto.randomUUID(), email, context, new Date().toISOString()).run();

    return json({ ok: true }, 201);
  } catch (error) {
    console.error(JSON.stringify({ event: "binwave_early_access_failed", message: String(error) }));
    return json({ error: "We could not save your request. Please try again shortly." }, 500);
  }
}
