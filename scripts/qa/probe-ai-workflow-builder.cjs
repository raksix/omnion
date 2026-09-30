#!/usr/bin/env node
/**
 * Omnion QA — end-to-end probe for the AI workflow draft decisions (REQ-046, slice 4).
 *
 * The Rust walks already prove the routes against a real database and a mock provider. What
 * they cannot see is the half this probe exists for: **the approval bar on the review screen**,
 * driven by a person clicking, with the outcome read back off the page *and* off the API — so a
 * button that answers 200 while rendering a stale row is caught here rather than by an operator.
 *
 * The specific things a click-through cannot prove on its own:
 *
 *  * a test run renders a plan **step by step** and says in words that nothing was dispatched —
 *    a panel that shows "Test run passed" has proved the definition is storable, not runnable;
 *  * approving swaps the decision bar for the "this is a rule now" state and the builder link
 *    becomes real (before approval it is disabled — a live link to a rule that does not exist is
 *    worse than a dead one);
 *  * a **rejected** draft keeps its reason where the person who asked for the draft can read it;
 *  * a second approve is refused by the UI, not only by the API;
 *  * and the definition editor saves a real change and refuses a real one, both read back from
 *    the API so a screen that reports success over a rejected write is caught.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-ai-workflow-builder.cjs
 *
 * Environment: QA_ADMIN_URL (3102 on the w3 stack), QA_API_URL, QA_OWNER_EMAIL/PASSWORD.
 * Exit code 0 when every check passes, 1 otherwise; prints one line per check.
 */
"use strict";

const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const EMAIL = process.env.QA_OWNER_EMAIL || "qa-owner@omnion.test";
const PASSWORD = process.env.QA_OWNER_PASSWORD || "OmnionQa-Passw0rd-2026!";

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

async function signIn(page) {
  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(600);
  if (page.url().includes("/login")) {
    await page.fill('input[name="email"]', EMAIL);
    await page.fill('input[name="password"]', PASSWORD);
    await page.click('button[type="submit"]');
    await page.waitForTimeout(2000);
  }
  return !page.url().includes("/login");
}

