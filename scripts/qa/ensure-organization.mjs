#!/usr/bin/env node
/**
 * Seed the QA database's tenant, without a browser (the harness's precondition step).
 *
 * ## Why this exists
 *
 * `run.sh` resets the QA database on every pass and then refuses to walk unless an organization
 * exists — a correct guard, added because a pass over an installation with no tenant answers every
 * org-scoped read with `403` and still writes a complete, green-looking report. But the only thing
 * that created one was the browser's first-run wizard, so on a machine where the wizard does not
 * reach its submit (the race this branch's `wizard-gate.test.cjs` now guards, and a cold `next dev`
 * makes worse) **the guard could never be satisfied** and every pass aborted before measuring
 * anything. A precondition with no way to be met is a harness that only ever fails.
 *
 * So the tenant is created here, over the API, with the SAME endpoints the wizard calls — not by
 * inserting rows. Seeding by SQL would satisfy the count query while leaving `onboarding_state`,
 * the membership row and the site's own row unwritten, and every one of those is what an
 * org-scoped read joins against: the pass would then measure screens that answer 403 and call it
 * the product's answer.
 *
 * ## Idempotent, and honest when it cannot seed
 *
 * Re-running against a database that already has an organization exits 0 with a line saying so. A
 * non-zero exit here would abort a pass that is already in a perfectly good state, and the message
 * names the real condition (a refused write) rather than exiting on a count it merely observed.
 */
import { readFileSync } from "node:fs";

const api = process.env.QA_API_URL || "http://127.0.0.1:18085";
const creds = {
  display_name: process.env.QA_OWNER_NAME || "QA Owner",
  email: process.env.QA_OWNER_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_OWNER_PASSWORD || "OmnionQa-Passw0rd-2026!",
  org: process.env.QA_ORG_NAME || "QA Organization",
  orgSlug: process.env.QA_ORG_SLUG || "qa-org",
  site: process.env.QA_SITE_NAME || "QA Site",
  siteKey: process.env.QA_SITE_KEY || "main",
  domain: process.env.QA_SITE_DOMAIN || "qa.omnion.test",
};

/** One request, with the session and CSRF cookies the API hands back on the owner POST. */
async function call(path, init = {}) {
  const response = await fetch(`${api}${path}`, {
    ...init,
    headers: {
      accept: "application/json",
      ...(typeof init.body === "string" ? { "content-type": "application/json" } : {}),
      ...(init.headers ?? {}),
    },
  });
  const setCookie = response.headers.getSetCookie?.() ?? [];
  const text = await response.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = { raw: text.slice(0, 300) };
  }
  return { status: response.status, body, setCookie };
}

/** The CSRF token the owner POST set as a readable cookie, for the writes that follow. */
function csrfFrom(setCookie) {
  for (const cookie of setCookie) {
    const match = /omnion_csrf=([^;]+)/.exec(cookie);
    if (match) return decodeURIComponent(match[1]);
  }
  return null;
}

/**
 * How far the first run has come, as the API reports it.
 *
 * `steps` is the shape, NOT a `step` string. The first version read `body.step` and compared it
 * against the words "organization"/"site"/"theme"/"complete" — **a field the status body does not
 * have**. So every comparison was false, the "an owner already exists" guard was true on a database
 * with no owner, the organization POST was gated behind a step name that never matched, and the
 * seeder went straight to creating a site on an installation that still needed an organization. The
 * API answered `409 setup_incomplete: the setup still needs: organization`, which reads like a
 * product refusal and was a read of a field that was never there.
 *
 * A comparison against a field that cannot exist always takes the same branch, and WHICH branch
 * that is depends entirely on what the unreachable branch did — here, on writing to an API in an
 * order it refuses.
 */
function stepsOf(body) {
  const steps = body?.steps ?? {};
  return {
    owner: Boolean(steps.owner),
    organization: Boolean(steps.organization),
    site: Boolean(steps.site),
    theme: Boolean(steps.theme),
    ai: Boolean(steps.ai),
  };
}

