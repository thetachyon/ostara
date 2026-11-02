// Vercel serverless proxy: forwards every /api/* request to the Rust backend.
//
// Why a function instead of a vercel.json rewrite: rewrite destinations are
// static — they cannot read environment variables at request time. This
// function reads BACKEND_URL at runtime, so the frontend keeps calling
// same-origin /api/... with zero JS changes and you only ever set one env var.
//
// Set BACKEND_URL in the Vercel project settings, e.g.
//   https://ostara-api.fly.dev
// (no trailing slash needed; one is tolerated).
//
// NOTE on the SSE intel stream (/api/intel/stream): serverless functions have
// a maximum execution duration (tens of seconds on Hobby). The infinite event
// stream will be cut when the function times out and the browser's
// EventSource will reconnect — the ticker keeps working, with periodic
// reconnects. For a persistent stream, point the browser at the backend URL
// directly or use a plan with longer function durations.

export default async function handler(req, res) {
  const backend = (process.env.BACKEND_URL || "").replace(/\/+$/, "");
  if (!backend) {
    res.status(500).json({
      error: "misconfigured",
      detail:
        "BACKEND_URL is not set on this Vercel project. Set it to the backend's public URL (e.g. https://ostara-api.fly.dev) and redeploy.",
    });
    return;
  }

  // req.url is the original path+query, e.g. /api/prices?symbols=AAPL
  const target = backend + req.url;

  const headers = { ...req.headers };
  delete headers.host;
  delete headers.connection;
  delete headers["content-length"];

  const init = { method: req.method, headers };
  if (req.method !== "GET" && req.method !== "HEAD") {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = Buffer.concat(chunks);
    if (body.length > 0) init.body = body;
  }

  let upstream;
  try {
    upstream = await fetch(target, init);
  } catch (err) {
    res.status(502).json({
      error: "bad_gateway",
      detail: `could not reach the backend at ${backend}: ${err.message}`,
    });
    return;
  }

  res.status(upstream.status);
  upstream.headers.forEach((value, key) => {
    const k = key.toLowerCase();
    // Let the serverless runtime manage framing itself.
    if (k === "content-encoding" || k === "transfer-encoding" || k === "connection") return;
    res.setHeader(key, value);
  });
  if (res.flushHeaders) res.flushHeaders();

  if (upstream.body) {
    for await (const chunk of upstream.body) {
      if (!res.write(chunk)) {
        // Backpressure: wait for the socket to drain before reading more.
        await new Promise((resolve) => res.once("drain", resolve));
      }
    }
  }
  res.end();
}
