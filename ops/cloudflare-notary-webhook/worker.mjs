export const MAX_BODY_BYTES = 65_536;

function configured(env) {
  return (
    typeof env.WEBHOOK_SECRET === "string" &&
    env.WEBHOOK_SECRET.length >= 32 &&
    typeof env.GITHUB_TOKEN === "string" &&
    env.GITHUB_TOKEN.length > 0 &&
    typeof env.GITHUB_REPOSITORY === "string" &&
    /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(env.GITHUB_REPOSITORY)
  );
}

async function bodyFits(request) {
  const declared = request.headers.get("content-length");
  if (declared !== null) {
    const length = Number(declared);
    if (!Number.isSafeInteger(length) || length < 0 || length > MAX_BODY_BYTES) {
      return false;
    }
  }

  if (request.body === null) {
    return true;
  }

  const reader = request.body.getReader();
  let total = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        return true;
      }
      total += value.byteLength;
      if (total > MAX_BODY_BYTES) {
        await reader.cancel();
        return false;
      }
    }
  } finally {
    reader.releaseLock();
  }
}

export async function handleRequest(request, env, fetchImpl = fetch) {
  if (!configured(env)) {
    return new Response("Worker configuration error\n", { status: 500 });
  }

  if (request.method !== "POST") {
    return new Response("Method not allowed\n", {
      status: 405,
      headers: { Allow: "POST" },
    });
  }

  const expectedPath = `/apple-notary/${env.WEBHOOK_SECRET}`;
  if (new URL(request.url).pathname !== expectedPath) {
    return new Response("Not found\n", { status: 404 });
  }

  if (!(await bodyFits(request))) {
    return new Response("Payload too large\n", { status: 413 });
  }

  const payload = {
    event_type: "apple-notarization-complete",
    client_payload: {
      source: "apple-notary-webhook",
      received_at: new Date().toISOString(),
    },
  };

  let response;
  try {
    response = await fetchImpl(
      `https://api.github.com/repos/${env.GITHUB_REPOSITORY}/dispatches`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${env.GITHUB_TOKEN}`,
          Accept: "application/vnd.github+json",
          "Content-Type": "application/json",
          "User-Agent": "codexify-notary-webhook",
          "X-GitHub-Api-Version": "2022-11-28",
        },
        body: JSON.stringify(payload),
      },
    );
  } catch {
    return new Response("GitHub dispatch failed\n", { status: 502 });
  }

  if (response.status !== 204) {
    return new Response("GitHub dispatch failed\n", { status: 502 });
  }

  return new Response("Accepted\n", { status: 202 });
}

export default {
  fetch(request, env) {
    return handleRequest(request, env);
  },
};
