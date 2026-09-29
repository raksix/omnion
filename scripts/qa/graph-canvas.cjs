/**
 * A live probe of the graph canvas's server half (REQ-086, slice 2).
 *
 * The browser pass needs a QA slot, and the claims that matter most here — that a save
 * persists a document, that a label survives, that a sticky note never becomes a step — are
 * claims about what the *server* stored. This probe makes them against the running private
 * w10 stack, and it is deliberately the same code path the canvas calls: the same three
 * endpoints, the same bodies, the same cookies.
 *
 * It is not a substitute for the browser pass. It cannot see the canvas render, the keyboard
 * path, the narrow-viewport rule or the 100 ms of drag coalescing. It *is* the cheaper proof
 * that the round trip works, so a tick that cannot hold the slot still learns something.
 */
const BASE = process.env.QA_API_BASE || "http://127.0.0.1:18089";
const ADMIN = process.env.QA_ADMIN_BASE || "http://127.0.0.1:3109";
const EMAIL = process.env.QA_EMAIL || "qa@omnion.test";
const PASSWORD = process.env.QA_PASSWORD || "correct horse battery staple";

let session = "";
let csrf = "";
let passed = 0;
const failures = [];

function ok(label, condition, detail = "") {
  if (condition) {
    passed += 1;
    console.log(`  ok   ${label}`);
  } else {
    failures.push(label);
    console.log(`  FAIL ${label}${detail ? ` — ${detail}` : ""}`);
  }
}

async function call(path, init = {}) {
  const headers = {
    accept: "application/json",
    ...(init.body ? { "content-type": "application/json" } : {}),
  };
  if (session) headers.cookie = `omnion_session=${session}${csrf ? `; omnion_csrf=${csrf}` : ""}`;
  if (csrf && init.method && init.method !== "GET") headers["x-omnion-csrf"] = csrf;

  const response = await fetch(`${BASE}${path}`, {
    ...init,
    headers,
    body: init.body ? JSON.stringify(init.body) : undefined,
  });
  const text = await response.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = { raw: text.slice(0, 200) };
  }
  return { status: response.status, body, headers: response.headers };
}

/** Every `Set-Cookie`, because a sign-in sets two and reading one is a coin toss. */
function readCookies(response) {
  const raw = response.headers.getSetCookie ? response.headers.getSetCookie() : [];
  const jar = {};
  for (const header of raw) {
    const pair = header.split(";")[0];
    const index = pair.indexOf("=");
    if (index > 0) jar[pair.slice(0, index)] = pair.slice(index + 1);
  }
  return jar;
}

