#!/usr/bin/env node
/**
 * A tiny signed-webhook receiver for local development and CI (docs/01-VISION.md §13, P12).
 *
 * It accepts the deliveries Omnion posts, verifies the HMAC-SHA256 signature the platform signs
 * each body with (`X-Omnion-Signature: v1=<hex>` over `<timestamp>.<raw body>`, the timestamp
 * carried in `X-Omnion-Timestamp`), and keeps every delivery it saw so a walk can assert on it.
 * A delivery whose signature does not verify is answered with `401` and never counts as
 * accepted — the same thing a real receiver must do.
 *
 * Routes:
 *   POST /hooks/omnion  the delivery endpoint (point an Omnion webhook at this URL)
 *   GET  /received      the deliveries captured so far, as JSON
 *   GET  /healthz       readiness probe
 *
 * Usage: `node infra/mocks/webhook-receiver.mjs [port]` (default 8124, or `$PORT`).
 *   Signing secret: `$OMNION_WEBHOOK_SECRET` (default `omnion-dev-webhook-secret`).
 * No dependencies — this file runs on any machine that has Node.
 */

import { createHmac, timingSafeEqual } from "node:crypto";
import { createServer } from "node:http";

const port = Number(process.argv[2] ?? process.env.PORT ?? 8124);
const secret = process.env.OMNION_WEBHOOK_SECRET ?? "omnion-dev-webhook-secret";

/** Every delivery the receiver captured, newest last. */
const received = [];

/**
 * Verify a signature the way a receiver must: recompute the MAC over the bytes that arrived,
 * and compare in constant time.
 */
function verify(timestamp, body, header) {
  const prefix = "v1=";
  if (typeof header !== "string" || !header.startsWith(prefix)) {
    return false;
  }

  const expected = createHmac("sha256", secret)
    .update(`${timestamp}.`)
    .update(body)
    .digest();
  let provided;
  try {
    provided = Buffer.from(header.slice(prefix.length), "hex");
  } catch {
    return false;
  }

  return provided.length === expected.length && timingSafeEqual(provided, expected);
}

/** Answer with JSON. */
function send(response, status, payload) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(payload));
}

const server = createServer((request, response) => {
  const url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);

  if (request.method === "GET" && url.pathname === "/healthz") {
    send(response, 200, { ok: true });
    return;
  }

  if (request.method === "GET" && url.pathname === "/received") {
    send(response, 200, { received });
    return;
  }

  if (request.method === "POST" && url.pathname === "/hooks/omnion") {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      const body = Buffer.concat(chunks);
      const raw = request.headers["x-omnion-timestamp"];
      const timestamp = Number(raw);
      const valid = verify(Number.isNaN(timestamp) ? 0 : timestamp, body, request.headers["x-omnion-signature"]);

      let payload = null;
      try {
        payload = JSON.parse(body.toString("utf8"));
      } catch {
        payload = null;
      }

      const entry = {
        at: new Date().toISOString(),
        event: request.headers["x-omnion-event"] ?? null,
        delivery: request.headers["x-omnion-delivery"] ?? null,
        timestamp: Number.isNaN(timestamp) ? null : timestamp,
        signature_valid: valid,
        bytes: body.length,
        body: payload,
      };
      received.push(entry);
      console.log(JSON.stringify(entry));

      if (!valid) {
        send(response, 401, { ok: false, error: "signature_invalid" });
        return;
      }

      send(response, 200, { ok: true });
    });
    return;
  }

  send(response, 404, { ok: false, error: "not_found" });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`webhook receiver listening on http://127.0.0.1:${port}`);
});
