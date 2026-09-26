#!/usr/bin/env node
/**
 * A tiny OpenAI-compatible server for local development and CI (docs/06-AI-HUB.md, P11).
 *
 * It answers the two endpoints the AI Hub speaks to — `GET /v1/models` and
 * `POST /v1/chat/completions`, the latter in both the JSON and the `text/event-stream` shape —
 * with a fixed answer and fixed token counts. That makes a provider round trip reproducible
 * without a key from a real vendor: connect it as a provider with base URL
 * `http://127.0.0.1:<port>/v1`, and the platform's chat streams against it.
 *
 * The model key `broken-model` is answered with `500`, so the failure path can be exercised too.
 *
 * Usage: `node infra/mocks/openai-compatible.mjs [port]` (default 8123, or `$PORT`).
 * No dependencies — this file is meant to run on any machine that has Node.
 */

import { createServer } from "node:http";

const port = Number(process.argv[2] ?? process.env.PORT ?? 8123);
/** Models the mock reports. */
const MODELS = ["mock-small", "mock-large"];
/** Delay between stream chunks, so a client can see the answer arrive. */
const CHUNK_DELAY_MS = 40;

/** The answer the mock gives, model included so a caller can tell the routes apart. */
function answerFor(model) {
  return `Hello from the mock (${model}).`;
}

/** Token counts the mock pretends to have used. */
const USAGE = { prompt_tokens: 7, completion_tokens: 5, total_tokens: 12 };

/** Read a JSON body. */
function readJson(request) {
  return new Promise((resolve, reject) => {
    let raw = "";
    request.on("data", (chunk) => {
      raw += chunk;
    });
    request.on("end", () => {
      if (!raw) {
        resolve({});
        return;
      }
      try {
        resolve(JSON.parse(raw));
      } catch (cause) {
        reject(cause);
      }
    });
    request.on("error", reject);
  });
}

/** Write a JSON answer. */
function sendJson(response, status, body) {
  const payload = JSON.stringify(body);
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(payload),
  });
  response.end(payload);
}

const server = createServer(async (request, response) => {
  const url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);

  if (request.method === "GET" && url.pathname === "/v1/models") {
    sendJson(response, 200, {
      object: "list",
      data: MODELS.map((id) => ({ id, object: "model" })),
    });
    return;
  }

  if (request.method === "POST" && url.pathname === "/v1/chat/completions") {
    let body;
    try {
      body = await readJson(request);
    } catch {
      sendJson(response, 400, { error: { message: "the body must be JSON" } });
      return;
    }

    const model = typeof body.model === "string" ? body.model : "";
    const messages = Array.isArray(body.messages) ? body.messages : [];
    if (model === "") {
      sendJson(response, 400, { error: { message: "a model is required" } });
      return;
    }
    if (model === "broken-model") {
      sendJson(response, 500, { error: { message: "the mock cannot serve this model" } });
      return;
    }
    if (messages.length === 0) {
      sendJson(response, 400, { error: { message: "at least one message is required" } });
      return;
    }

    const answer = answerFor(model);

    if (body.stream !== true) {
      sendJson(response, 200, {
        id: "mock-completion",
        object: "chat.completion",
        model,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: answer },
            finish_reason: "stop",
          },
        ],
        usage: USAGE,
      });
      return;
    }

    response.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });

    const frames = [];
    for (const piece of answer.split(/(?<=\s)/)) {
      frames.push({ choices: [{ index: 0, delta: { content: piece } }] });
    }
    frames.push({ choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
    frames.push({ choices: [], usage: USAGE });

    let settled = false;
    request.on("close", () => {
      settled = true;
    });

    for (const frame of frames) {
      if (settled) {
        return;
      }
      response.write(`data: ${JSON.stringify(frame)}\n\n`);
      await new Promise((resolve) => setTimeout(resolve, CHUNK_DELAY_MS));
    }
    response.write("data: [DONE]\n\n");
    response.end();
    return;
  }

  sendJson(response, 404, { error: { message: `no route for ${request.method} ${url.pathname}` } });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`openai-compatible mock listening on http://127.0.0.1:${port}/v1`);
});
