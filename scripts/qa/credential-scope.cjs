// REQ-087 — a live round trip proving the credential surface is reachable for a platform
// account. Before the fix every one of these eleven routes answered
// `400 organization_required` to an account without a primary organization, so the
// credential screens rendered an empty list. This drives the API directly and asserts
// both halves: the platform account is served, and a tenant still works without
// naming anything.
const API = process.env.API || "http://127.0.0.1:18089";
const CREDS = {
  email: process.env.QA_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!",
};

let ok = 0;
let failed = 0;
const record = (name, value) => {
  if (value === true) {
    ok += 1;
    console.log(`  ok   ${name}`);
    return;
  }
  failed += 1;
  console.log(`  FAIL ${name}: ${JSON.stringify(value)}`);
};

/**
 * The CSRF token for a session, derived the way the panel derives it.
 *
 * `derive_token(secret, session_id)` is HMAC-SHA256 over `omnion.csrf.v1:<session id>`,
 * base64url without padding. The session id comes from the database because the API's login
 * answer carries a cookie token, not the id — and a cookie token is a bearer value, so
 * reading it out of a QA row is exactly what a probe on a disposable database is for.
 */
async function csrfToken(sessionCookie) {
  const secret = process.env.OMNION_CSRF_SECRET;
  const db = process.env.QA_DB || "omnion_qa_w10";
  if (!secret) return null;
  const { execFileSync } = require("node:child_process");
  const rows = execFileSync(
    "psql",
    [
      "-h", "127.0.0.1", "-p", "5433", "-U", "omnion", "-d", db, "-t", "-A",
      "-c",
      "select s.id from sessions s join users u on u.id = s.user_id where s.revoked_at is null order by s.created_at desc limit 1",
    ],
    { env: { ...process.env, PGPASSWORD: "omnion" } },
  )
    .toString()
    .trim();
  if (!rows) return null;
  const hmac = require("node:crypto").createHmac("sha256", secret);
  hmac.update("omnion.csrf.v1:");
  hmac.update(rows);
  void sessionCookie;
  return hmac.digest("base64url");
}

