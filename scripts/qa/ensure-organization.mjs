// Ensure the QA stack has an organization, so the rule screens are not testing an empty tenant.
//
// The first-run wizard creates the owner account and stops there. A freshly reset QA database
// therefore holds a platform account (`organization_id IS NULL`) with an EMPTY organizations
// table, and every rule belongs to a tenant: the editor refuses its own save with "Choose an
// organization before saving a rule.", the list renders empty, and each depth note downstream
// reads as a broken screen. The tenant picker does not render at all — it is gated on
// `organizations.length > 1` — so a probe has nothing to click. That is a harness gap, not a
// product defect.
//
// `POST /onboarding/organization` is the endpoint that exists for exactly this state and
// refuses once a tenant exists, which makes this idempotent: it fills the gap when there is one
// and reports "already exists" when there is not. It is run BEFORE the walkthrough so the pass
// measures the product rather than the seeding.
//
// Usage: node scripts/qa/ensure-organization.mjs --url <api> --admin <panel>

// Both spellings are accepted: `--name=value` and `--name value`. A caller reaching for the
// second gets the default for the first, which looks exactly like the flag being ignored -- and
// for `--admin` that default is the MAIN writer's port, so the step would quietly talk to a stack
// that is not this writer's.
function arg(name, fallback) {
  const prefix = `--${name}=`;
  const joined = process.argv.find((entry) => entry.startsWith(prefix));
  if (joined) return joined.slice(prefix.length);
  const flag = process.argv.indexOf(`--${name}`);
  if (flag >= 0 && flag + 1 < process.argv.length) return process.argv[flag + 1];
  return fallback;
}

const URL_API = arg("url", "http://127.0.0.1:18080");
const URL_ADMIN = arg("admin", "http://127.0.0.1:3100");
const NAME = arg("name", "QA Organization");
const SLUG = arg("slug", "qa-org");
const EMAIL = arg("email", "qa-owner@omnion.test");
const PASSWORD = arg("password", "OmnionQa-Passw0rd-2026!");

// The panel's `/api/v1/*` routes are the ones the browser uses, so the cookie the CSRF guard
// reads is minted by the same origin the walkthrough signs in against. Talking to the API
// directly would work too, but the session cookie is scoped and the panel is what the pass uses.
const cookieJar = new Map();

function storeCookies(response) {
  const raw = response.headers.getSetCookie ? response.headers.getSetCookie() : [];
  for (const line of raw) {
    const [pair] = line.split(";");
    const index = pair.indexOf("=");
    if (index > 0) cookieJar.set(pair.slice(0, index).trim(), pair.slice(index + 1).trim());
  }
}

function cookieHeader() {
  return Array.from(cookieJar.entries())
    .map(([name, value]) => `${name}=${value}`)
    .join("; ");
}

// The CSRF token is minted as a readable cookie at sign-in and the server accepts it from
// either the cookie or the header, so carrying the jar is enough. The header is added as well
// because that is the path a browser takes and it costs nothing.
function csrfHeader() {
  const token = cookieJar.get("omnion_csrf");
  return token ? { "x-omnion-csrf": token } : {};
}

async function call(url, init) {
  const response = await fetch(url, {
    ...init,
    headers: {
      // Only send a `cookie` header once there is one: the first request has an empty jar, and
      // an empty `cookie:` header is not the same as omitting it.
      ...(cookieJar.size ? { cookie: cookieHeader() } : {}),
      ...(init.body ? { "content-type": "application/json" } : {}),
      ...csrfHeader(),
      ...(init.headers || {}),
    },
  });
  storeCookies(response);
  const text = await response.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = { raw: text.slice(0, 200) };
  }
  return { status: response.status, body };
}

async function main() {
  // `GET /onboarding` is the one endpoint here that needs no session: a client has to be able to
  // ask "is this installation set up?" before it can sign in, and the answer holds no secret
  // (step booleans and static labels). Asking it FIRST is what makes the fresh-database case
  // legible instead of looking like a broken sign-in.
  //
  // The previous version signed in first and reported `401 invalid_credentials` on every fresh
  // reset, which is the one state this step exists for. run.sh resets the database, so the owner
  // does not exist yet: there is nobody to sign in as, and the walkthrough's wizard — which runs
  // immediately after this step — is what creates the account, the organization and the site. So
  // the step was not merely failing, it was guaranteed to fail on every pass and print
  // "rule screens will report empty" over a stack that was about to be seeded correctly.
  const status = await call(`${URL_ADMIN}/api/v1/onboarding`, { method: "GET" });
  const steps = status.body?.steps;
  if (status.status !== 200 || !steps || typeof steps.owner !== "boolean") {
    console.error(
      `[qa] onboarding status unreadable (${status.status}): ${JSON.stringify(status.body).slice(0, 200)}`,
    );
    process.exit(1);
  }

  // No account yet is NOT a failure: the wizard is mid-flight and owns this state end to end.
  // Claiming the pass is broken here is how a working pass gets reported as a broken product.
  if (!steps.owner) {
    console.log("[qa] no account yet — the first-run wizard seeds the owner, organization and site");
    return;
  }

  // An account without a tenant is the one state this step really repairs: a resumed or partial
  // first run leaves exactly that, and every rule belongs to a tenant.
  if (steps.organization) {
    console.log("[qa] organization already present (onboarding reports it)");
    return;
  }

  const login = await call(`${URL_ADMIN}/api/v1/auth/login`, {
    method: "POST",
    body: JSON.stringify({ email: EMAIL, password: PASSWORD }),
  });
  if (login.status !== 200) {
    console.error(
      `[qa] an account exists but sign-in failed (${login.status}): ${JSON.stringify(login.body).slice(0, 200)}`,
    );
    process.exit(1);
  }

  const before = await call(`${URL_ADMIN}/api/v1/organizations`, { method: "GET" });
  const existing = Array.isArray(before.body?.organizations) ? before.body.organizations : [];
  if (existing.length > 0) {
    console.log(`[qa] organization already present (${existing.length})`);
    return;
  }

  // A platform account has no tenant and the list is empty, so this is the state the endpoint
  // exists for. `step_already_done` would mean something else created one in between, which is
  // also fine — the pass needs a tenant, not this particular call to have won.
  const created = await call(`${URL_ADMIN}/api/v1/onboarding/organization`, {
    method: "POST",
    body: JSON.stringify({ name: NAME, slug: SLUG }),
  });

  if (created.status === 200) {
    console.log(`[qa] organization created (${SLUG})`);
    return;
  }
  const code = created.body?.error?.code ?? "";
  if (code === "step_already_done") {
    console.log("[qa] organization already created by another step");
    return;
  }
  console.error(
    `[qa] organization not created (${created.status}, ${code}): ${JSON.stringify(created.body).slice(0, 200)}`,
  );
  process.exit(1);
}

main().catch((error) => {
  // `fetch failed` hides the reason in `cause` (a refused connection, a DNS miss, a reset), and
  // a harness step that reports only that string sends the next reader looking in the wrong place.
  const cause = error?.cause ? ` (${error.cause.message || error.cause.code || error.cause})` : "";
  console.error(`[qa] ensure-organization failed: ${error.message}${cause}`);
  process.exit(1);
});
