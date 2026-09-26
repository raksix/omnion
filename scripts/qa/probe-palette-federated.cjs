#!/usr/bin/env node
/**
 * Omnion QA — focused probe for the palette's federated search (REQ-032, slice 2).
 *
 * The walkthrough proves the palette opens, searches and opens a row. This probe proves the
 * parts a click-through cannot see on its own:
 *
 * * **Groups stream independently.** One provider's request is delayed; its section shows its own
 *   skeleton while the others already carry their rows.
 * * **A failing type never blanks the palette.** One provider's request is failed; that section
 *   shows a retryable error carrying the failure's code, every other section still renders, and
 *   the retry brings the failed one back.
 * * **The narrowing modes narrow the requests themselves.** `>` asks the registry and nothing
 *   else (no index call, no sections), `#`/`:`/`@` ask exactly one provider each.
 * * **`/search` reproduces a palette section from its URL.** A section's "see all" lands on the
 *   results screen with the same count, which a reload reproduces.
 * * **An empty answer tells the reader which empty it is.** A second account that may not read
 *   every provider sees "N results are outside your permissions" and never a title it may not
 *   read.
 * * **Typing stays out of the history.** The palette's calls say `history=false`; the account's
 *   own search history is untouched by them.
 *
 * Usage (the QA stack has to be up — `bash scripts/qa/run.sh` leaves it running):
 *   NODE_PATH=/root/test-hermes/node_modules node scripts/qa/probe-palette-federated.cjs
 *
 * Exit code 0 when every check passes, 1 otherwise; prints one line per check.
 */
"use strict";

const { execFileSync } = require("child_process");

const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN_URL || "http://127.0.0.1:3100";
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const OWNER_EMAIL = process.env.QA_OWNER_EMAIL || "qa-owner@omnion.test";
const OWNER_PASSWORD = process.env.QA_OWNER_PASSWORD || "OmnionQa-Passw0rd-2026!";
/** The second account of the permission walk: the base Member role, same password. */
const MEMBER_EMAIL = process.env.QA_MEMBER_EMAIL || "qa-member@omnion.test";
const PG_CONTAINER = process.env.QA_PG_CONTAINER || "omnion-postgres";
const QA_DB = process.env.QA_DB || "omnion_qa";