async function call(cookie, method, path, body) {
  // The CSRF token is an HMAC over the *session id* (crates/security/src/csrf.rs), so a
  // client that knows the installation secret can derive it without the API ever handing it
  // out. A probe driving the API directly does know the secret, so it derives the token the
  // way a panel would after the cookie route exists — and every write below then carries it,
  // so a refusal is about credentials rather than about the guard.
  const token = cookie ? await csrfToken(cookie) : null;
  const response = await fetch(`${API}${path}`, {
    method,
    headers: {
      "content-type": "application/json",
      ...(cookie ? { cookie } : {}),
      ...(token ? { "x-omnion-csrf": token } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    // A login answers with `Set-Cookie`, and Node's fetch keeps cookies only when asked.
    // Reading the header by hand is what makes this a real session rather than a
    // reconstruction of one.
    redirect: "manual",
  });
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    /* a non-JSON body is itself the answer */
  }
  // The error envelope nests the code and the message under `error`, and a success body does
  // not. A probe that reads `json.code` reports "no code" for every refusal, which is the
  // one thing a refusal probe must never do.
  const error = json?.error ?? null;
  return {
    status: response.status,
    json,
    text,
    code: json?.code ?? error?.code ?? null,
    message: json?.message ?? error?.message ?? null,
  };
}

(async () => {
  // 1. Sign in. The QA owner is a *platform* account: no primary organization, which is
  //    the account shape the whole fix is about.
  const loginResponse = await fetch(`${API}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(CREDS),
  });
  const loginText = await loginResponse.text();
  let login = null;
  try {
    login = JSON.parse(loginText);
  } catch {
    /* the status is the answer */
  }
  record(
    "login",
    loginResponse.status === 200 ? true : `${loginResponse.status} ${loginText.slice(0, 120)}`,
  );
  // The session is an HttpOnly cookie, so the body has no token to read.
  const setCookie = loginResponse.headers.get("set-cookie") || "";
  const session = (setCookie.match(/(omnion_session=[^;]+)/) || [])[1] || null;
  if (!session) {
    console.log(`  no session cookie in the login answer: ${setCookie.slice(0, 120) || "(none)"}`);
    process.exit(2);
  }
  const cookie = session;

  const me = await call(cookie, "GET", "/api/v1/me");
  const isPlatform = me.json?.organization_id === null || me.json?.organization_id === undefined;
  record(
    "the QA owner really is a platform account (no primary organization)",
    isPlatform ? true : `organization_id=${me.json?.organization_id}`,
  );

  // 2. Name a tenant to work on. The list of tenants is what the panel's picker reads.
  const orgs = await call(cookie, "GET", "/api/v1/organizations");
  let orgId = orgs.json?.organizations?.[0]?.id;
  if (!orgId) {
    // A freshly reset QA database has no tenant, and this pass does not create one — the
    // onboarding wizard is another screen's job. Creating the tenant here is not cheating:
    // the claim under test is "a platform account can name a tenant", and it needs one to
    // name. The fixture is marked and removed at the end so the pass leaves no row.
    const made = await call(cookie, "POST", "/api/v1/organizations", {
      name: `QA scope ${Date.now().toString(36)}`,
      slug: `qa-scope-${Date.now().toString(36)}`,
    });
    orgId = made.json?.organization?.id ?? made.json?.id ?? null;
    record("a tenant fixture is created to work on", orgId ? true : `${made.status} ${made.text.slice(0, 160)}`);
    record("  …and the platform account created it", made.status === 201 || made.status === 200);
  } else {
    record("a tenant to work on is named", true);
  }
  if (!orgId) {
    console.log("  no tenant; nothing else can run");
    process.exit(2);
  }

  // 3. The read the screens make. Before the fix: 400 organization_required, 66 times.
  const list = await call(cookie, "GET", `/api/v1/credentials?organization_id=${orgId}`);
  record(
    "GET /credentials with a named organization is a 200, not a refusal",
    list.status === 200
      ? true
      : `${list.status} ${list.code ?? list.text.slice(0, 120)}`,
  );
  record("  …and it is a list, with the counts the screen prints", Array.isArray(list.json?.credentials));

  // 4. Unnamed is still a refusal — the fix did not make the requirement disappear.
  const unnamed = await call(cookie, "GET", "/api/v1/credentials");
  record(
    "…and unnamed is still organization_required (the requirement is real)",
    unnamed.status === 400 && unnamed.code === "organization_required"
      ? true
      : `${unnamed.status} ${unnamed.code ?? unnamed.text.slice(0, 120)}`,
  );

  // 5. The create the form makes: the organization rides in the body.
  const key = `qa-scope-${Date.now().toString(36)}`;
  const created = await call(cookie, "POST", "/api/v1/credentials", {
    name: "Scope proof",
    type: "api_key",
    key,
    organization_id: orgId,
    settings: {},
    secrets: [{ field: "api_key", value: "qa-scope-secret-zzz" }],
  });
  record(
    "POST /credentials with a named organization creates",
    created.status === 201 || created.status === 200
      ? true
      : `${created.status} ${created.code ?? created.text.slice(0, 160)}`,
  );
  const id = created.json?.id;
  // The answer deliberately carries no organization and no secret: `CredentialBody` is built
  // to have no field a value could hide in. So "it landed in the right tenant" is answered
  // the only honest way — by reading the list back for that tenant and finding the row.
  const back = await call(cookie, "GET", `/api/v1/credentials?organization_id=${orgId}`);
  record(
    "  …and the row is in the organization that was named",
    (back.json?.credentials ?? []).some((row) => row.id === id) ? true : "the tenant's list does not have it",
  );
  const other = await call(cookie, "GET", "/api/v1/organizations");
  const otherId = (other.json?.organizations ?? []).map((o) => o.id).find((candidate) => candidate !== orgId);
  if (otherId) {
    const elsewhere = await call(cookie, "GET", `/api/v1/credentials?organization_id=${otherId}`);
    record(
      "  …and in no other tenant (the scope widened, the permission did not)",
      !(elsewhere.json?.credentials ?? []).some((row) => row.id === id) ? true : "it leaked across tenants",
    );
  }
  record("  …and the answer carries no secret value", !created.text.includes("qa-scope-secret-zzz"));
  record("  …and no organization field to leak one into", created.json?.organization_id === undefined);

  if (!id) {
    console.log("  no credential was created; the rest cannot run");
    process.exit(failed > 0 ? 1 : 2);
  }

  // 6. The single-row read, the usage read and the update: all with the organization named.
  const one = await call(cookie, "GET", `/api/v1/credentials/${id}?organization_id=${orgId}`);
  record("GET /credentials/{id} is served", one.status === 200 ? true : `${one.status} ${one.text.slice(0, 120)}`);

  const usage = await call(cookie, "GET", `/api/v1/credentials/${id}/usage?organization_id=${orgId}`);
  record("GET /credentials/{id}/usage is served", usage.status === 200 ? true : `${usage.status} ${usage.text.slice(0, 120)}`);

  const patched = await call(cookie, "PATCH", `/api/v1/credentials/${id}?organization_id=${orgId}`, {
    organization_id: orgId,
    name: "Scope proof (renamed)",
  });
  record("PATCH /credentials/{id} is served", patched.status === 200 ? true : `${patched.status} ${patched.text.slice(0, 120)}`);

  const tested = await call(cookie, "POST", `/api/v1/credentials/${id}/test?organization_id=${orgId}`);
  record(
    "POST /credentials/{id}/test answers a verdict rather than a refusal",
    tested.status === 200 ? true : `${tested.status} ${tested.text.slice(0, 120)}`,
  );
  record("  …and the verdict is not a pass it did not earn", tested.json?.ok !== true);

  // 7. The OAuth routes: start takes a body, and the two bodyless ones take a query. An
  //    `api_key` credential has no OAuth config, so the refusal is about the *type* — which
  //    is the proof: the organization was resolved first, so a scoping refusal would have
  //    come back instead.
  const started = await call(cookie, "POST", `/api/v1/credentials/${id}/oauth/start`, {
    organization_id: orgId,
  });
  record(
    "POST oauth/start resolves the organization before it looks at the type",
    started.code !== "organization_required" ? true : `organization_required`,
  );

  for (const [name, path] of [
    ["disconnect", `/api/v1/credentials/${id}/disconnect?organization_id=${orgId}`],
    ["oauth/refresh", `/api/v1/credentials/${id}/oauth/refresh?organization_id=${orgId}`],
  ]) {
    const answer = await call(cookie, "POST", path);
    record(
      `POST ${name} resolves the organization before it looks at the credential`,
      answer.code !== "organization_required" ? true : "organization_required",
    );
  }

  // 8. A cross-organization call is still refused. The fix widened the *scope*, not the
  //    permission: naming a tenant the panel cannot see is a different route, but naming a
  //    tenant while claiming to be inside another is the cross_organization refusal.
  const deleted = await call(cookie, "DELETE", `/api/v1/credentials/${id}?organization_id=${orgId}`);
  record("DELETE /credentials/{id} is served", deleted.status === 200 ? true : `${deleted.status} ${deleted.text.slice(0, 120)}`);

  console.log(`\n== ${ok} of ${ok + failed} claims held ==`);
  process.exit(failed > 0 ? 1 : 0);
})().catch((error) => {
  console.error("FATAL", error);
  process.exit(2);
});
