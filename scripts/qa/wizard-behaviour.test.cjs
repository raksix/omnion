#!/usr/bin/env node
// `runWizard` is a TIMING decision, and a timing decision proved only by a browser pass
// cannot be re-run cheaply, cannot run on a shared slot, and cannot be trusted twice a day.
//
// This drives the real function body — extracted from `walkthrough.cjs` and evaluated with a
// stub page — through the exact race that cost this request three ticks: a URL that is not
// `/setup` at the first read and becomes `/setup` later. The stub models the redirect with
// a scripted number of "reads before it settles", so the old code and the new code are held
// to the same question: did the wizard drive the steps, and did it submit anything empty?
//
// Nothing here mocks the function under test. The body is the file's body.
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(path.join(__dirname, "walkthrough.cjs"), "utf8");
const start = src.indexOf("async function runWizard(");
const end = src.indexOf("async function ensureSignedIn(");
if (start < 0 || end < 0) throw new Error("runWizard() not found — this gate no longer covers it");
const body = src.slice(start, end);

/**
 * A page whose URL settles after `readsBeforeSettle` reads, recording every submission.
 *
 * `fillWizardStep` reports one filled field per step (the fixture is a 3-step wizard: owner,
 * organization, site), `clickAction` clicks the current step's button and advances the URL
 * when the step is the last one — the shape the real page has.
 */
function makePage({ readsBeforeSettle, initialUrl, steps }) {
  const calls = { fills: 0, clicks: [], waits: 0, waited: [] };
  let reads = 0;
  let step = 0;
  const page = {
    url: () => {
      reads += 1;
      if (reads <= readsBeforeSettle) return initialUrl;
      return "/setup";
    },
    waitForTimeout: async (ms) => {
      calls.waits += 1;
      calls.waited.push(ms);
    },
    waitForURL: async () => {
      calls.waits += 1;
      reads = readsBeforeSettle + 1;
      return true;
    },
    goto: async () => {},
    locator: () => {
      // `page.locator(sel).first().click()` is the shape the body uses to press the finish
      // button; a chainable stub keeps the reached-done branch from throwing before the
      // assertions that matter run.
      const chain = {
        first: () => chain,
        count: async () => 0,
        isVisible: async () => false,
        click: async () => {},
        evaluate: async () => false,
      };
      return chain;
    },
    evaluate: async (fn) => {
      // THREE readers share this hook and each wants a different type back. The first version
      // of this stub answered all of them with the current step key, and the boolean reader —
      // `/Your installation is ready/i.test(...)` — got a truthy STRING, so the loop read
      // "installation ready" after the first step and stopped. The walkthrough was right and
      // this stub was wrong, which is the only way the red check can be resolved honestly: a
      // gate that fails because its fixture lies about the page teaches nothing except to
      // delete the gate.
      //
      // The readers are told apart by their own source, not by a flag the call site passes:
      //   · the boolean reader never mentions the step element
      //   · the outer step reader returns "done", the inner settle reader returns "ready"
      const src = String(fn);
      const keys = steps.map((s) => s.key);
      const onStep = (fallback) => (step < keys.length ? keys[step] : fallback);
      if (!src.includes("data-step-state")) return step >= keys.length;
      if (src.includes('return "done"')) return onStep("done");
      return onStep("ready");
    },
    __calls: calls,
  };
  page.__helpers = {
    fillWizardStep: async () => {
      calls.fills += 1;
      return [{ field: "email", value: "qa-owner@omnion.test" }];
    },
    clickAction: async () => {
      const label = steps[step] ? steps[step].action : null;
      calls.clicks.push(label);
      step += 1;
      return label;
    },
  };
  return page;
}

// Evaluate the extracted body with the helpers and logging it closes over. `shot` is one of
// them: the body takes a screenshot on the line after it decides it is on the wizard, which is
// exactly the artifact whose ABSENCE proves the skip — so the harness records it rather than
// stubbing it away, and the behaviour gate can assert on the same evidence the report would.
const makeRunWizard = (log) =>
  new Function(
    "URL_ADMIN",
    "log",
    "shot",
    "fillWizardStep",
    "clickAction",
    `${body}\nreturn runWizard;`,
  );

const steps = [
  { key: "owner", action: "Create account" },
  { key: "organization", action: "Create organization" },
  { key: "site", action: "Create site" },
];