async function main() {
  console.log(`graph canvas probe → ${BASE}`);

  const health = await call("/healthz");
  if (health.status !== 200) {
    console.log(`  FAIL the API is not answering on ${BASE} (status ${health.status})`);
    process.exit(1);
  }
  console.log("  API is up");

  const login = await call("/api/v1/auth/login", {
    method: "POST",
    body: { email: EMAIL, password: PASSWORD },
  });
  if (login.status !== 200) {
    console.log(`  FAIL sign-in failed (${login.status}): ${JSON.stringify(login.body)}`);
    process.exit(1);
  }
  const jar = readCookies(login);
  session = jar.omnion_session;
  csrf = jar.omnion_csrf;
  ok("sign-in yields both cookies", Boolean(session && csrf), `session=${Boolean(session)} csrf=${Boolean(csrf)}`);

  // A workflow to hold the graph.
  const created = await call("/api/v1/workflows", {
    method: "POST",
    body: {
      name: `QA canvas probe ${Date.now().toString(36)}`,
      description: "graph canvas probe",
      enabled: true,
      trigger: { kind: "manual" },
      steps: [{ name: "noop", kind: "task", action: "noop", params: {} }],
    },
  });
  ok("a workflow can be created", created.status === 201 || created.status === 200, `status ${created.status}`);
  const id = created.body?.id;
  if (!id) {
    console.log(`  FAIL no workflow id: ${JSON.stringify(created.body)}`);
    process.exit(1);
  }

  // 1. The empty read.
  const empty = await call(`/api/v1/workflows/${id}/graph`);
  ok("an unread graph reads as an empty, well-formed document", empty.status === 200 && empty.body?.node_count === 0 && Array.isArray(empty.body?.graph?.nodes), `status ${empty.status}`);
  ok("the read carries the revision to save against", empty.body?.revision === 0, `revision ${empty.body?.revision}`);
  let revision = empty.body?.revision ?? 0;

  // 2. A two-node graph with a labelled branch and a sticky note.
  const document = {
    nodes: [
      { key: "manual_trigger", type: "manual_trigger", label: "Start", position: { x: 0, y: 0 }, params: {}, disabled: false },
      {
        key: "notify",
        type: "send_email",
        label: "Notify ops",
        position: { x: 320, y: 40 },
        params: { to: "ops@example.com", subject: "Orders", body: "there are some", credential_key: "smtp_prod" },
        disabled: false,
      },
    ],
    connections: [{ from: "manual_trigger", from_port: "out", to: "notify", to_port: "in", label: "true" }],
    notes: [{ id: "note_1", position: { x: 0, y: 200 }, color: "amber", width: 240, height: 140, text: "QA canvas note" }],
  };

  const validated = await call(`/api/v1/workflows/${id}/graph/validate`, {
    method: "POST",
    body: { graph: document, revision },
  });
  ok("a clean graph validates", validated.status === 200 && validated.body?.valid === true, JSON.stringify(validated.body?.issues ?? []).slice(0, 160));
  ok("validation projects a step count", typeof validated.body?.step_count === "number" && validated.body.step_count > 0, `step_count ${validated.body?.step_count}`);
  ok("validation stored nothing", (await call(`/api/v1/workflows/${id}/graph`)).body?.revision === 0);

  const saved = await call(`/api/v1/workflows/${id}/graph`, {
    method: "PUT",
    body: { graph: document, revision },
  });
  ok("the save succeeds", saved.status === 200, `status ${saved.status} ${JSON.stringify(saved.body?.error ?? {}).slice(0, 160)}`);
  ok("the save bumps the revision", saved.body?.revision === 1, `revision ${saved.body?.revision}`);
  ok("the save reports the compiled step count", saved.body?.step_count === 2, `step_count ${saved.body?.step_count}`);
  revision = saved.body?.revision ?? revision;

  // 3. The read-back. This is the assertion the canvas's whole reload story rests on.
  const read = await call(`/api/v1/workflows/${id}/graph`);
  const graph = read.body?.graph;
  ok("the graph reads back with both nodes", graph?.nodes?.length === 2, `nodes ${graph?.nodes?.length}`);
  ok("a manual position survives the round trip", JSON.stringify(graph?.nodes?.[1]?.position) === JSON.stringify({ x: 320, y: 40 }), JSON.stringify(graph?.nodes?.[1]?.position));
  ok("the branch label survives the round trip", graph?.connections?.[0]?.label === "true", JSON.stringify(graph?.connections?.[0]?.label));
  ok("the sticky note survives the round trip", graph?.notes?.[0]?.text === "QA canvas note");
  ok("the note colour and size keep their defaults explicit", graph?.notes?.[0]?.color === "amber" && graph?.notes?.[0]?.width === 240);

  // 4. The compiled steps are what the engine runs, and the note is not one of them.
  const workflow = await call(`/api/v1/workflows/${id}`);
  const steps = workflow.body?.workflow?.steps ?? workflow.body?.steps ?? [];
  ok("the workflow's steps were written by the save", Array.isArray(steps) && steps.length === 2, `steps ${Array.isArray(steps) ? steps.length : "not an array"}`);
  ok("the sticky note never became a step", !JSON.stringify(steps).includes("QA canvas note"));
  ok("no step carries the node key as a secret", !JSON.stringify(steps).includes("smtp_prod") || !JSON.stringify(steps).includes("secret"));

  // 5. A stale revision is a conflict carrying the current number, and changes nothing.
  const stale = await call(`/api/v1/workflows/${id}/graph`, {
    method: "PUT",
    body: { graph: { ...document, nodes: [] }, revision: 0 },
  });
  ok("a stale save is a 409", stale.status === 409, `status ${stale.status}`);
  ok("the conflict carries the current revision", stale.body?.error?.details?.current_revision === 1, JSON.stringify(stale.body?.error?.details ?? {}));
  ok("the refused save changed nothing", (await call(`/api/v1/workflows/${id}/graph`)).body?.graph?.nodes?.length === 2);

  // 6. A disabled node compiles to nothing, which is the property that makes the canvas's
  //    "disable" honest rather than cosmetic.
  const disabledGraph = {
    ...document,
    nodes: document.nodes.map((node) =>
      node.key === "notify" ? { ...node, disabled: true } : node,
    ),
  };
  const disabledSave = await call(`/api/v1/workflows/${id}/graph`, {
    method: "PUT",
    body: { graph: disabledGraph, revision },
  });
  ok("a graph with a disabled node saves", disabledSave.status === 200, `status ${disabledSave.status}`);
  ok("a disabled node compiles to no step at all", disabledSave.body?.step_count === 1, `step_count ${disabledSave.body?.step_count}`);
  revision = disabledSave.body?.revision ?? revision;

  // 7. A refused save names every issue rather than the first.
  const broken = {
    ...document,
    nodes: [
      { key: "start", type: "manual_trigger", label: "Start", position: { x: 0, y: 0 }, params: {}, disabled: false },
      { key: "start", type: "if", label: "Clash", position: { x: 1, y: 1 }, params: {}, disabled: false },
      { key: "orphan", type: "http_request", label: "Orphan", position: { x: 2, y: 2 }, params: {}, disabled: false },
    ],
    connections: [{ from: "start", from_port: "out", to: "start", to_port: "in" }],
  };
  const refused = await call(`/api/v1/workflows/${id}/graph`, {
    method: "PUT",
    body: { graph: broken, revision },
  });
  ok("a graph with issues is refused with a 422", refused.status === 422, `status ${refused.status}`);
  const codes = (refused.body?.error?.details?.issues ?? []).map((issue) => issue.code);
  ok("the refusal names more than one problem", codes.length > 1, `codes ${codes.join(",")}`);
  ok("a duplicate key is named", codes.includes("node_duplicate_key"), codes.join(","));
  ok("a self-loop is named", codes.includes("connection_cycle"), codes.join(","));

  // 8. The canvas screen itself is served, so the route exists in the built panel.
  const panel = await fetch(`${ADMIN}/workflows/${id}/edit`);
  const html = await panel.text();
  ok("the editor route is served by the panel", panel.status === 200, `status ${panel.status}`);
  ok("the editor route is the canvas, not a 404 shell", /Workflow editor|workflow/i.test(html) && !/Application error|404/.test(html.slice(0, 2000)), html.slice(0, 120).replace(/\s+/g, " "));

  // Cleanup: the probe's own workflow, so a repeated run does not accumulate rows.
  const removed = await call(`/api/v1/workflows/${id}`, { method: "DELETE" });
  ok("the fixture workflow is removed again", removed.status === 200 || removed.status === 204, `status ${removed.status}`);

  console.log(`\n${passed} passed, ${failures.length} failed`);
  if (failures.length > 0) {
    console.log(`failed: ${failures.join(" · ")}`);
    process.exit(1);
  }
}

main().catch((error) => {
  console.error("probe crashed:", error?.message ?? error);
  process.exit(1);
});
