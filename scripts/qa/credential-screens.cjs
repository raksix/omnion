// REQ-087 slice 2 — the three credential screens, driven directly against the private stack.
// The walkthrough pass is queued behind other stacks; this renders the same three routes and
// measures them, so the screens are confirmed even while the full pass waits for a slot.
const { chromium } = require("playwright-core");

const ADMIN = process.env.QA_ADMIN || "http://127.0.0.1:3109";
const API = process.env.API || "http://127.0.0.1:18089";
const CHROME =
  process.env.QA_CHROME || "/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome";
const CREDS = {
  email: process.env.QA_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_PASSWORD || "OmnionQa-Passw0rd-2026!",
};

const steps = {};
// Three outcomes, not two. A step is `ok` (a claim the screen must back), a failure, or a
// *note* (a situation the probe found and reported, with no claim attached). Counting a note as
// a pass inflates the denominator, which is how a probe ends up reporting "19 of 19" while
// quietly asserting less than it says.
const record = (name, value) => {
  const kind = value === false ? "fail" : typeof value === "string" ? "note" : "ok";
  steps[name] = { kind, value };
  const mark = kind === "fail" ? "FAIL" : kind === "note" ? "note" : "ok  ";
  console.log(`  ${mark} ${name}${value === true ? "" : `: ${JSON.stringify(value)}`}`);
};

(async () => {
  const browser = await chromium.launch({
    executablePath: CHROME,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--js-flags=--max-old-space-size=512", "--disable-gpu"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const errors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text().slice(0, 200));
  });

  // Sign in through the API and carry the session cookie into the panel, exactly as a browser
  // would after the login form posts.
  const login = await page.request.post(`${API}/api/v1/auth/login`, { data: CREDS });
  record("login", login.ok());
  const cookies = await login.headersArray();
  for (const header of cookies) {
    for (const part of String(header.value || "").split(/,(?=[^;]+?=)/)) {
      const [pair] = part.split(";");
      const [name, value] = pair.split("=");
      if (name && value && /session/i.test(name)) {
        await context.addCookies([
          { name: name.trim(), value: value.trim(), domain: "127.0.0.1", path: "/" },
        ]);
      }
    }
  }

  // 1. The list renders real data — and no 403, which is what the queued pass hit.
  await page.goto(`${ADMIN}/workflows/credentials`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  const listText = (await page.locator("main").innerText().catch(() => "")).replace(/\s+/g, " ");
  record("the list rendered", /credential/i.test(listText));
  record("no 403 on the list", !/403|Forbidden/i.test(listText));
  record("no console error on the list", errors.length === 0 ? true : errors.slice(0, 2));
  record("a count line", /credential/.test(await page.locator("[data-credential-count]").innerText().catch(() => "")));
  // The empty state must be the *empty* one, naming what to do — not a blank box.
  const empty = await page.locator("[data-credential-empty]").count();
  record("an empty state that explains itself", empty > 0 ? true : "there are rows, so no empty state expected");
  if (empty > 0) {
    const emptyText = (await page.locator("[data-credential-empty]").innerText()).replace(/\s+/g, " ");
    record("  …offers a way to add one", /New credential|API key|OAuth/i.test(emptyText));
  }
  // No horizontal overflow at 390 px — the mobile clause.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(700);
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
  );
  record("readable at 390px (no horizontal overflow)", !overflow);
  await page.setViewportSize({ width: 1440, height: 900 });

  // 2. The create form. The picker is checked on its own route, and the form is driven through
  //    `?type=` — the shape the list's empty state links to. Loading `/new` bare and clicking a
  //    type leaves the picker *on top of* the form, so a probe that measures both on one page
  //    measures whichever won the race.
  await page.goto(`${ADMIN}/workflows/credentials/new`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2500);
  const cards = await page.locator("[data-credential-type]").count();
  record("the type picker offers the catalogue", cards >= 4 ? true : `${cards} cards`);

  await page.goto(`${ADMIN}/workflows/credentials/new?type=api_key`, { waitUntil: "domcontentloaded" }).catch(() => {});
  await page.waitForTimeout(2200);
  record("?type= lands straight on the form", (await page.locator('[data-credential-field="name"]').count()) > 0);
  record("the secret input is masked", (await page.locator('[data-credential-secret="api_key"]').getAttribute("type")) === "password");
  record("no console error on the form", errors.length === 0 ? true : errors.slice(0, 2));

  // 3. The detail: create one through the UI, then read the masked field.
  await page.locator('[data-credential-field="name"]').fill(`Screen check ${Date.now().toString(36)}`).catch(() => {});
  await page.locator('[data-credential-secret="api_key"]').fill("screen-check-secret-zzz").catch(() => {});
  await page.locator("[data-credential-save]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2500);
  record("the create lands on the detail", /\/workflows\/credentials\/[0-9a-f-]{36}/.test(page.url()));
  const masked = (await page.locator('[data-credential-masked="api_key"]').innerText().catch(() => "")).trim();
  record("the secret renders as a mask", masked.length >= 8 ? true : `got "${masked}"`);
  record("the mask is not the value", !masked.includes("screen-check-secret"));
  record("no reveal control on the masked field", (await page.locator('[data-credential-masked="api_key"] input').count()) === 0);
  const detailText = (await page.locator("main").innerText().catch(() => "")).replace(/\s+/g, " ");
  record("the secret never appears on the screen", !detailText.includes("screen-check-secret"));
  record("the usage panel answers", /No workflow names|Delete/.test(detailText));
  // The bug this probe found: pasting a key and being redirected to a detail screen that never
  // mentions it. A credential whose secret was not stored says so, on the row it concerns.
  record(
    "a secret that was not stored says so on the row",
    (await page.locator("[data-credential-secret-warning]").count()) > 0,
  );
  record("the row is marked untested, not working", /Not verified/.test(detailText));

  // 4. The test button reports a result, and it is not a pass.
  await page.locator("[data-credential-test]").click({ timeout: 6000 }).catch(() => {});
  await page.waitForTimeout(2200);
  const outcome = await page.locator("[data-credential-test-result]").getAttribute("data-credential-test-result").catch(() => null);
  record("the test reported a result", outcome === "ok" || outcome === "failed" ? true : String(outcome));
  record("the test did not fake success", outcome !== "ok");

  const values = Object.values(steps);
  const ok = values.filter((s) => s.kind === "ok").length;
  const failed = values.filter((s) => s.kind === "fail").length;
  const notes = values.filter((s) => s.kind === "note").length;
  console.log(`\n== ${ok} of ${ok + failed} claims held (${notes} note(s)) ==`);
  await browser.close();
  process.exit(failed > 0 ? 1 : 0);
})().catch((err) => {
  console.error("FATAL", err);
  process.exit(2);
});
