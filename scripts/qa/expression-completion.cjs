#!/usr/bin/env node
// Live proof of the expression autocomplete and the embedded editors (REQ-086, slice 3)
// against the private w10 stack.
//
// Deliberately not a browser pass, for the same reason `graph-expressions.cjs` is not one: the
// claim is about the **answer**, and specifically about three things a screenshot cannot show
// without a queue slot five other writers are holding —
//
//   1. the candidates follow the graph the caller *sent*, not the stored one. A completion
//      list built from the saved graph answers against a topology the person has already
//      changed on screen, and it does so at the moment they are most likely to trust it;
//   2. a node key the graph does not carry is a **named 404**, not an empty list, because an
//      empty list reads as "nothing completes here";
//   3. the three groups the REQ names are distinguishable in the answer, so the menu can show
//      them without the client re-deriving anything.
//
// The editor widgets themselves are DOM, and those are covered by the browser walkthrough
// (`runGraphCanvasDepth`). What this probe proves is that the server half of the feature is
// reachable, correct and guarded — the half a unit test cannot prove and a screenshot cannot
// read.
//
//   QA_STACK=w10 QA_API_PORT=18089 bash scripts/qa/expression-completion.cjs

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
  event: { title: "Published", author: { name: "ada" } },
  node: { count: 2 },
};

/** A three-node graph. `trigger_1 → fetch_1 → notify_1`, so `notify_1` has one parent. */
const GRAPH = {
  nodes: [
    { key: "trigger_1", type: "manual_trigger", label: "When clicked", position: { x: 0, y: 0 } },
    { key: "fetch_1", type: "http_request", label: "Fetch", position: { x: 260, y: 0 } },
    { key: "notify_1", type: "http_request", label: "Notify", position: { x: 520, y: 0 } },
  ],
  connections: [
    { from: "trigger_1", from_port: "main", to: "fetch_1", to_port: "in" },
    { from: "fetch_1", from_port: "main", to: "notify_1", to_port: "in" },
  ],
  notes: [],
};

/** The labels of the candidates, in the order the route returned them. */
const labelsOf = (body) =>
  (body?.candidates ?? []).map((candidate) => candidate.label).filter(Boolean);

const sourceOf = (body, label) =>
  (body?.candidates ?? []).find((candidate) => candidate.label === label)?.source;