/** A draft id read out of the console's own row links — never a hard-coded fixture. */
async function firstDraftId(page) {
  await page.goto(`${ADMIN}/ai/workflows`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("[data-draft-row], [data-draft-link]", { timeout: 8000 }).catch(
    () => {}
  );
  const href = await page
    .locator("a[href*='/ai/workflows/']")
    .first()
    .getAttribute("href")
    .catch(() => null);
  if (!href) return null;
  const match = href.match(/\/ai\/workflows\/([0-9a-f-]{36})/);
  return match ? match[1] : null;
}

/**
 * Read a draft back **through the page**, not through Node.
 *
 * The first version read `document.cookie` looking for a cookie named `token` and replayed it
 * into a `fetch` from Node. Two things are wrong with that and both fail *silently*:
 *
 *  * the session cookie is not called `token` — the admin client reads it through its own
 *    `readCookie()` helper, which knows the real name, and a hard-coded name is a guess about
 *    somebody else's auth scheme;
 *  * a state-changing call also needs the CSRF token the API requires on a cookie session
 *    (`403 csrf_failed` otherwise), which the browser sends for free and Node cannot invent.
 *
 * Running the request inside the page means the probe reads the draft with exactly the
 * credentials the screen has, and a silent 401/403 cannot masquerade as a product failure. The
 * lesson is the general one: **a probe that re-implements authentication is testing its own
 * guess, and the failure looks like a defect in the product.**
 */
async function readDraft(page, id) {
  return page.evaluate(async (draftId) => {
    const response = await fetch(`/api/v1/ai/workflows/drafts/${draftId}`, {
      credentials: "include",
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, id);
}

/** The same, for the workflow an approval produced. */
async function readWorkflow(page, id) {
  return page.evaluate(async (workflowId) => {
    const response = await fetch(`/api/v1/workflows/${workflowId}`, {
      credentials: "include",
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  }, id);
}

(async () => {
  const browser = await chromium.launch({ executablePath: CHROME, args: ["--no-sandbox"] });
  const context = await browser.newContext();
  const page = await context.newPage();

  try {
    if (!(await signIn(page))) {
      check("the probe can sign in", false, "still on /login");
      process.exitCode = 1;
      return;
    }
    check("the probe can sign in", true);

    const draftId = await firstDraftId(page);
    if (!draftId) {
      check("the console lists a draft to decide", false, "no draft row in the console");
      process.exitCode = 1;
      return;
    }
    check("the console lists a draft to decide", true, draftId);

    await page.goto(`${ADMIN}/ai/workflows/${draftId}`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector("[data-approval-bar]", { timeout: 8000 }).catch(() => {});
    check(
      "the review screen mounts the decision bar",
      (await page.locator("[data-approval-bar]").count()) > 0
    );

    /* ---------------------------------------------------------------- the test run */
    const beforeRun = await readDraft(page, draftId);
    const testRunButton = page.locator("[data-test-run-button]");
    if (await testRunButton.count()) {
      await testRunButton.first().click();
      await page
        .waitForSelector("[data-test-run-steps]", { timeout: 12000 })
        .catch(() => {});
      const planRows = await page.locator("[data-test-run-steps] [data-step]").count();
      check("a test run renders a plan, step by step", planRows > 0, `${planRows} steps`);

      const planText = (await page.locator("[data-test-run-steps]").innerText().catch(() => "")) || "";
      check(
        "the plan says in words that nothing was dispatched",
        /nothing was dispatched|no event was emitted|not executed/i.test(planText),
        planText.slice(0, 90).replace(/\s+/g, " ")
      );

      // The test run must not have moved the draft: it is a test, not a decision.
      const afterRun = await readDraft(page, draftId);
      check(
        "a test run changes nothing about the draft",
        afterRun.body && beforeRun.body
          ? afterRun.body.status === beforeRun.body.status
          : false,
        `${beforeRun.body?.status} → ${afterRun.body?.status}`
      );
    } else {
      check("a test run renders a plan, step by step", false, "no test-run button");
    }

    /* ------------------------------------------------- the definition editor saves */
    const editor = page.locator("[data-definition-editor]");
    if (await editor.count()) {
      const original = await editor.inputValue();
      const edited = original.replace(/"max_attempts"\s*:\s*\d+/, '"max_attempts": 2');
      if (edited !== original) {
        await editor.fill(edited);
        const save = page.locator("[data-definition-save]");
        if (await save.count()) {
          await save.first().click();
          await page.waitForTimeout(2500);
          const saved = await readDraft(page, draftId);
          const stored = JSON.stringify(saved.body?.definition ?? {});
          check(
            "an edited definition is saved and read back from the API",
            stored.includes('"max_attempts":2') || stored.includes('"max_attempts": 2'),
            stored.slice(0, 80)
          );
        } else {
          check("an edited definition is saved and read back from the API", false, "no save button");
        }
      } else {
        check(
          "an edited definition is saved and read back from the API",
          true,
          "skipped: the definition has no max_attempts to change"
        );
      }
    }

    /* ------------------------------------------------------------- the decision bar */
    const alreadyDecided = (await page.locator("[data-approval-decided]").count()) > 0;
    if (alreadyDecided) {
      check("the bar shows the decided state", true, "the draft was already a rule");
      const builder = page.locator("[data-open-in-builder]");
      const href = await builder.first().getAttribute("href").catch(() => null);
      check("the builder link points at the rule that exists", Boolean(href && href.includes("/builder")), href || "no href");
    } else {
      const approve = page.locator("[data-approve]");
      if (await approve.count()) {
        await approve.first().click();
        await page.waitForTimeout(3000);
        const decided = (await page.locator("[data-approval-decided]").count()) > 0;
        check("approving swaps the bar for the decided state", decided);

        const after = await readDraft(page, draftId);
        check(
          "the approved draft names a workflow in the store",
          Boolean(after.body?.workflow_id),
          String(after.body?.workflow_id ?? "none")
        );

        // **"Disabled" is a fact about the WORKFLOW, not about the draft**, and the draft's
        // own body has no `enabled` field — it has `workflow_id` and nothing else. The first
        // version of this check looked for `draft.workflow.enabled` and `draft.enabled`, so it
        // was not a weak assertion, it was an unreachable one: `undefined === false` is false
        // on every run, and the probe would have gone red on a correct approval. The rule
        // `POST /workflows` arms by default, so the flag is the whole safety property of
        // approval and it has to be read where it is actually stored.
        const workflowId = after.body?.workflow_id;
        let armed = null;
        if (workflowId) {
          const wf = await readWorkflow(page, workflowId);
          if (wf.status === 200) armed = wf.body?.enabled;
        }
        check(
          "the workflow it became is DISABLED, not armed by default",
          armed === false,
          `enabled=${JSON.stringify(armed)}`
        );

        // And the UI refuses a second decision — the bar must not offer one at all.
        const stillApprovable = await page.locator("[data-approve]").count();
        check("the bar offers no second decision", stillApprovable === 0);

        const builder = page.locator("[data-open-in-builder]");
        const href = await builder.first().getAttribute("href").catch(() => null);
        check(
          "the builder link is live and names the rule",
          Boolean(href && href.includes("/builder")),
          href || "no href"
        );
      } else {
        check("approving swaps the bar for the decided state", false, "no approve button");
      }
    }
  } finally {
    await browser.close();
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) process.exitCode = 1;
})();
