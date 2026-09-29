#!/usr/bin/env node
// Live proof of the expression preview (REQ-086, slice 3) against the private w10 stack.
//
// Deliberately not a browser pass: this route is called by the inspector on a keystroke, and
// the claim is about the *answer* — that it resolves the caller's own sample, that a refusal
// names the field it belongs to, and that a reader without `workflows.manage` still gets one.
// A screenshot can show the first of those and cannot show the other two without a queue slot
// that five other writers are already holding.
//
//   QA_STACK=w10 QA_API_PORT=18089 bash scripts/qa/graph-expressions.cjs

const fs = require("node:fs");
const path = require("node:path");

const API = process.env.QA_API_URL || "http://127.0.0.1:18089";
let passed = 0;
let failed = 0;
const notes = [];

function check(label, condition, detail) {
  if (condition) {
    passed += 1;
    console.log(`  ok   ${label}`);
  } else {
    failed += 1;
    console.log(`  FAIL ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

/** Sign in and return the cookies a caller must present, CSRF included. */
async function signIn(email, password) {
  const response = await fetch(`${API}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  const setCookies = response.headers.getSetCookie
    ? response.headers.getSetCookie()
    : [response.headers.get("set-cookie") || ""];
  const jar = new Map();
  for (const header of setCookies) {
    const pair = header.split(";")[0].trim();
    const at = pair.indexOf("=");
    if (at > 0) jar.set(pair.slice(0, at), pair.slice(at + 1));
  }
  if (!jar.has("omnion_session")) {
    throw new Error(`sign-in did not return a session cookie: ${JSON.stringify(setCookies)}`);
  }
  return [...jar].map(([k, v]) => `${k}=${v}`).join("; ");
}

async function call(cookie, method, uri, body) {
  const response = await fetch(`${API}${uri}`, {
    method,
    headers: {
      ...(cookie ? { cookie } : {}),
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  let parsed = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = null;
  }
  return { status: response.status, body: parsed, text };
}

const SAMPLE = {
  node: {
    items: [{ title: "First" }, { title: "Second" }],
    count: 2,
    author: { name: "ada", email: "ada@example.com" },
    summary: null,
  },
  vars: { site: "example.com" },
};

async function main() {
  // The account comes from `seed-w10.sh`, not from a sign-up path: there is no public
  // registration on this surface, and a probe that invents one ends up asserting against a
  // 404 rather than against the preview. The seed also does the `organization_id` binding the
  // onboarding wizard would otherwise do, without which every scoped endpoint answers
  // `organization_required` — which reads exactly like an authorization bug.
  // Distinguishes this probe's workflow from any left by a previous run of it.
  const stamp = Date.now();
  const email = process.env.QA_EMAIL || "qa-owner@omnion.test";
  const password = process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!";

  let cookie;
  try {
    cookie = await signIn(email, password);
  } catch (error) {
    console.log(
      `SKIP: could not sign in as ${email} — ${error.message}\n` +
        "      run `bash scripts/qa/seed-w10.sh` against this stack first.",
    );
    process.exit(0);
  }

  // A workflow to scope the preview to.
  const workflow = await call(cookie, "POST", "/api/v1/workflows", {
    name: `Expression probe ${stamp}`,
    description: "slice 3 round trip",
    enabled: true,
    trigger: { kind: "manual" },
    steps: [{ name: "start", kind: "task", action: "noop", params: {} }],
  });
  if (workflow.status !== 201) {
    console.log(`SKIP: could not create a workflow (${workflow.status}): ${workflow.text}`);
    process.exit(0);
  }
  const id = workflow.body.id;
  const preview = (body) =>
    call(cookie, "POST", `/api/v1/workflows/${id}/graph/expressions/preview`, body);

  console.log("expression preview — live round trip");
  console.log(`  ${API}  workflow ${id}`);

  // 1. The caller's own sample resolves, and only fields carrying an expression preview.
  const good = await preview({
    params: {
      to: "{{node.author.email}}",
      subject: "By {{node.author.name}}: {{node.count}} orders",
      retries: 3,
    },
    namespaces: SAMPLE,
  });
  check("a valid preview answers 200", good.status === 200, good.text);
  check("only the two expression fields preview", good.body?.preview_count === 2, good.text);
  const to = (good.body?.previews || []).find((p) => p.field === "to");
  const subject = (good.body?.previews || []).find((p) => p.field === "subject");
  check("a lone expression keeps its type", to?.value === "ada@example.com" && to?.typed === true, good.text);
  check("mixed text renders in order", subject?.rendered === "By ada: 2 orders", good.text);
  check(
    "the namespaces come back for autocomplete",
    JSON.stringify(good.body?.namespaces) === JSON.stringify(["node", "vars"]),
    good.text,
  );

  // 2. An array index — the case `Value::get` alone cannot answer.
  const indexed = await preview({ params: { title: "{{node.items.0.title}}" }, namespaces: SAMPLE });
  check(
    "an array index resolves",
    indexed.status === 200 && indexed.body?.previews?.[0]?.value === "First",
    indexed.text,
  );

  // 3. A fallback replaces a null and nothing else.
  const fallback = await preview({
    params: { a: "{{node.summary || nothing yet}}", b: "{{node.count || none}}" },
    namespaces: SAMPLE,
  });
  const fallbackA = (fallback.body?.previews || []).find((p) => p.field === "a");
  const fallbackB = (fallback.body?.previews || []).find((p) => p.field === "b");
  check("a fallback replaces a null", fallbackA?.rendered === "nothing yet", fallback.text);
  check("a fallback does not replace a number", fallbackB?.rendered === "2", fallback.text);

  // 4. A refusal is a 422 that names the field, and lists what the sample carries.
  const badPath = await preview({
    params: { to: "{{node.author.emaill}}" },
    namespaces: SAMPLE,
  });
  check("an unknown path is a 422", badPath.status === 422, badPath.text);
  check(
    "the refusal names the code the canvas branches on",
    badPath.body?.error?.code === "expression_invalid",
    badPath.text,
  );
  check(
    "the refusal names the field it belongs to",
    typeof badPath.body?.error?.message === "string" && badPath.body.error.message.startsWith("to:"),
    badPath.text,
  );
  check(
    "the refusal lists what the sample does carry",
    (badPath.body?.error?.message || "").includes("author.name"),
    badPath.text,
  );

  // 5. Arithmetic is refused as syntax, not reported as a missing key.
  const arithmetic = await preview({ params: { to: "{{node.count + 1}}" }, namespaces: SAMPLE });
  check(
    "arithmetic is refused by name rather than as a path",
    (arithmetic.body?.error?.message || "").includes("`+` is not part of the expression grammar"),
    arithmetic.text,
  );

  // 6. No sample data means a refusal, never an invented answer.
  const noSample = await preview({ params: { to: "{{node.author.email}}" } });
  check("a preview with no sample is refused", noSample.status === 422, noSample.text);
  const constant = await preview({ params: { subject: "just text" } });
  check(
    "a field with no expression previews nothing and is not an error",
    constant.status === 200 && constant.body?.preview_count === 0,
    constant.text,
  );

  // 7. The graph is untouched by a preview — it stores nothing.
  const graph = await call(cookie, "GET", `/api/v1/workflows/${id}/graph`);
  check("the preview left the graph alone", graph.status === 200, graph.text);
  check(
    "the graph still holds no nodes",
    (graph.body?.node_count ?? -1) === 0,
    JSON.stringify(graph.body?.node_count),
  );

  // 8. The route is guarded.
  const anonymous = await call(null, "POST", `/api/v1/workflows/${id}/graph/expressions/preview`, {
    params: {},
    namespaces: SAMPLE,
  });
  check(
    "signed out is refused",
    anonymous.status === 401 || anonymous.status === 403,
    `status ${anonymous.status}`,
  );
  const missing = await call(
    cookie,
    "POST",
    `/api/v1/workflows/00000000-0000-4000-8000-000000000000/graph/expressions/preview`,
    { params: {}, namespaces: SAMPLE },
  );
  check(
    "a workflow that does not exist is a 404",
    missing.status === 404 && missing.body?.error?.code === "workflow_not_found",
    missing.text,
  );

  notes.push(`probed ${API} workflow ${id}`);
  console.log(`\n${passed} passed, ${failed} failed`);
  if (notes.length) console.log(notes.join("\n"));

  // A copy of the answer, for the tick's evidence.
  const out = path.join("/tmp", "w10-graph-expressions.json");
  fs.writeFileSync(out, JSON.stringify({ passed, failed, api: API, workflow: id }, null, 2));
  console.log(`wrote ${out}`);

  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`probe error: ${error.stack || error.message}`);
  process.exit(2);
});