async function main() {
  const status = await call("/api/v1/onboarding");
  let steps = stepsOf(status.body);
  console.log(`[qa-seed] onboarding steps: ${JSON.stringify(steps)}`);

  // Declared out here because the branch below only runs when there is no owner yet, and the
  // cookie jar it returns is what every later write authenticates with.
  let ownerCookies = [];
  if (!steps.owner) {
    const owner = await call("/api/v1/onboarding/owner", {
      method: "POST",
      body: JSON.stringify({
        display_name: creds.display_name,
        email: creds.email,
        password: creds.password,
      }),
    });
    if (owner.status !== 201 && owner.status !== 200) {
      console.error(
        `[qa-seed] the owner could not be created (${owner.status}): ${JSON.stringify(owner.body).slice(0, 300)}`,
      );
      process.exit(1);
    }
    ownerCookies = owner.setCookie;
    if (ownerCookies.length === 0) {
      console.error("[qa-seed] the owner was created but the API set no session cookie");
      process.exit(1);
    }
    console.log(`[qa-seed] owner created and signed in (${owner.status})`);
  } else {
    console.log("[qa-seed] an owner already exists — skipping the owner POST");
  }

  // The writes below need the session cookie the owner POST handed back. `cookies` is assigned in
  // BOTH branches: the first version only assigned it in the sign-in branch and then set it to
  // `[]` in the else — so a fresh database (the normal case, since run.sh resets before every
  // pass) signed the owner in, dropped the session on the floor, and the next write answered 401
  // `unauthenticated`. The status was reported honestly, which is the only reason this was visible
  // at all; the message read like a product refusal and was a missing assignment.
  let cookies;
  if (steps.owner) {
    const signIn = await call("/api/v1/auth/login", {
      method: "POST",
      body: JSON.stringify({ email: creds.email, password: creds.password }),
    });
    if (signIn.status !== 200) {
      console.error(
        `[qa-seed] the existing owner could not sign in (${signIn.status}): ${JSON.stringify(signIn.body).slice(0, 200)}`,
      );
      process.exit(1);
    }
    cookies = signIn.setCookie;
    console.log("[qa-seed] signed in as the existing owner");
  } else {
    cookies = ownerCookies;
    console.log("[qa-seed] using the session the owner POST returned");
  }

  // An empty cookie header is not a thing to send: the API would answer 401 and the message would
  // name authentication rather than the missing credential.
  if (!cookies || cookies.length === 0) {
    console.error("[qa-seed] no session cookie was obtained, so no further write can succeed");
    process.exit(1);
  }

  const cookieHeader = cookies.map((cookie) => cookie.split(";")[0]).join("; ");
  const csrf = csrfFrom(cookies) ?? "";

  const write = (path, body) =>
    call(path, {
      method: "POST",
      body: JSON.stringify(body),
      headers: { cookie: cookieHeader, "x-csrf-token": csrf },
    });

  const afterOwner = stepsOf((await call("/api/v1/onboarding")).body);
  if (!afterOwner.organization) {
    const org = await write("/api/v1/onboarding/organization", {
      name: creds.org,
      slug: creds.orgSlug,
    });
    if (org.status !== 200 && org.status !== 201) {
      console.error(
        `[qa-seed] the organization could not be created (${org.status}): ${JSON.stringify(org.body).slice(0, 300)}`,
      );
      process.exit(1);
    }
    console.log("[qa-seed] organization created");
  }

  const afterOrg = stepsOf((await call("/api/v1/onboarding")).body);
  if (!afterOrg.site) {
    const site = await write("/api/v1/onboarding/site", {
      name: creds.site,
      key: creds.siteKey,
      domain: creds.domain,
    });
    if (site.status !== 200 && site.status !== 201) {
      console.error(
        `[qa-seed] the site could not be created (${site.status}): ${JSON.stringify(site.body).slice(0, 300)}`,
      );
      process.exit(1);
    }
    console.log("[qa-seed] site created");
  }

  const final = stepsOf((await call("/api/v1/onboarding")).body);
  console.log(`[qa-seed] done — steps are now ${JSON.stringify(final)}`);
  // The whole point of the seed is a tenant an org-scoped read will accept, so the assertion is the
  // two booleans the walk's screens join against — not "the script finished".
  if (!final.organization || !final.site) {
    console.error(
      `[qa-seed] FATAL: the walk needs an organization AND a site; got ${JSON.stringify(final)}`,
    );
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(`[qa-seed] failed: ${error?.message ?? error}`);
  process.exit(1);
});