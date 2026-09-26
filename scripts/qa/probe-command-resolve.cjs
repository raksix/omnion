#!/usr/bin/env node
/**
 * Omnion QA — focused probe for the palette's natural-language resolution (REQ-032, slice 4).
 *
 * The walkthrough types a phrase and lands on a screen; this probe proves what a click-through
 * cannot see on its own: that the card shows the *interpretation* before anything runs and that
 * nothing has run while it is up (the audit trail is read back through the API), that a reading
 * with nowhere to go offers alternatives instead of a Run, that a phrase nobody can read never
 * produces a Run at all, that a reading of an action still goes through the platform's own
 * confirmation, that a slow or failing reader leaves the results untouched, and that a connected
 * model that does not answer in time degrades to the platform's own grammar.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-command-resolve.cjs
 *
 * Exit code 0 when every check passes, 1 otherwise; prints one line per check.
 */
"use strict";

const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const EMAIL = process.env.QA_OWNER_EMAIL || "qa-owner@omnion.test";
const PASSWORD = process.env.QA_OWNER_PASSWORD || "OmnionQa-Passw0rd-2026!";

/** The phrase the request itself uses as its example. */
const PHRASE = "Open Mehmet's last 10 tickets";

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

async function signIn(page) {
  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(500);
  if (page.url().includes("/login")) {
    await page.fill('input[name="email"]', EMAIL);
    await page.fill('input[name="password"]', PASSWORD);
    await page.click('button[type="submit"]');
    await page.waitForTimeout(1600);
  }
  return !page.url().includes("/login");
}

/**
 * Open the palette from the keyboard and return whether it is there.
 *
 * The press is retried: the panel is a development server, and a keystroke that lands before the
 * shell has hydrated is a keystroke nobody heard — a probe that gives up on the first one reports
 * a product defect where there is only a slow compile.
 */
async function openPalette(page, attempts = 4) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    if ((await page.locator("[data-search-palette]").count()) > 0) {
      return true;
    }
    await page.keyboard.press("Control+K");
    await page
      .waitForSelector("[data-search-palette]", { timeout: 2500 })
      .catch(() => {});
    await page.waitForTimeout(400);
    if ((await page.locator("[data-search-palette]").count()) > 0) {
      return true;
    }
  }
  return false;
}

/** Type a phrase into the palette's box. */
async function type(page, text) {
  const input = page.locator("[data-palette-input]").first();
  await input.fill(text);
  return input;
}

/** Wait for the card to settle (ready or failed) and return its state. */
async function waitForCard(page, timeout = 12000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const state = await page
      .locator("[data-palette-ai]")
      .first()
      .getAttribute("data-palette-ai-state")
      .catch(() => null);
    if (state === "ready" || state === "failed") {
      return state;
    }
    await page.waitForTimeout(200);
  }
  return "timeout";
}

const cardText = (page) =>
  page
    .locator("[data-palette-ai]")
    .first()
    .innerText()
    .then((text) => text.replace(/\s+/g, " ").trim())
    .catch(() => "");

/** The `command.run` entries of the audit trail, newest first. */
async function auditRuns(page, action = "command.run") {
  const body = await page
    .evaluate(
      ([wanted]) =>
        fetch("/api/v1/iam/audit?limit=50", { credentials: "same-origin" }).then((response) =>
          response.ok ? response.json() : { entries: [] },
        ),
      [action],
    )
    .catch(() => null);
  const entries = Array.isArray(body?.entries) ? body.entries : [];
  return entries.filter((entry) => entry.action === action);
}

