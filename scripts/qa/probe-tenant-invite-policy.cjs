/**
 * Focused probe for the REQ-005 slice-3 remainder screens (invite policy, queue, Audit tab).
 *
 * The full walkthrough is the acceptance gate; this exists because the shared box was running
 * five other QA passes at once and the full pass died on `Page crashed` (host memory pressure,
 * not a defect) at media-trash — six routes before it ever reached these screens. The rule this
 * follows is the one in the ledger: prove the screen with a focused probe against your own stack
 * when the box is saturated, and do not conclude your change is slow because another writer's
 * browser is.
 *
 * Everything here runs against the w5 stack (API :18084, admin :3104, database omnion_qa_w5).
 */
const { chromium } = require("playwright-core");
const fs = require("fs");
const path = require("path");

const API = process.env.QA_API_BASE || "http://127.0.0.1:18084";
const ADMIN = process.env.QA_ADMIN_BASE || "http://127.0.0.1:3104";
const OUT = process.env.PROBE_OUT || "/tmp/probe-w5-slice3";
const CHROME =
  process.env.CHROME_PATH || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";

const CREDS = {
  email: "qa-owner@omnion.test",
  password: "OmnionQa-Passw0rd-2026!",
};

const steps = [];
let failures = 0;

function step(name, detail) {
  steps.push({ name, ...detail });
  console.log(`[probe] ${name} ${JSON.stringify(detail)}`);
}

function expect(condition, message) {
  if (condition) return true;
  failures += 1;
  console.log(`[probe] FAIL ${message}`);
  return false;
}