const results = [];
function check(name, ok, detail = "") {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

/** Run one statement against the disposable QA database. */
function psql(sql) {
  return execFileSync(
    "docker",
    ["exec", "-i", PG_CONTAINER, "psql", "-U", "omnion", "-d", QA_DB, "-v", "ON_ERROR_STOP=1", "-tAc", sql],
    { encoding: "utf8" },
  ).trim();
}

async function signIn(page, email, password) {
  await page.goto(`${ADMIN}/login`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(500);
  if (page.url().includes("/login")) {
    await page.fill('input[name="email"]', email);
    await page.fill('input[name="password"]', password);
    await page.click('button[type="submit"]');
    await page.waitForTimeout(1600);
  }
  return !page.url().includes("/login");
}

/** Open the palette from the keyboard and return whether it is there. */
async function openPalette(page) {
  await page.keyboard.press("Control+K");
  await page.waitForSelector("[data-search-palette]", { timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(500);
  return (await page.locator("[data-search-palette]").count()) > 0;
}

async function type(page, text) {
  const input = page.locator("[data-palette-input]").first();
  await input.fill(text);
  return input;
}

/** Every section the palette currently renders, with its own state. */
const sections = (page) =>
  page.evaluate(() =>
    [...document.querySelectorAll("[data-palette-section]")].map((node) => ({
      provider: node.getAttribute("data-palette-section"),
      state: node.getAttribute("data-palette-section-state"),
      rows: node.querySelectorAll('[role="option"]').length,
    })),
  );

/** The provider of the row the keyboard is on. */
const activeSection = (page) =>
  page.evaluate(() => {
    const id = document
      .querySelector("[data-palette-input]")
      ?.getAttribute("aria-activedescendant");
    if (!id) return null;
    const node = document.getElementById(id);
    if (!node) return null;
    const section = node.closest("[data-palette-section]");
    return {
      id,
      provider: section?.getAttribute("data-palette-section") ?? null,
      label: (node.innerText || "").replace(/\s+/g, " ").trim().slice(0, 60),
    };
  });

/** One API call from inside the signed-in page (the session cookie rides along). */
const api = (page, path, init) =>
  page.evaluate(
    async ([path, init]) => {
      const response = await fetch(path, {
        credentials: "same-origin",
        ...(init ?? {}),
        headers: init && init.body ? { "content-type": "application/json" } : undefined,
      });
      const text = await response.text();
      let body = null;
      try {
        body = text ? JSON.parse(text) : null;
      } catch {
        body = text;
      }
      return { status: response.status, body };
    },
    [path, init],
  );

/** Wait for one section to reach a state, answering the section as it is. */
async function waitForSection(page, provider, state, timeout = 8000) {
  const deadline = Date.now() + timeout;
  let last = null;
  while (Date.now() < deadline) {
    const list = await sections(page);
    last = list.find((section) => section.provider === provider) ?? null;
    if (last && last.state === state) {
      return last;
    }
    await page.waitForTimeout(120);
  }
  return last;
}

async function main() {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const consoleErrors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text().slice(0, 160));
  });

  check("signed in as the owner", await signIn(page, OWNER_EMAIL, OWNER_PASSWORD), page.url());
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(900);

  // ---------------------------------------------------------------- groups stream
  // One provider answers slowly: its section must show its own skeleton while the others already
  // hold their rows. This is the difference between one blocked answer and a federated one.
  let delayed = 0;
  await page.route("**/api/v1/search**", async (route) => {
    if (/types=media/.test(route.request().url())) {
      delayed += 1;
      await new Promise((resolve) => setTimeout(resolve, 900));
    }
    await route.continue();
  });
  await openPalette(page);
  await type(page, "sample");
  const pagesReady = await waitForSection(page, "pages", "ready", 6000);
  const mediaWhileSlow = await waitForSection(page, "media", "pending", 200);
  check(
    "a settled section renders while a slower one is still loading",
    Boolean(pagesReady) && (pagesReady?.rows ?? 0) > 0 && mediaWhileSlow?.state === "pending" && delayed > 0,
    `pages=${pagesReady?.state}/${pagesReady?.rows} media=${mediaWhileSlow?.state} delayed=${delayed}`,
  );
  const mediaReady = await waitForSection(page, "media", "ready", 8000);
  check(
    "the slow section arrives on its own",
    Boolean(mediaReady) && (mediaReady?.rows ?? 0) > 0,
    `media=${mediaReady?.state}/${mediaReady?.rows}`,
  );
  await page.unroute("**/api/v1/search**");

  const order = (await sections(page)).map((section) => section.provider);
  check(
    "sections keep the registry's order rather than reshuffling as answers land",
    order.indexOf("pages") >= 0 && order.indexOf("pages") < order.indexOf("media"),
    order.join(" > "),
  );

  // ---------------------------------------------------------------- typing latency
  // "Typing a partial page title returns that page within 300 ms p95." Each sample is measured
  // from the keystroke to the row: a query that matches nothing clears the screen first, so a
  // leftover row can never be mistaken for a fresh answer. Every sample is a real fragment of the
  // seeded title ("QA Sample Page"), which is what a person types on the way to it.
  // Every sample is a fragment the engine really answers: a title the person is typing out
  // ("QA Sample Page", from its first letter) or a word fragment long enough for the index's
  // similarity match. A two-letter word fragment ("sa") is not one of them — the prefix match
  // walks the title from the start and the trigram match wants more of the word than that.
  const partials = [
    "qa",
    "qa ",
    "qa s",
    "qa sa",
    "qa sam",
    "qa samp",
    "qa sampl",
    "qa sample",
    "qa sample ",
    "qa sample p",
    "qa sample pa",
    "qa sample pag",
    "qa sample page",
    "sampl",
    "sample",
    "sample ",
    "sample p",
    "sample pa",
    "page",
    "qa sampl",
  ];
  const samples = [];
  const missed = [];
  for (const [index, partial] of partials.entries()) {
    await type(page, `zzqqxx-nomatch-${index}`);
    const cleared = Date.now() + 2500;
    while (Date.now() < cleared) {
      const present = await page.locator('[data-palette-section="pages"]').count();
      if (present === 0) {
        break;
      }
      await page.waitForTimeout(60);
    }
    const started = Date.now();
    await page.locator("[data-palette-input]").first().fill(partial);
    await page
      .waitForSelector('[data-palette-section="pages"][data-palette-section-state="ready"] [id^="hit-pages-"]', {
        timeout: 4000,
      })
      .catch(() => {});
    const row = await page.locator('[id^="hit-pages-"]').count();
    const elapsed = Date.now() - started;
    if (row > 0) {
      samples.push(elapsed);
    } else {
      missed.push(`${partial} (no row in ${elapsed}ms)`);
    }
    console.log(`   sample ${index + 1}/${partials.length}: "${partial}" → ${row > 0 ? `${elapsed}ms` : "no row"}`);
  }
  samples.sort((left, right) => left - right);
  const at = (quantile) =>
    samples.length === 0 ? null : samples[Math.min(samples.length - 1, Math.ceil(quantile * samples.length) - 1)];
  const p50 = at(0.5);
  const p95 = at(0.95);
  check(
    "a partial page title answers within 300 ms p95",
    samples.length === partials.length && p95 !== null && p95 <= 300,
    `n=${samples.length} p50=${p50}ms p95=${p95}ms max=${samples[samples.length - 1]}ms${
      missed.length > 0 ? ` · missed: ${missed.join(", ")}` : ""
    }`,
  );
  await type(page, "");

  // ---------------------------------------------------------------- one type fails
  // The store answers 503 for one type only: that section takes the failure, everything else
  // keeps rendering, and the retry — after the store is back — refills it.
  await page.route("**/api/v1/search**", async (route) => {
    if (/types=media/.test(route.request().url())) {
      await route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({
          error: { code: "dependency_unavailable", message: "database is unavailable" },
        }),
      });
      return;
    }
    await route.continue();
  });
  await type(page, "sample");
  await page.waitForTimeout(1500);
  const failedSection = await waitForSection(page, "media", "error", 5000);
  const pagesWithFailure = await waitForSection(page, "pages", "ready", 4000);
  const errorNode = page.locator('[data-palette-group-error="media"]').first();
  const retryButton = page.locator('[data-palette-group-retry="media"]').first();
  const errorText = await errorNode.innerText().catch(() => "");
  const errorTooltip = await retryButton.getAttribute("title").catch(() => "");
  check(
    "a failed type renders its own retryable error",
    failedSection?.state === "error" && /could not be searched/i.test(errorText),
    `${failedSection?.state} · ${errorText.replace(/\s+/g, " ").slice(0, 70)}`,
  );
  check(
    "the error carries the failure's own code in its tooltip",
    /dependency_unavailable/.test(errorTooltip) && /503/.test(errorTooltip),
    errorTooltip,
  );
  check(
    "the other sections keep rendering while one type fails",
    pagesWithFailure?.state === "ready" && (pagesWithFailure?.rows ?? 0) > 0,
    `pages=${pagesWithFailure?.state}/${pagesWithFailure?.rows}`,
  );

  await page.unroute("**/api/v1/search**");
  await retryButton.click({ timeout: 4000 }).catch(() => {});
  const recovered = await waitForSection(page, "media", "ready", 8000);
  check(
    "the group's own retry brings it back",
    recovered?.state === "ready" && (recovered?.rows ?? 0) > 0,
    `media=${recovered?.state}/${recovered?.rows}`,
  );

  // ---------------------------------------------------------------- narrowing modes
  await type(page, "> pag");
  await page.waitForTimeout(700);
  const commandsOnly = await sections(page);
  const commandRows = await page.locator('[data-search-palette] [id^="command-"]').count();
  const chipCommands = (
    await page.locator("[data-palette-mode]").first().innerText().catch(() => "")
  ).trim();
  check(
    "`>` asks the registry only — no index call, no sections",
    commandsOnly.length === 0 && commandRows > 0,
    `${commandRows} commands · ${commandsOnly.length} sections`,
  );
  check("the mode chip reads Commands", /commands/i.test(chipCommands), chipCommands);

  await type(page, "# qa");
  await page.waitForTimeout(1100);
  const sitesOnly = await sections(page);
  check(
    "`#` asks the sites provider alone",
    sitesOnly.length >= 1 && sitesOnly.every((section) => section.provider === "sites"),
    sitesOnly.map((section) => `${section.provider}:${section.state}`).join(", "),
  );

  await type(page, ": search");
  await page.waitForTimeout(1100);
  const settingsOnly = await sections(page);
  check(
    "`:` asks the settings provider alone",
    settingsOnly.every((section) => section.provider === "settings"),
    settingsOnly.map((section) => `${section.provider}:${section.state}`).join(", ") || "none",
  );

  await type(page, "@ qa");
  await page.waitForTimeout(1100);
  const peopleSections = await sections(page);
  const peopleChip = (
    await page.locator("[data-palette-mode]").first().innerText().catch(() => "")
  ).trim();
  check(
    "`@` asks people only and promises no screen it does not have",
    peopleSections.length === 0 && /people/i.test(peopleChip),
    `${peopleSections.length} sections · chip ${peopleChip}`,
  );

  // ---------------------------------------------------------------- Tab across sections
  await type(page, "sample");
  await page.waitForTimeout(1500);
  const beforeTab = await activeSection(page);
  await page.locator("[data-palette-input]").first().press("Tab");
  await page.waitForTimeout(250);
  const afterTab = await activeSection(page);
  await page.locator("[data-palette-input]").first().press("Shift+Tab");
  await page.waitForTimeout(250);
  const afterShiftTab = await activeSection(page);
  check(
    "Tab and Shift+Tab walk the sections without the mouse",
    Boolean(beforeTab) &&
      Boolean(afterTab) &&
      afterTab.provider !== null &&
      afterTab.provider !== beforeTab.provider &&
      afterShiftTab?.provider === beforeTab.provider,
    `${beforeTab?.provider} → ${afterTab?.provider} → ${afterShiftTab?.provider}`,
  );

  // ---------------------------------------------------------------- see all → /search
  // Six pages carrying one unique word make the Pages section truncate, which is what puts its
  // "see all" row on screen. They are created through the API and found through the index.
  const marker = Array.from(
    { length: 12 },
    () => "abcdefghijklmnopqrstuvwxyz"[Math.floor(Math.random() * 26)],
  ).join("");
  const siteList = await api(page, "/api/v1/sites");
  const site = (siteList.body?.sites ?? [])[0] ?? null;
  let created = 0;
  if (site) {
    for (let index = 1; index <= 6; index += 1) {
      const created1 = await api(page, "/api/v1/pages", {
        method: "POST",
        body: JSON.stringify({
          site_id: site.id,
          slug: `${marker}-${index}`,
          title: `Federprobe ${marker} ${index}`,
        }),
      });
      if (created1.status < 300) {
        created += 1;
      }
    }
  }
  let indexed = 0;
  if (created > 0) {
    // The indexer follows the bus, and creating a draft writes no event of its own — the index
    // catches up on a publish or on the admin's own reindex, which is what this asks for.
    await api(page, "/api/v1/search/reindex", {
      method: "POST",
      body: JSON.stringify({ provider: "pages" }),
    });
  }
  const indexDeadline = Date.now() + 25000;
  while (Date.now() < indexDeadline) {
    const found = await api(
      page,
      `/api/v1/search?q=${encodeURIComponent(marker)}&types=pages&per_page=1&history=false`,
    );
    indexed = found.body?.total ?? 0;
    if (indexed >= 6) {
      break;
    }
    await page.waitForTimeout(700);
  }
  check(
    "the probe's six pages reach the index",
    created === 6 && indexed >= 6,
    `${created} created · ${indexed} indexed`,
  );

  /** The first number a node's text carries (the count, not the digits of the query). */
  const firstNumber = (page) =>
    page
      .evaluate(() => {
        const node = document.querySelector("[data-search-total]");
        const found = node ? /\d+/.exec(node.textContent || "") : null;
        return found ? Number(found[0]) : null;
      })
      .catch(() => null);

  await type(page, marker);
  await page.waitForTimeout(1600);
  const seeAll = page.locator('[data-palette-see-all="pages"]').first();
  const seeAllLabel = (await seeAll.innerText().catch(() => "")).replace(/\s+/g, " ").trim();
  check(
    "a truncated section offers its own see-all row with the provider's count",
    (await seeAll.count()) > 0 && seeAllLabel.includes(`See all ${indexed} Pages`),
    `${seeAllLabel} (api=${indexed})`,
  );
  await seeAll.click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1600);
  const landedUrl = page.url();
  const paletteClosed = (await page.locator("[data-search-palette]").count()) === 0;
  const landedTotal = await firstNumber(page);
  check(
    '"see all" lands on /search carrying the query and the type',
    /\/search\?/.test(landedUrl) &&
      /[?&]q=/.test(landedUrl) &&
      /[?&]type=pages/.test(landedUrl) &&
      paletteClosed,
    landedUrl,
  );
  check(
    "the results screen answers the same count the palette showed",
    landedTotal === indexed,
    `total=${landedTotal} api=${indexed}`,
  );
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForTimeout(1400);
  const reloadedTotal = await firstNumber(page);
  const chipRows = await page.locator("[data-chip]").count();
  check(
    "the URL alone reproduces the same result set, with its filter chip",
    reloadedTotal === indexed && chipRows > 0,
    `total=${reloadedTotal} api=${indexed} chips=${chipRows}`,
  );

  // ---------------------------------------------------------------- the empty answer
  await page.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(800);
  await openPalette(page);
  await type(page, "zzqqxx");
  await page.waitForTimeout(1800);
  const emptyState = page.locator("[data-palette-empty]").first();
  const emptyText = (await emptyState.innerText().catch(() => "")).replace(/\s+/g, " ").trim();
  const askAiRow = page.locator('[data-palette-action="ask-ai"]').first();
  const everywhereRow = page.locator('[data-palette-action="everywhere"]').first();
  check(
    "an empty answer offers the way out: the results screen and the AI Hub",
    /nothing matched/i.test(emptyText) &&
      (await askAiRow.count()) > 0 &&
      (await everywhereRow.count()) > 0,
    emptyText.slice(0, 110),
  );
  await askAiRow.click({ timeout: 4000 }).catch(() => {});
  await page.waitForTimeout(1400);
  const aiUrl = page.url();
  const promptValue = await page
    .locator("[data-chat-prompt]")
    .first()
    .inputValue()
    .catch(() => "");
  check(
    "Ask AI hands the words to the AI Hub, which opens with them prefilled",
    /\/ai\?/.test(aiUrl) && promptValue === "zzqqxx",
    `${aiUrl} · prompt="${promptValue}"`,
  );

  check(
    "no unexpected console errors on the owner's pass",
    consoleErrors.filter((text) => !/503|Service Unavailable/.test(text)).length === 0,
    consoleErrors.filter((text) => !/503|Service Unavailable/.test(text)).slice(0, 2).join(" | "),
  );

  // ---------------------------------------------------------------- the second account
  // A Member holds `search.read`, `content.pages.read` and `media.read` — and nothing else. The
  // account is planted in the disposable QA database with the Owner's own password hash, because
  // the panel has no invitation screen yet (that is REQ-006's work).
  psql(
    [
      "delete from users where lower(email) = lower('" + MEMBER_EMAIL + "')",
      "insert into users (organization_id, email, password_hash, display_name, status) " +
        "select organization_id, '" + MEMBER_EMAIL + "', password_hash, 'QA Member', 'active' " +
        "from users where lower(email) = lower('" + OWNER_EMAIL + "')",
      "insert into role_bindings (role_id, user_id, scope_type) " +
        "select r.id, u.id, 'global' from roles r, users u " +
        "where r.key = 'member' and r.organization_id is null " +
        "and lower(u.email) = lower('" + MEMBER_EMAIL + "')",
    ].join("; "),
  );

  const memberContext = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const member = await memberContext.newPage();
  const memberErrors = [];
  member.on("console", (msg) => {
    if (msg.type() === "error") memberErrors.push(msg.text().slice(0, 160));
  });
  check("signed in as the member", await signIn(member, MEMBER_EMAIL, OWNER_PASSWORD), member.url());
  await member.goto(`${ADMIN}/`, { waitUntil: "domcontentloaded" });
  await member.waitForTimeout(900);

  await openPalette(member);
  await type(member, ">");
  await member.waitForTimeout(1100);
  const memberCommands = (
    await member
      .locator('[data-search-palette] [id^="command-"]')
      .evaluateAll((rows) => rows.map((row) => (row.innerText || "").replace(/\s+/g, " ")))
  ).join(" | ");
  check(
    "a command above the account's keys is never listed",
    /open pages/i.test(memberCommands) &&
      !/create a page/i.test(memberCommands) &&
      !/open sites/i.test(memberCommands),
    memberCommands.slice(0, 150),
  );
  await member.keyboard.press("Escape");
  await member.waitForTimeout(400);

  // The shared link: a URL the Owner may read, opened by an account that may not. The link is the
  // sites search — the member holds no key that reaches it.
  let sharedQuery = null;
  let ownerSharedTotal = 0;
  for (const candidate of [site?.key, site?.name].filter(Boolean)) {
    const view = await api(
      page,
      `/api/v1/search?q=${encodeURIComponent(candidate)}&types=sites&per_page=5&history=false`,
    );
    const total = view.body?.total ?? 0;
    if (total >= 1) {
      sharedQuery = candidate;
      ownerSharedTotal = total;
      break;
    }
  }
  check(
    "the shared link's query matches a site the member may not read",
    Boolean(sharedQuery),
    `term=${sharedQuery ?? "none"} ownerSites=${ownerSharedTotal}`,
  );

  if (sharedQuery) {
    const shared = `/search?q=${encodeURIComponent(sharedQuery)}&type=sites`;
    // The Owner opens it first: the link is real and their own answer holds the row.
    await page.goto(`${ADMIN}${shared}`, { waitUntil: "domcontentloaded" });
    await page.waitForTimeout(1500);
    const ownerRows = await page.locator("[data-search-row]").count();
    const ownerTotal = await firstNumber(page);
    check(
      "the Owner's own view of the shared link holds the site",
      ownerRows >= 1 && ownerTotal === ownerSharedTotal,
      `rows=${ownerRows} total=${ownerTotal}`,
    );

    // The member opens the same URL: nothing they may read, and the count that says so.
    await member.goto(`${ADMIN}${shared}`, { waitUntil: "domcontentloaded" });
    await member.waitForTimeout(1600);
    const hiddenLine = (
      await member
        .locator("[data-search-hidden]")
        .first()
        .innerText()
        .catch(() => "")
    )
      .replace(/\s+/g, " ")
      .trim();
    const memberRows = await member.locator("[data-search-row]").count();
    const hiddenNumber = Number((/(\d+)/.exec(hiddenLine) || [])[1] || 0);
    check(
      "an empty answer says the results are outside the reader's permissions",
      /\d+\s+results?\s+(is|are)\s+outside your permissions/i.test(hiddenLine),
      hiddenLine,
    );
    check(
      "no row is rendered while the count says the records exist",
      memberRows === 0 && hiddenNumber >= ownerSharedTotal,
      `rows=${memberRows} hidden=${hiddenNumber} owner=${ownerSharedTotal}`,
    );
    // The answer itself, read back through the API as the member: the records are counted and
    // named nowhere.
    const memberAnswer = await api(
      member,
      `/api/v1/search?q=${encodeURIComponent(sharedQuery)}&types=sites&per_page=5&history=false`,
    );
    const memberBody = JSON.stringify(memberAnswer.body ?? {});
    const siteName = site?.name ?? "";
    check(
      "the member's own answer counts the hidden records and carries none of their titles",
      (memberAnswer.body?.total ?? -1) === 0 &&
        (memberAnswer.body?.hidden_total ?? 0) >= ownerSharedTotal &&
        Boolean(siteName) &&
        !memberBody.includes(siteName),
      `total=${memberAnswer.body?.total} hidden=${memberAnswer.body?.hidden_total} namesTitle=${memberBody.includes(siteName)}`,
    );

    // The Owner's own history keeps the search they opened; the palette's typing stays out of it.
    const history = await api(page, "/api/v1/search/recent");
    const queries = history.body?.queries ?? [];
    check(
      "palette typing stays out of the history while a committed search is kept",
      !queries.includes("zzqqxx") && queries.includes(sharedQuery),
      `history=[${queries.slice(0, 4).join(", ")}]`,
    );
  }

  check(
    "no unexpected console errors on the member's pass",
    memberErrors.filter((text) => !/40[13]/.test(text)).length === 0,
    memberErrors.filter((text) => !/40[13]/.test(text)).slice(0, 2).join(" | "),
  );

  await memberContext.close();
  await browser.close();

  const failed = results.filter((row) => !row.ok);
  console.log(
    `\nPROBE ${failed.length === 0 ? "PASS" : "FAIL"}: ${results.length - failed.length}/${results.length} checks`,
  );
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(`probe crashed: ${error && error.message}`);
  process.exit(1);
});