async function main() {
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

  // The workflow's **stored** graph is left empty on purpose. Every upstream candidate below
  // can then only have come from the request body, which is the whole claim.
  const created = await call(cookie, "POST", "/api/v1/workflows", {
    name: `Completion probe ${stamp}`,
    description: "slice 3 completion round trip",
    enabled: true,
    trigger: { kind: "manual" },
    steps: [{ name: "start", kind: "task", action: "noop", params: {} }],
  });
  if (created.status !== 201) {
    console.log(`SKIP: could not create a workflow (${created.status}): ${created.text}`);
    process.exit(0);
  }
  const id = created.body.id;
  const complete = (body) =>
    call(cookie, "POST", `/api/v1/workflows/${id}/graph/expressions/complete`, body);

  console.log("expression completion — live round trip");
  console.log(`  ${API}  workflow ${id}`);

  // 1. The three groups, together.
  const all = await complete({
    node_key: "notify_1",
    prefix: "",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check("an empty prefix answers 200", all.status === 200, `status ${all.status}: ${all.text}`);
  const labels = labelsOf(all.body);
  check("the runtime namespaces are offered", labels.includes("$item"), labels.join(", "));
  check("the pinned sample's paths are offered", labels.includes("event.title"), labels.join(", "));
  check("the upstream node is offered", labels.includes("fetch_1"), labels.join(", "));

  // 2. Sources are distinguishable — the REQ names all three.
  check(
    "the three sources are distinguishable",
    sourceOf(all.body, "$item") === "runtime" &&
      sourceOf(all.body, "event.title") === "sample" &&
      sourceOf(all.body, "fetch_1") === "upstream",
    JSON.stringify(
      (all.body?.candidates ?? []).map((c) => `${c.label}:${c.source}`),
    ),
  );

  // 3. Upstream is DIRECT. A grandparent is not in the step's namespace today, so offering it
  //    suggests an expression that stops resolving the moment the middle wire is deleted.
  check(
    "only the direct parent is in scope",
    JSON.stringify(all.body?.upstream_nodes) === JSON.stringify(["fetch_1"]),
    JSON.stringify(all.body?.upstream_nodes),
  );
  check(
    "a grandparent is not offered",
    !labels.includes("trigger_1"),
    labels.join(", "),
  );

  // 4. The candidates follow the graph the caller sent. `notify_2` is wired to `trigger_1`
  //    here and to nothing else, and it is not in the stored graph at all — so a list built
  //    from the stored graph would say something different.
  const rewired = {
    nodes: [
      ...GRAPH.nodes,
      { key: "notify_2", type: "http_request", label: "Notify 2", position: { x: 780, y: 0 } },
    ],
    connections: [
      { from: "trigger_1", from_port: "main", to: "notify_2", to_port: "in" },
    ],
    notes: [],
  };
  const sent = await complete({
    node_key: "notify_2",
    prefix: "",
    graph: rewired,
    namespaces: {},
  });
  check("an unsaved wire is answered from", sent.status === 200, sent.text);
  check(
    "the answer follows the graph in the body, not the stored one",
    JSON.stringify(sent.body?.upstream_nodes) === JSON.stringify(["trigger_1"]),
    JSON.stringify(sent.body?.upstream_nodes),
  );

  // 5. A prefix narrows the list; an impossible one is an empty SUCCESS.
  const narrowed = await complete({
    node_key: "notify_1",
    prefix: "$va",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check(
    "a prefix narrows the list to what matches",
    narrowed.status === 200 && JSON.stringify(labelsOf(narrowed.body)) === JSON.stringify(["$vars"]),
    narrowed.text,
  );

  // Three characters into a namespace the person had not chosen yet. Refusing this would be
  // worse than offering everything — a list that empties mid-typing is the thing people
  // report as "autocomplete does not work".
  const partial = await complete({
    node_key: "notify_1",
    prefix: "$it",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check(
    "a prefix typed inside a namespace still matches",
    JSON.stringify(labelsOf(partial.body)) === JSON.stringify(["$item"]),
    partial.text,
  );

  const none = await complete({
    node_key: "notify_1",
    prefix: "zzzz",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check(
    "an impossible prefix is an empty success, not an error",
    none.status === 200 && none.body?.candidate_count === 0,
    none.text,
  );

  // The whole expression is accepted as a prefix too, so typing `{{` cannot empty the menu.
  const braced = await complete({
    node_key: "notify_1",
    prefix: "{{$va",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check(
    "a prefix written as a whole expression is accepted",
    JSON.stringify(labelsOf(braced.body)) === JSON.stringify(["$vars"]),
    braced.text,
  );

  // 6. A node key the graph does not carry is a NAMED 404, not an empty list.
  const noSuchNode = await complete({
    node_key: "no_such_node",
    prefix: "",
    graph: GRAPH,
    namespaces: SAMPLE,
  });
  check(
    "a node key the graph does not carry is a 404 naming the node",
    noSuchNode.status === 404 &&
      noSuchNode.body?.error?.code === "node_not_found" &&
      String(noSuchNode.body?.error?.message ?? "").includes("no_such_node"),
    noSuchNode.text,
  );

  // 7. A misspelled key is refused rather than answered from defaults — otherwise the caller
  //    sees four runtime namespaces and concludes the node has no parents.
  const typo = await call(
    cookie,
    "POST",
    `/api/v1/workflows/${id}/graph/expressions/complete`,
    { node_key: "notify_1", prefx: "", graph: GRAPH },
  );
  check(
    "a misspelled field is refused, not defaulted",
    typo.status === 400 || typo.status === 422,
    `${typo.status}: ${typo.text}`,
  );

  const noGraph = await call(cookie, "POST", `/api/v1/workflows/${id}/graph/expressions/complete`, {
    node_key: "notify_1",
    prefix: "",
  });
  check(
    "a missing graph is refused, not read as an empty one",
    noGraph.status === 400 || noGraph.status === 422,
    `${noGraph.status}: ${noGraph.text}`,
  );

  // 8. It is a read, and it is scoped exactly like the preview.
  const anonymous = await call(
    null,
    "POST",
    `/api/v1/workflows/${id}/graph/expressions/complete`,
    { node_key: "notify_1", prefix: "", graph: GRAPH, namespaces: SAMPLE },
  );
  check(
    "signed out is refused",
    anonymous.status === 401 || anonymous.status === 403,
    `status ${anonymous.status}`,
  );

  const missing = await call(
    cookie,
    "POST",
    "/api/v1/workflows/00000000-0000-4000-8000-000000000000/graph/expressions/complete",
    { node_key: "notify_1", prefix: "", graph: GRAPH, namespaces: SAMPLE },
  );
  check(
    "a workflow that does not exist is a 404 with the same code the preview uses",
    missing.status === 404 && missing.body?.error?.code === "workflow_not_found",
    missing.text,
  );

  // 9. An offered candidate is a promise the preview keeps. This is the assertion that ties
  //    the two halves of slice 3 together: if the completion list ever offers a path the
  //    evaluator refuses, the feature is worse than no feature at all.
  const offered = labelsOf(all.body).filter((label) => label.startsWith("event."));
  const previewed = await call(
    cookie,
    "POST",
    `/api/v1/workflows/${id}/graph/expressions/preview`,
    {
      params: Object.fromEntries(offered.map((label) => [`probe_${label}`, `{{${label}}}`])),
      namespaces: SAMPLE,
    },
  );
  check(
    "every offered sample path previews without a refusal",
    previewed.status === 200,
    `offered ${offered.join(", ")} → ${previewed.status}: ${previewed.text}`,
  );

  notes.push(`probed ${API} workflow ${id}`);
  console.log(`\n${passed} passed, ${failed} failed`);
  if (notes.length) console.log(notes.join("\n"));

  const out = path.join("/tmp", "w10-expression-completion.json");
  fs.writeFileSync(out, JSON.stringify({ passed, failed, api: API, workflow: id }, null, 2));
  console.log(`wrote ${out}`);

  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`probe error: ${error.stack || error.message}`);
  process.exit(2);
});