async function main() {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const consoleErrors = [];
  const serverErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") {
      consoleErrors.push({ text: msg.text().slice(0, 120), url: msg.location?.().url ?? "" });
    }
  });
  page.on("response", (response) => {
    if (response.status() >= 500) {
      serverErrors.push(`${response.status()} ${response.url()}`);
    }
  });

  check("signed in", await signIn(page), page.url());

  // ---------------------------------------------------------------- the reading, before anything runs
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  const runsBefore = await auditRuns(page);

  check("the palette opens", await openPalette(page));
  await type(page, PHRASE);
  const state = await waitForCard(page);
  check("the card shows a reading for the request's own phrase", state === "ready", state);

  const text = await cardText(page);
  check(
    "the reading names the domain, the filter, the count and the order",
    /tickets/i.test(text) && /assignee: mehmet/i.test(text) && /last 10/i.test(text) && /newest first/i.test(text),
    text.slice(0, 200),
  );
  check(
    "the card says where the reading came from",
    /read on the server|read locally|read by/i.test(text),
    text.slice(0, 200),
  );
  const confidence = await page
    .locator("[data-palette-ai-confidence]")
    .first()
    .getAttribute("data-palette-ai-confidence")
    .catch(() => null);
  check(
    "its confidence is stated",
    ["high", "medium", "low"].includes(confidence ?? ""),
    String(confidence),
  );

  // Nothing executes by being read: no run, no navigation, the palette still open.
  const runsAfterRead = await auditRuns(page);
  check(
    "reading a phrase runs nothing",
    runsAfterRead.length === runsBefore.length,
    `${runsBefore.length} → ${runsAfterRead.length} command.run entries`,
  );
  check(
    "the reading does not navigate on its own",
    new URL(page.url()).pathname === "/" &&
      (await page.locator("[data-search-palette]").count()) === 1,
    page.url(),
  );

  // No tickets list exists yet, so the reading offers alternatives rather than a Run.
  check(
    "a reading with nowhere to go offers no Run",
    (await page.locator("[data-palette-ai-run]").count()) === 0,
  );
  const alternatives = await page.locator("[data-palette-ai-alternative]").count();
  check(
    "and offers what else the words could mean instead",
    alternatives >= 1,
    `${alternatives} alternative(s)`,
  );
  check(
    "the card says why in plain words",
    /not something this panel indexes/i.test(text),
    text.slice(0, 240),
  );

  // ---------------------------------------------------------------- Edit as search
  await page.locator("[data-palette-ai-edit]").first().click();
  await page.waitForTimeout(1500);
  const edited = new URL(page.url());
  check(
    "Edit as search lands on the results screen with the words",
    edited.pathname === "/search" &&
      (edited.searchParams.get("q") ?? "").includes("tickets") &&
      (edited.searchParams.get("q") ?? "").toLowerCase().includes("mehmet"),
    page.url(),
  );
  check(
    "and carries the order the phrase asked for",
    edited.searchParams.get("sort") === "newest",
    page.url(),
  );
  check(
    "the palette closed behind it",
    (await page.locator("[data-search-palette]").count()) === 0,
  );

  // ---------------------------------------------------------------- a runnable reading really runs
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  await openPalette(page);
  await type(page, "show me the newest pages");
  check("a reading onto a domain the index has settles", (await waitForCard(page)) === "ready");
  const pagesText = await cardText(page);
  check(
    "the card proposes the search it read",
    /pages/i.test(pagesText) && /newest first/i.test(pagesText),
    pagesText.slice(0, 160),
  );
  check(
    "a runnable reading carries Run",
    (await page.locator("[data-palette-ai-run]").count()) === 1,
  );
  await page.locator("[data-palette-ai-run]").first().click();
  await page.waitForTimeout(1600);
  const ran = new URL(page.url());
  check(
    "Run lands on the matching list with its filters applied",
    ran.pathname === "/search" &&
      (ran.searchParams.get("type") ?? "") === "pages" &&
      ran.searchParams.get("sort") === "newest",
    page.url(),
  );
  const chips = await page.locator("[data-search-chips] [data-chip]").count();
  check("the results screen shows the filter as a chip", chips >= 1, `${chips} chip(s)`);

  // ---------------------------------------------------------------- a command reading
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  await openPalette(page);
  await type(page, "open media");
  check("a reading of a command settles", (await waitForCard(page)) === "ready");
  const commandCard = await cardText(page);
  check(
    "the card names the command it read",
    /open media/i.test(commandCard),
    commandCard.slice(0, 160),
  );
  await page.locator("[data-palette-ai-run]").first().click();
  await page.waitForTimeout(1600);
  check("Run opens that command's screen", page.url().includes("/media"), page.url());

  // ---------------------------------------------------------------- an action reading still asks first
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);
  const runsBeforeAction = await auditRuns(page);
  await openPalette(page);
  await type(page, "rebuild the search index");
  check("a reading of an action settles", (await waitForCard(page)) === "ready");
  await page.locator("[data-palette-ai-run]").first().click();
  await page.waitForTimeout(900);
  const asked = (await page.locator("[data-palette-confirm]").count()) === 1;
  check("running an action from a reading asks first", asked);
  const runsWhileAsking = await auditRuns(page);
  check(
    "and runs nothing while the question is open",
    runsWhileAsking.length === runsBeforeAction.length,
    `${runsBeforeAction.length} → ${runsWhileAsking.length} command.run entries`,
  );
  await page.locator("[data-palette-input]").press("Escape");
  await page.waitForTimeout(400);

  // ---------------------------------------------------------------- a phrase nobody can read
  await openPalette(page);
  await type(page, "zzqqxx");
  check("an unreadable phrase settles", (await waitForCard(page)) === "ready");
  const unclear = await cardText(page);
  check(
    "the card says it could not read it",
    /not sure what this means/i.test(unclear),
    unclear.slice(0, 160),
  );
  check(
    "a low-confidence phrase offers no Run at all",
    (await page.locator("[data-palette-ai-run]").count()) === 0,
  );
  const unclearAlternatives = await page.locator("[data-palette-ai-alternative]").count();
  check(
    "it offers alternatives instead",
    unclearAlternatives >= 1,
    `${unclearAlternatives} alternative(s)`,
  );
  const runsAfterUnclear = await auditRuns(page);
  check(
    "and still runs nothing",
    runsAfterUnclear.length === runsBeforeAction.length,
    `${runsAfterUnclear.length} command.run entries`,
  );

  // ---------------------------------------------------------------- a slow reader does not block
  // One handler at a time: a route left registered while another answers the same URL is a race,
  // and the point here is the palette, not the interception.
  const resolveUrl = "**/api/v1/command-center/resolve";

  await page.route(resolveUrl, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 3000));
    await route.continue().catch(() => {});
  });
  await type(page, "show me the newest pages");
  await page.waitForTimeout(700);
  const resolving = await page
    .locator("[data-palette-ai]")
    .first()
    .getAttribute("data-palette-ai-state")
    .catch(() => null);
  check("a slow reading shows a resolving state", resolving === "resolving", String(resolving));

  // Typing keeps working, and the results are not held hostage by the reading: while the request
  // is still in flight the box has already answered the question its own way (here: nothing
  // matched, which is its own state and not a spinner).
  await type(page, "show me the newest pages of the site");
  await page.waitForTimeout(1200);
  const typedWhileResolving = await page
    .locator("[data-palette-input]")
    .first()
    .inputValue()
    .catch(() => "");
  const answerSurfaces = await page
    .locator("[data-palette-empty], [data-palette-section], [data-palette-skeleton]")
    .count();
  const stillResolving = await page
    .locator("[data-palette-ai]")
    .first()
    .getAttribute("data-palette-ai-state")
    .catch(() => null);
  check(
    "typing keeps working while the reading is in flight",
    typedWhileResolving === "show me the newest pages of the site" &&
      answerSurfaces > 0 &&
      stillResolving === "resolving",
    `input kept, ${answerSurfaces} answer surface(s), card ${stillResolving}`,
  );
  await page.unroute(resolveUrl);
  check("the slow reading settles once it is allowed to", (await waitForCard(page)) === "ready");
  await page.waitForTimeout(300);

  // ---------------------------------------------------------------- a failing reader
  await page.route(resolveUrl, (route) =>
    route.fulfill({
      status: 503,
      contentType: "application/json",
      body: JSON.stringify({
        error: { code: "dependency_unavailable", message: "the reader is unavailable" },
      }),
    }),
  );
  await type(page, "show me the newest media files");
  check("a failing reading settles as failed", (await waitForCard(page)) === "failed");
  check(
    "the card offers a retry instead of a dead end",
    (await page.locator("[data-palette-ai-retry]").count()) === 1,
  );
  await page.unroute(resolveUrl);
  await page.waitForTimeout(300);
  await page.locator("[data-palette-ai-retry]").first().click();
  check("retrying reads the phrase again", (await waitForCard(page)) === "ready");
  await page.keyboard.press("Escape");
  await page.waitForTimeout(400);

  // ---------------------------------------------------------------- a model that does not answer
  // A provider that never answers, made the installation's default: the palette must degrade to its
  // own reading instead of waiting on it. The endpoint is a black hole (TEST-NET-1), so the read
  // ends on the resolver's own six-second bound rather than on an answer.
  //
  // The name is cleared first: a probe that crashed mid-way must not leave an installation whose
  // every reading times out (the pass this probe belongs to runs on a disposable database, but a
  // rerun against the same stack must still start from the same state).
  const leftovers = await page.evaluate(async () => {
    const listed = await fetch("/api/v1/ai/providers", { credentials: "same-origin" })
      .then((answer) => (answer.ok ? answer.json() : { providers: [] }))
      .catch(() => ({ providers: [] }));
    const stale = (listed.providers ?? []).filter((row) => row.name === "QA black hole");
    for (const row of stale) {
      await fetch(`/api/v1/ai/providers/${row.id}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
    }
    return stale.length;
  });
  if (leftovers > 0) {
    console.log(`note  cleared ${leftovers} leftover test provider(s) from an earlier run`);
  }

  const provider = await page.evaluate(async () => {
    const response = await fetch("/api/v1/ai/providers", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        name: "QA black hole",
        base_url: "http://192.0.2.1:81/v1",
        api_key: "qa-unused",
        is_default: true,
        models: [{ key: "qa-slow", supports_streaming: true }],
      }),
    });
    const body = await response.json().catch(() => null);
    const created = { status: response.status, id: body?.id ?? null, modelId: null };

    if (created.id) {
      const listed = await fetch(`/api/v1/ai/models?provider_id=${created.id}`, {
        credentials: "same-origin",
      })
        .then((answer) => (answer.ok ? answer.json() : { models: [] }))
        .catch(() => ({ models: [] }));
      const model = (listed.models ?? []).find((row) => row.provider_id === created.id);
      if (model) {
        await fetch(`/api/v1/ai/models/${model.id}`, {
          method: "PATCH",
          credentials: "same-origin",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ is_default: true }),
        });
        created.modelId = model.id;
      }
    }
    return created;
  });
  check(
    "a default model can be connected for the test",
    provider.status === 201 && provider.modelId !== null,
    `provider ${provider.status}, model ${provider.modelId ?? "none"}`,
  );

  if (provider.id) {
    await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(900);
    await openPalette(page);
    const started = Date.now();
    await type(page, "show me the newest pages");
    const settled = await waitForCard(page, 20000);
    const waited = Date.now() - started;
    check(
      "a model that never answers degrades instead of hanging the box",
      settled === "ready" && waited < 18000,
      `${settled} after ${waited} ms`,
    );
    const degraded = await cardText(page);
    check(
      "and says the reading is local",
      /read locally/i.test(degraded) && /pages/i.test(degraded),
      degraded.slice(0, 200),
    );
    const resolutions = await auditRuns(page, "command.resolve");
    check(
      "the attempt is audited with the interpreted intent, not with content rows",
      resolutions.length >= 1 &&
        typeof resolutions[0]?.metadata?.query === "string" &&
        resolutions[0]?.metadata?.status === "timeout",
      JSON.stringify(resolutions[0]?.metadata ?? null).slice(0, 160),
    );
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);
    const removed = await page.evaluate(async (id) => {
      const response = await fetch(`/api/v1/ai/providers/${id}`, {
        method: "DELETE",
        credentials: "same-origin",
      });
      return response.status;
    }, provider.id);
    check("the test model is removed again", removed === 204, String(removed));
  }

  // ---------------------------------------------------------------- console hygiene
  check(
    "no 5xx answers during the desktop pass",
    serverErrors.filter((line) => !line.includes("/command-center/resolve")).length === 0,
    serverErrors.slice(0, 3).join(" | "),
  );
  const unexpected = consoleErrors.filter(
    (entry) => !/\/command-center\/resolve$/.test(entry.url),
  );
  check(
    "no console errors on the desktop pass, apart from the failure this probe asks for",
    unexpected.length === 0,
    unexpected
      .slice(0, 2)
      .map((entry) => `${entry.text} @ ${entry.url}`)
      .join(" | "),
  );

  // ---------------------------------------------------------------- mobile
  const mobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    deviceScaleFactor: 2,
  });
  const mpage = await mobile.newPage();
  check("signed in on mobile", await signIn(mpage), mpage.url());
  await mpage.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await mpage.waitForTimeout(900);
  await mpage.click("[data-search-box]");
  await mpage.waitForTimeout(700);
  await type(mpage, "newest pages");
  const mobileState = await waitForCard(mpage);
  check("the card shows on a phone", mobileState === "ready", mobileState);
  const widths = await mpage
    .locator("[data-palette-ai]")
    .first()
    .evaluate((card) => {
      const box = card.getBoundingClientRect();
      return { width: Math.round(box.width), viewport: window.innerWidth };
    })
    .catch(() => null);
  check(
    "it spans the sheet rather than floating in a corner",
    Boolean(widths) && widths.width > widths.viewport * 0.7,
    JSON.stringify(widths),
  );
  const targets = await mpage
    .locator("[data-palette-ai-run], [data-palette-ai-edit], [data-palette-ai-alternative]")
    .evaluateAll((nodes) =>
      nodes.map((node) => Math.round(node.getBoundingClientRect().height)),
    )
    .catch(() => []);
  check(
    "its controls are 44px on a phone",
    targets.length >= 2 && targets.every((height) => height >= 44),
    JSON.stringify(targets),
  );

  await browser.close();

  const failed = results.filter((row) => !row.ok);
  console.log(
    `\n${results.length - failed.length}/${results.length} checks passed` +
      (failed.length ? ` — failing: ${failed.map((row) => row.name).join(", ")}` : ""),
  );
  process.exit(failed.length ? 1 : 0);
}

main().catch((error) => {
  console.error(`probe crashed: ${error.message}`);
  process.exit(1);
});