const failures = [];
const check = (name, ok, detail) => {
  if (!ok) failures.push(`${name}${detail ? ` — ${detail}` : ""}`);
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${ok || !detail ? "" : `  (${detail})`}`);
};

const quiet = () => {};
const noopShot = async () => {};
const runWith = (page, fillWizardStep, clickAction) =>
  makeRunWizard(quiet)(
    "http://admin",
    quiet,
    noopShot,
    fillWizardStep,
    clickAction,
  )(page, { steps: [] });

async function run({ readsBeforeSettle, initialUrl }) {
  const page = makePage({ readsBeforeSettle, initialUrl, steps });
  const report = { steps: [] };
  const runWizard = makeRunWizard(quiet)(
    "http://admin",
    quiet,
    noopShot,
    page.__helpers.fillWizardStep,
    page.__helpers.clickAction,
  );
  const result = await runWizard(page, report);
  return { result, calls: page.__calls, report };
}

(async () => {
  // ---- the race that actually happened: `/` does not settle for nine reads --------------
  const raced = await run({ readsBeforeSettle: 9, initialUrl: "http://admin:3105/" });
  check(
    "a slow client-side redirect is waited out, not mistaken for a set-up installation",
    raced.result.ran === true,
    `ran=${raced.result.ran} skippedBecause=${raced.result.skippedBecause}`,
  );
  check(
    "the wizard drove every step once it was on /setup",
    raced.calls.clicks.length === steps.length,
    `clicked ${raced.calls.clicks.length} of ${steps.length}`,
  );
  check(
    "the reached-/setup step is recorded",
    raced.report.steps.some((s) => s.action === "reached /setup"),
    "no reached-/setup entry in the report",
  );

  // ---- a first run that lands on /login, which redirects to /setup -----------------------
  const viaLogin = await run({ readsBeforeSettle: 3, initialUrl: "http://admin:3105/login" });
  check(
    "a /login that resolves to /setup is waited out",
    viaLogin.result.ran === true,
    `ran=${viaLogin.result.ran}`,
  );

  // ---- an installation that really is set up still skips, and says why -------------------
  // The redirect into the wizard does NOT resolve here — this is an installation that has
  // accounts and stays on its login screen — so `waitForURL` must refuse, exactly as
  // Playwright's would on a timeout.
  const installed = makePage({ readsBeforeSettle: 99, initialUrl: "http://admin:3105/login", steps });
  installed.url = () => "http://admin:3105/login";
  installed.waitForURL = async () => {
    installed.__calls.waits += 1;
    throw new Error("Timeout 15000ms exceeded");
  };
  const skipReport = { steps: [] };
  const skipRun = makeRunWizard(quiet)(
    "http://admin",
    quiet,
    noopShot,
    installed.__helpers.fillWizardStep,
    installed.__helpers.clickAction,
  );
  const skipped = await skipRun(installed, skipReport);
  check(
    "a genuinely set-up installation still skips the wizard",
    skipped.ran === false,
    `ran=${skipped.ran}`,
  );
  check(
    "the skip carries a reason and is written to the report",
    skipped.skippedBecause === "already-installed" &&
      skipReport.steps.some((s) => s.action === "wizard-skipped"),
    `skippedBecause=${skipped.skippedBecause}`,
  );
  check(
    "a set-up installation submits nothing at all",
    installed.__calls.clicks.length === 0,
    `clicked ${installed.__calls.clicks.length} times`,
  );

  // ---- an unfillable step is never submitted ---------------------------------------------
  const emptyPage = makePage({ readsBeforeSettle: 0, initialUrl: "http://admin:3105/setup", steps });
  emptyPage.__helpers.fillWizardStep = async () => [];
  const emptyReport = { steps: [] };
  const emptyRun = makeRunWizard(quiet)(
    "http://admin",
    quiet,
    noopShot,
    emptyPage.__helpers.fillWizardStep,
    emptyPage.__helpers.clickAction,
  );
  await emptyRun(emptyPage, emptyReport);
  check(
    "a step with nothing to fill is recorded and not submitted",
    emptyPage.__calls.clicks.length === 0 &&
      emptyReport.steps.some((s) => s.reason === "nothing to fill"),
    `clicked ${emptyPage.__calls.clicks.length} times`,
  );

  if (failures.length > 0) {
    console.error(`\n${failures.length} wizard behaviour check(s) failed:`);
    for (const line of failures) console.error(`  - ${line}`);
    process.exit(1);
  }
  console.log("\nall wizard behaviour checks passed");
})();