/** One API call with the session cookie, answering `{ status, body }`. */
async function api(pathname, { method = "GET", body, cookie } = {}) {
  const response = await fetch(`${API}${pathname}`, {
    method,
    headers: {
      ...(cookie ? { cookie } : {}),
      ...(body ? { "content-type": "application/json" } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
    redirect: "manual",
  });
  const text = await response.text();
  let parsed = null;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = text;
  }
  return { status: response.status, body: parsed };
}

async function login() {
  const response = await fetch(`${API}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: CREDS.email, password: CREDS.password }),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`login refused ${response.status}: ${text.slice(0, 200)}`);
  }
  const cookie = (response.headers.get("set-cookie") || "").split(";")[0];
  if (!cookie) throw new Error("login set no session cookie");
  return cookie;
}


/**
 * Create a *manager*: a member of the tenant holding `organizations.manage` and nothing more.
 *
 * The probe needs an account that is deliberately NOT an owner, because the QA owner holds
 * `owner` at global scope and the policy must not queue an owner — probing the queue with that
 * account proves nothing (it answers 201 with a link, correctly).
 *
 * Two details that make this work, and both were wrong on the first attempt:
 *
 * * **The password hash is copied from an existing account.** There is no public register route,
 *   and re-implementing argon2 in a probe would be a second implementation of the thing being
 *   verified. Copying the QA owner's own hash is honest: the probe authenticates as the manager
 *   with the *owner's* password, and the account exists purely to hold a different permission set.
 * * **The column names are `role_bindings.user_id` and `role_permissions.permission_key`.** Not
 *   `subject_id`/`subject_type` (migration 0016 adds those as generated columns a trigger fills
 *   from `user_id`) and not `key`. Reading the migration before naming columns is cheaper than
 *   debugging a trigger.
 */
async function createManager(organizationId, stamp) {
  const { execFileSync } = require("child_process");
  const email = `qa-manager-${stamp}@omnion.test`;
  const key = `probe-manager-${stamp.toString(36)}`;

  // Literal interpolation is safe *because* every value is generated here or read from the
  // database: the stamp is a number, the id is a uuid from the API, the hash is `$argon2…$`.
  // The one value a probe must never interpolate is a token, and there is none in this statement.
  const sql = `
    with source as (
      select password_hash from users where email = '${CREDS.email}'
    ),
    account as (
      insert into users (id, organization_id, email, password_hash, display_name, status)
      values (gen_random_uuid(), '${organizationId}', '${email}',
              (select password_hash from source), 'Probe Manager', 'active')
      returning id
    ),
    membership as (
      insert into organization_members (organization_id, user_id, status, is_primary)
      select '${organizationId}', id, 'active', false from account
      returning user_id
    ),
    role_row as (
      insert into roles (organization_id, key, name, description, priority)
      values ('${organizationId}', '${key}', 'Probe Manager',
              'A QA-only manager with no owner rights', 500)
      returning id
    ),
    granted as (
      insert into role_permissions (role_id, permission_key, effect)
      select id, 'organizations.manage', 'allow' from role_row
      returning role_id
    )
    insert into role_bindings (role_id, user_id, scope_type, organization_id)
      select role_id, user_id, 'organization', '${organizationId}' from membership, granted;
  `;

  execFileSync(
    "docker",
    [
      "exec",
      "-i",
      process.env.QA_PG_CONTAINER || "omnion-postgres",
      "psql",
      "-v",
      "ON_ERROR_STOP=1",
      "-q",
      "-U",
      "omnion",
      "-d",
      process.env.QA_DB || "omnion_qa_w5",
      "-c",
      sql,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );

  return { ok: true, email, password: CREDS.password };
}

async function loginAs(email, password) {
  const response = await fetch(`${API}/api/v1/auth/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  if (!response.ok) throw new Error(`manager login refused ${response.status}`);
  return (response.headers.get("set-cookie") || "").split(";")[0];
}

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const cookie = await login();
  step("login", { ok: true });

  // A tenant to work in, through the API the panel uses.
  //
  // The QA owner is a *tenant* account (`platform_only` refuses `POST /organizations`), so a new
  // tenant cannot be created here — and it does not need to be: the seeded `qa-org` is the tenant
  // the panel's own screens are exercised against. Discovering this from a 403 is the reason the
  // probe reads its tenant from the caller's own memberships rather than assuming a create works.
  //
  // The switcher names the tenant `organization_id` (it is a membership, not an organization), and
  // reading `.id` here produced `undefined` in every path — which surfaced as a 400 from the
  // server about a UUID, four steps later. Every layer of a probe can be quietly wrong while the
  // failure lands somewhere unrelated.
  const mine = await api("/api/v1/me/organizations", { cookie });
  const row = (mine.body?.organizations || []).find((entry) => entry.is_primary) ||
    (mine.body?.organizations || [])[0];
  const org = row
    ? { id: row.organization_id, slug: row.slug, name: row.name, primary: Boolean(row.is_primary) }
    : null;
  if (!org || !org.id) {
    console.log(`[probe] the account belongs to no tenant: ${JSON.stringify(mine).slice(0, 300)}`);
    process.exit(1);
  }
  step("tenant", org);

  const stamp = Date.now();
  const closedAddress = `probe-closed-${stamp}@omnion.test`;
  const queueAddress = `probe-queued-${stamp}@omnion.test`;

  const setPolicy = async (policy) =>
    api(`/api/v1/organizations/${org.id}/settings`, {
      method: "PUT",
      cookie,
      body: {
        locale: "en",
        timezone: "UTC",
        invite_policy: policy,
        default_invite_role_id: null,
        logo_media_id: null,
        accent_color: null,
        audit_retention_days: 365,
      },
    });

  // ---- 1. closed refuses, and names the policy -----------------------------------------------
  await setPolicy("closed");
  const closed = await api(`/api/v1/organizations/${org.id}/invitations`, {
    method: "POST",
    cookie,
    body: { email: closedAddress },
  });
  step("closed", { status: closed.status, code: closed.body?.error?.code });
  expect(closed.status === 403, `closed must refuse, got ${closed.status}`);
  expect(
    closed.body?.error?.code === "invitations_closed",
    `closed must refuse by name, got ${closed.body?.error?.code}`,
  );
  expect(
    closed.body?.error?.details?.invite_policy === "closed",
    "the refusal must carry the policy in details",
  );

  // ---- 2. self_serve hands over a link that works --------------------------------------------
  await setPolicy("self_serve");
  const live = await api(`/api/v1/organizations/${org.id}/invitations`, {
    method: "POST",
    cookie,
    body: { email: closedAddress },
  });
  step("self-serve", { status: live.status, hasToken: Boolean(live.body?.token) });
  expect(live.status === 201, `self_serve must create, got ${live.status}`);
  expect(Boolean(live.body?.token), "self_serve must hand over a link");

  const preview = await api(`/api/v1/invitations/${live.body.token}`);
  step("self-serve-preview", { usable: preview.body?.usable });
  expect(preview.body?.usable === true, "the self_serve link must be usable");

  // ---- 3. owner_approval queues, with no link at all -------------------------------------------
  //
  // The QA owner holds `owner` at *global* scope, so it is exactly the account the policy must
  // NOT queue: an owner inviting their own team is not a privilege escalation. Probing the queue
  // with that account proves nothing — the create answers 201 with a link, correctly. So the queue
  // is driven by a *manager*: an account that holds `organizations.manage` (so the route's own
  // guard lets it through) and is not an owner (so the policy has something to act on).
  const manager = await createManager(org.id, stamp);
  if (!manager.ok) {
    console.log(`[probe] could not set up a manager: ${manager.detail}`);
    process.exit(1);
  }
  step("manager", { email: manager.email, isOwner: false });

  await setPolicy("owner_approval");
  const managerCookie = await loginAs(manager.email, manager.password);
  const queued = await api(`/api/v1/organizations/${org.id}/invitations`, {
    method: "POST",
    cookie: managerCookie,
    body: { email: queueAddress },
  });
  step("queued", {
    status: queued.status,
    state: queued.body?.invitation?.status,
    tokenLength: (queued.body?.token || "").length,
  });
  expect(queued.status === 202, `a queued create is 202, got ${queued.status}`);
  expect(
    queued.body?.invitation?.status === "awaiting_approval",
    `the row must be queued, got ${queued.body?.invitation?.status}`,
  );
  expect(
    (queued.body?.token || "").length === 0,
    "a queued create must return NO token — the queue would be advisory otherwise",
  );

  // The manager cannot release its own queue entry — the whole difference between `self_serve`
  // and `owner_approval`. The route guard already passed; only the owner check can refuse it.
  const selfRelease = await api(
    `/api/v1/organizations/${org.id}/invitations/${queued.body?.invitation?.id}/release`,
    { method: "POST", cookie: managerCookie },
  );
  step("manager-release", { status: selfRelease.status, code: selfRelease.body?.error?.code });
  expect(
    selfRelease.status === 403 && selfRelease.body?.error?.code === "not_an_organization_owner",
    `a manager must not release its own queue entry, got ${selfRelease.status}/${selfRelease.body?.error?.code}`,
  );

  const queue = await api(`/api/v1/organizations/${org.id}/invitations/queue`, { cookie });
  step("queue", {
    status: queue.status,
    rows: queue.body?.invitations?.length,
    listed: queue.body?.invitations?.some((row) => row.email === queueAddress),
  });
  expect(queue.status === 200, `the queue must read, got ${queue.status}`);
  expect(
    queue.body?.invitations?.some((row) => row.email === queueAddress),
    "the queue must list the waiting row",
  );

  // The owner's release mints the link, exactly once. The QA owner *is* a global owner, so this
  // is the account the policy defers to.
  const released = await api(
    `/api/v1/organizations/${org.id}/invitations/${queued.body?.invitation?.id}/release`,
    { method: "POST", cookie },
  );
  step("owner-release", {
    status: released.status,
    state: released.body?.invitation?.status,
    hasToken: Boolean(released.body?.token),
  });
  expect(released.status === 200, `an owner must be able to release, got ${released.status}`);
  expect(Boolean(released.body?.token), "the release must hand over the link");
  const releasedPreview = await api(`/api/v1/invitations/${released.body?.token}`);
  step("owner-release-preview", { usable: releasedPreview.body?.usable });
  expect(releasedPreview.body?.usable === true, "the released link must work");

  // A second release is refused by name, because it would mint a *different* link and silently
  // orphan the one the inviter already sent.
  const releasedTwice = await api(
    `/api/v1/organizations/${org.id}/invitations/${queued.body?.invitation?.id}/release`,
    { method: "POST", cookie },
  );
  step("owner-release-twice", {
    status: releasedTwice.status,
    code: releasedTwice.body?.error?.code,
  });
  expect(
    releasedTwice.status === 409 && releasedTwice.body?.error?.code === "invitation_not_queued",
    `a second release must be refused by name, got ${releasedTwice.status}/${releasedTwice.body?.error?.code}`,
  );

  // ---- 4. the audit feed: this tenant's rows, the exact filter, the CSV ----------------------
  const feed = await api(`/api/v1/organizations/${org.id}/audit`, { cookie });
  const actions = new Set((feed.body?.actions || []).map((value) => value));
  step("audit", {
    status: feed.status,
    rows: feed.body?.entries?.length,
    total: feed.body?.total,
    actions: [...actions].slice(0, 8),
  });
  expect(feed.status === 200, `the audit feed must read, got ${feed.status}`);
  expect(
    actions.has("organization.settings.updated"),
    "the settings write must be in the trail — the filter is built from these rows",
  );
  expect(
    actions.has("organization.member.invitation_queued"),
    "a queued invitation must record its own action, distinct from an ordinary invite",
  );

  const everyRow = (feed.body?.entries || []).every(
    (row) => row.actor_type === "system" || typeof row.actor_name === "string",
  );
  expect(everyRow, "every row must name its actor, or say that nobody did it");

  const narrowed = await api(
    `/api/v1/organizations/${org.id}/audit?action=organization.settings.updated`,
    { cookie },
  );
  step("audit-filter", {
    status: narrowed.status,
    rows: narrowed.body?.entries?.length,
    total: narrowed.body?.total,
  });
  expect(narrowed.status === 200, `the exact filter must read, got ${narrowed.status}`);
  expect(
    (narrowed.body?.entries || []).every(
      (row) => row.action === "organization.settings.updated",
    ),
    "the exact filter must not leak other actions",
  );
  expect(
    (narrowed.body?.total || 0) < (feed.body?.total || 0),
    "a narrowed feed must count fewer rows than the whole one",
  );

  const typo = await api(`/api/v1/organizations/${org.id}/audit?actor=not-an-id`, { cookie });
  step("audit-typo", { status: typo.status, code: typo.body?.error?.code });
  expect(
    typo.status === 400 && typo.body?.error?.code === "invalid_actor_filter",
    `a typo'd actor must be refused, got ${typo.status}/${typo.body?.error?.code}`,
  );

  const csv = await fetch(
    `${API}/api/v1/organizations/${org.id}/audit?format=csv`,
    { headers: { cookie } },
  );
  const csvText = await csv.text();
  const csvRows = csvText.split("\n").filter((line) => line.trim() !== "");
  step("audit-csv", {
    status: csv.status,
    header: csvRows[0],
    rows: csvRows.length - 1,
    pageRows: (feed.body?.entries || []).length,
  });
  expect(
    csvRows.length - 1 === (feed.body?.entries || []).length,
    `the CSV must carry the page the tab renders: ${csvRows.length - 1} vs ${
      (feed.body?.entries || []).length
    }`,
  );

  // ---- 5. the two screens, in a browser -------------------------------------------------------
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();

  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 200));
  });
  page.on("pageerror", (error) => consoleErrors.push(String(error).slice(0, 200)));

  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('input[type="email"]', { timeout: 20000 });
  await page.locator('input[type="email"]').first().fill(CREDS.email);
  await page.locator('input[type="password"]').first().fill(CREDS.password);
  await page.locator('button[type="submit"]').first().click();
  await page.waitForSelector('nav[aria-label="Sections"]', { timeout: 30000 });
  step("browser-login", { ok: true });

  // The owner's release above drained the queue, so the panel has to be driven with a *new* one:
  // an empty queue renders nothing at all (a permanently empty box is a control that never does
  // anything), which is the correct behaviour and would prove nothing on screen.
  const panelAddress = `probe-panel-${Date.now()}@omnion.test`;
  await setPolicy("owner_approval");
  const panelQueued = await api(`/api/v1/organizations/${org.id}/invitations`, {
    method: "POST",
    cookie: managerCookie,
    body: { email: panelAddress },
  });
  expect(
    panelQueued.status === 202,
    `the panel's row must be queued by the manager, got ${panelQueued.status}`,
  );

  await page.goto(`${ADMIN}/organizations/${org.id}?tab=members`, {
    waitUntil: "domcontentloaded",
  });
  await page.waitForSelector("[data-invitation-queue]", { timeout: 20000 });
  const queueRow = await page.locator(`[data-queue-row="${panelAddress}"]`).count();
  const queueText = (await page
    .locator("[data-invitation-queue]")
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  step("browser-queue", { rowShown: queueRow > 0, text: queueText.slice(0, 140) });
  expect(queueRow > 0, "the queue panel must show the row the API queued");
  expect(
    /owner|approval|release/i.test(queueText),
    "the panel must explain what it is, not just list rows",
  );
  await page.screenshot({ path: path.join(OUT, "queue.png") });

  // The release button must be a real button with the row's own address, and the tab bar must
  // carry the Audit entry.
  const release = page.locator(`[data-queue-release="${panelAddress}"]`);
  const revoke = page.locator(`[data-queue-revoke="${panelAddress}"]`);
  step("browser-queue-actions", { release: await release.count(), revoke: await revoke.count() });
  expect((await release.count()) > 0, "the queue must offer a release");
  expect((await revoke.count()) > 0, "the queue must offer a revoke");

  // The Audit tab.
  await page.goto(`${ADMIN}/organizations/${org.id}?tab=audit`, {
    waitUntil: "domcontentloaded",
  });
  await page.waitForSelector("[data-audit-filters]", { timeout: 20000 });
  await page.waitForSelector("[data-audit-row]", { timeout: 20000 });
  const rows = await page.locator("[data-audit-row]").count();
  const count = (await page
    .locator("[data-audit-count]")
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  const optionCount = await page.locator("[data-audit-action-filter] option").count();
  step("browser-audit", { rows, count, optionCount });
  expect(rows > 0, "the Audit tab must list this tenant's rows");
  expect(optionCount > 1, "the action filter must be built from the tenant's own rows");
  await page.screenshot({ path: path.join(OUT, "audit.png") });

  // Narrow it and confirm the count moves with the rows.
  const firstAction = await page
    .locator("[data-audit-action-filter] option")
    .nth(1)
    .getAttribute("value");
  await page.locator("[data-audit-action-filter]").first().selectOption(firstAction);
  await page.waitForTimeout(1500);
  const narrowedRows = await page.locator("[data-audit-row]").count();
  const narrowedCount = (await page
    .locator("[data-audit-count]")
    .first()
    .innerText()
    .catch(() => "")).replace(/\s+/g, " ");
  step("browser-audit-filter", {
    action: firstAction,
    rows: narrowedRows,
    count: narrowedCount,
  });
  expect(narrowedRows <= rows, "narrowing must not show more rows");
  await page.screenshot({ path: path.join(OUT, "audit-filtered.png") });

  // 390x844: the tab has to stay readable on a phone.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(600);
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  step("browser-audit-mobile", { horizontalOverflow: overflow });
  expect(overflow <= 2, `the Audit tab must not overflow at 390px, overflow=${overflow}px`);
  await page.screenshot({ path: path.join(OUT, "audit-mobile.png") });

  step("console", { errors: consoleErrors.length, sample: consoleErrors.slice(0, 3) });
  expect(consoleErrors.length === 0, `the screens must log no console error: ${consoleErrors.slice(0, 2)}`);

  // Leave the tenant as the probe found it: the policy back to owner_approval, the two
  // invitations revoked. A probe that leaves a live invitation behind is a probe that breaks the
  // next one.
  for (const address of [closedAddress, queueAddress, panelAddress]) {
    const invitations = await api(`/api/v1/organizations/${org.id}/invitations`, { cookie });
    const row = (invitations.body?.invitations || []).find((item) => item.email === address);
    if (row) {
      await api(`/api/v1/organizations/${org.id}/invitations/${row.id}`, {
        method: "DELETE",
        cookie,
      });
    }
  }
  await setPolicy("owner_approval");
  step("cleanup", { ok: true });

  await browser.close();

  fs.writeFileSync(
    path.join(OUT, "probe.json"),
    JSON.stringify({ steps, failures, at: new Date().toISOString() }, null, 2),
  );
  console.log(`PROBE_FAILURES=${failures} PROBE_STEPS=${steps.length}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`[probe] FATAL ${error && error.stack ? error.stack : error}`);
  process.exit(2);
});
