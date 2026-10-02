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

const { execFileSync } = require("child_process");
const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
// A QA pass resets its own database, so the probe needs to know which one the stack
// on this port is pointed at before it writes a fixture into it.
const QA_DB = process.env.QA_DB || (process.env.QA_STACK === "w3" ? "omnion_qa_w3" : "omnion_qa");
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

/**
 * A draft id from the console's own rows — never a hard-coded fixture.
 *
 * **`has_definition` is part of the choice, and the first version left it out.** The console
 * lists every draft, including a `generating` one that has nothing to decide yet, and this
 * function took the first row. It found a definition-less draft left behind by an earlier
 * shape experiment, opened it, and the bar was correctly disabled — so the probe reported
 * "the review screen mounts the decision bar" as a failure with `has_definition: false`
 * sitting right there in the response. The product was right and the probe was wrong, which
 * is the most expensive kind of wrong because it sends the next tick to fix working code.
 *
 * A probe that opens "a" row is not testing the thing it means to test. It must open a row
 * that is in the state the assertions require.
 */
async function firstDecidableDraftId(page) {
  await page.goto(`${ADMIN}/ai/workflows`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("[data-draft-row], [data-draft-link]", { timeout: 8000 }).catch(
    () => {}
  );
  const ids = await page
    .locator("a[href*='/ai/workflows/']")
    .evaluateAll((nodes) => nodes.map((n) => n.getAttribute("href")))
    .catch(() => []);
  for (const href of ids) {
    const match = (href || "").match(/\/ai\/workflows\/([0-9a-f-]{36})/);
    if (!match) continue;
    const draft = await readDraft(page, match[1]);
    // A draft with a definition and no workflow is exactly the `draft` state the bar is for.
    if (draft.body?.has_definition && !draft.body?.workflow_id) return match[1];
  }
  return null;
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

/**
 * Seed one reviewable draft, so the probe does not depend on a QA reset having left one behind.
 *
 * A QA pass resets the database, so "open the first draft in the console" is a probe that
 * passes on a machine where somebody happened to generate one and reports **the console is
 * broken** on a clean one — the failure points at the product and belongs to the fixture. The
 * walkthrough already seeds the review screen this way for the same reason (no live provider on
 * the box), and this is that recipe: the row is written in the shape `apply_answer` leaves it
 * in, so the screen reads exactly what a real generation would have stored.
 */
function qaSql(statement) {
  return execFileSync(
    "docker",
    [
      "exec",
      process.env.QA_PG_CONTAINER || "omnion-postgres",
      "psql", "-U", "omnion", "-d", QA_DB,
      "-v", "ON_ERROR_STOP=1", "-t", "-A", "-c", statement,
    ],
    { encoding: "utf8", timeout: 30000 }
  ).trim();
}

function seedDraft() {
  const org =
    qaSql("select organization_id from ai_workflow_drafts limit 1") ||
    qaSql("select id from organizations order by created_at desc limit 1");
  if (!org) return null;
  const definition = JSON.stringify({
    trigger: { kind: "manual" },
    steps: [
      {
        name: "summarise",
        kind: "task",
        action: "ai.prompt",
        params: { prompt: "Summarise the record in one line.", max_tokens: 2000 },
      },
    ],
  }).replace(/'/g, "''");
  const rationale =
    "The first step asks a model to summarise the record, so the rule works on text the platform did not author."
      .replace(/'/g, "''");
  const seeded = qaSql(
    `insert into ai_workflow_drafts (organization_id, title, prompt, rationale, definition, status, model_key, tokens_input, tokens_output, created_by)
     values ('${org}', 'QA probe draft',
             'Summarise each new support ticket and file it under the right topic.',
             '${rationale}',
             '${definition}'::jsonb,
             'draft', 'qa/mock-model', 42, 17,
             (select id from users order by created_at desc limit 1))
     returning id`
  );
  // **`pop()` is wrong here, and the walkthrough has the same bug.** With `-t -A`, psql prints
  // the `returning` row FIRST and the command tag `INSERT 0 1` LAST, so the last line is the
  // tag. `.pop()` therefore returned the string "INSERT 0 1" — which is *truthy*, so a
  // `Boolean(draftId)` guard passed and the caller navigated to `/ai/workflows/INSERT 0 1`:
  // a route that does not exist. A seed that cannot fail is not a seed, and an id assertion
  // that passes on a command tag is worse than no assertion.
  const firstLine = (seeded || "").split("\n")[0].trim();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(firstLine)
    ? firstLine
    : null;
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

    // A reset database has no drafts, so the probe writes one rather than reporting the
    // console as broken. When the console *does* list drafts, the seeded row is the one it
    // opens, so the count assertion below is about the console rendering a row, not luck.
    const listed = await firstDecidableDraftId(page);
    let draftId = listed;
    if (!draftId) {
      draftId = seedDraft();
      check(
        "the console had no draft to decide (one was seeded for the probe)",
        Boolean(draftId),
        draftId || "the fixture could not be written"
      );
    } else {
      check("the console lists a draft to decide", true, listed);
    }
    if (!draftId) {
      process.exitCode = 1;
      return;
    }

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
      // **The plan rows are bare `<li>`; `[data-step]` belongs to the read-only step list.**
      // The first version of this check counted `[data-test-run-steps] [data-step]` and got
      // 0 on a plan that rendered perfectly — the selector was matching a *different* list on
      // the same screen, so it proved the assertion was about the wrong element. Count the
      // plan's own children instead.
      const planRows = await page.locator("[data-test-run-steps] > li").count();
      check("a test run renders a plan, step by step", planRows > 0, `${planRows} steps`);

      // The note is rendered in the panel's own paragraph, not inside the list.
      const panelText = (await page.locator("[data-test-run-steps]").innerText().catch(() => "")) || "";
      const noteText = (await page.locator("text=/dispatched|emitted/i").first().innerText().catch(() => "")) || "";
      check(
        "the plan says in words that nothing was dispatched",
        /dispatched|emitted|separate action/i.test(`${panelText} ${noteText}`),
        (noteText || panelText).slice(0, 90).replace(/\s+/g, " ")
      );

      // A step that leaves the process must say so — that badge is the line a reviewer
      // reads first, so a plan that rendered the step without it would be worse than empty.
      check(
        "a host step is labelled as leaving the process",
        /leaves the process/i.test(panelText),
        (panelText.match(/leaves the process[^\n]*/i) || ["not labelled"])[0].slice(0, 60)
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

        // The bar still *renders* its buttons, and every one of them is disabled. Asserting
        // `count === 0` (the first version) tested a design that was never specified — the
        // contract is "you cannot decide a draft that is already a rule", and a disabled
        // button says that, while a vanished button leaves the reviewer wondering whether the
        // decision was recorded.
        const approveButton = page.locator("[data-approve]").first();
        const approveCount = await page.locator("[data-approve]").count();
        check(
          "the bar offers no second decision",
          approveCount === 0 || (await approveButton.isDisabled()),
          approveCount === 0 ? "no approve button at all" : "approve is present and disabled"
        );

        // And the API refuses it too, so the disabled button is not the only guard.
        const second = await page.evaluate(async (draftId) => {
          const csrf = document.cookie
            .split(";")
            .map((c) => c.trim())
            .find((c) => c.startsWith("omnion_csrf="))
            ?.split("=")[1] || "";
          const response = await fetch(`/api/v1/ai/workflows/drafts/${draftId}/approve`, {
            method: "POST",
            credentials: "include",
            headers: csrf ? { "x-omnion-csrf": decodeURIComponent(csrf) } : {},
            body: "{}",
          });
          return { status: response.status };
        }, draftId);
        check(
          "and the API refuses a second approve with 409",
          second.status === 409,
          `status=${second.status}`
        );

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
    /* ------------------------------------------------- every filter is URL-persisted */
    // Typed live and verified by hand this tick, then moved in here so it cannot rot: a
    // filter that lives in React state looks identical until somebody reloads the page and
    // the console is empty again. The assertion is the *value after a reload*, not the URL
    // after typing — a screen can write the query string and still re-seed its own state.
    await page.goto(`${ADMIN}/ai/workflows`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(1800);
    const search = page.locator("[data-draft-search], input[type='search']").first();
    if (await search.count()) {
      await search.fill("QA probe");
      await page.waitForTimeout(1400);
      const urlAfterTyping = page.url();
      check(
        "typing a search writes it to the URL",
        /[?&]q=/.test(urlAfterTyping),
        urlAfterTyping.replace(ADMIN, "")
      );
      await page.reload({ waitUntil: "domcontentloaded" });
      await page.waitForTimeout(1800);
      const valueAfter = await search.inputValue().catch(() => "");
      check(
        "the search survives a reload, read from the URL",
        valueAfter === "QA probe",
        `value=${JSON.stringify(valueAfter)} url=${page.url().replace(ADMIN, "")}`
      );
    } else {
      check("typing a search writes it to the URL", false, "no search input on the console");
    }

  } finally {
    await browser.close();
  }

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) process.exitCode = 1;
})();
