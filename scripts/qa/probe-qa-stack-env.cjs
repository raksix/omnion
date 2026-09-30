#!/usr/bin/env node
/*
 * The two environment facts a QA stack must have before a pass can mean anything.
 *
 * Both were found the expensive way, and both failed *silently* — the pass ran, screens were
 * visited, a report was written, and the report was a wall of refusals that read like hundreds of
 * defects. Neither produced an error that pointed at itself.
 *
 *   1. **No CSRF secret.** `headers_middleware` fail-closes: with `OMNION_CSRF_SECRET` unset,
 *      *every* cookie-authenticated POST answers 403. The first-run wizard therefore cannot create
 *      the organization, the owner has no tenant, every org-scoped route answers
 *      `organization_required`, and the report collects hundreds of repeats of two root causes.
 *      The refusal is correct product behaviour. An unconfigured QA stack is the defect.
 *
 *   2. **The API started without it and nobody noticed for three ticks**, because the fixture was
 *      "fixed" twice at the wizard level — each time correctly, and each time upstream of the
 *      cause. The wizard retry loop logged seven "Create organization" clicks and seven 403s, and
 *      that log line was the only honest signal in the whole run.
 *
 * So this asserts on the *stack*, not on a symptom: `run.sh` must start the API with a CSRF secret,
 * and the fixture must prove the wizard actually reaches "your installation is ready" and that the
 * signed-in owner has a tenant. A wizard that "ran" is not a wizard that finished.
 */
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const RUN = path.join(__dirname, "run.sh");
const WALK = path.join(__dirname, "walkthrough.cjs");
const runSrc = fs.readFileSync(RUN, "utf8");
const walkSrc = fs.readFileSync(WALK, "utf8");

let pass = 0;
const cases = [];
const test = (name, fn) => cases.push([name, fn]);
const run = () => {
  for (const [name, fn] of cases) {
    try {
      fn();
      pass += 1;
      console.log(`  ok  ${name}`);
    } catch (err) {
      console.error(`  FAIL ${name}\n       ${err.message}`);
      process.exitCode = 1;
    }
  }
  console.log(`\n${pass}/${cases.length} PASS`);
};

test("run.sh starts the API with a CSRF secret", () => {
  // Read the actual `pm2 start "$API_BIN"` invocation rather than trusting a comment: the point of
  // this gate is that a comment is not an environment variable.
  // Anchor on the real invocation, not the first textual mention: `indexOf` finds the
  // delete-vs-restart COMMENT first and this gate would then read 2000 characters of prose.
  const anchor = runSrc.search(/^\s*pm2 start "\$API_BIN"/m);
  assert.notEqual(anchor, -1, "run.sh no longer starts the API this way");
  // The environment is the continuation chain ABOVE the command, so the window has to reach back
  // to the previous `pm2`/`wait_http` line and no further.
  const start = runSrc.slice(Math.max(0, anchor - 900), anchor + 200);
  assert.match(
    start,
    /OMNION_CSRF_SECRET="?\$\{QA_CSRF_SECRET:-[^}]+\}/,
    "the API is started without OMNION_CSRF_SECRET, so every cookie-authenticated POST is refused with 403",
  );
  // A blank default would be the same defect wearing a different hat.
  const defaultValue = /OMNION_CSRF_SECRET="?\$\{QA_CSRF_SECRET:-([^}]*)\}/.exec(start)?.[1] ?? "";
  assert.ok(defaultValue.trim().length >= 16, "the default CSRF secret is too short to be a secret");
});

test("the default CSRF secret is a throwaway and never a real credential", () => {
  const value = /OMNION_CSRF_SECRET="?\$\{QA_CSRF_SECRET:-([^}]*)\}/.exec(runSrc)?.[1];
  assert.ok(value, "no default secret is declared at all");
  // The database under this stack is dropped at the top of every pass, so this value protects
  // nothing outside the box. Asserting that keeps a later editor from swapping in a real one.
  assert.match(value, /qa|test|stack|disposable/i, "the default secret does not read as a throwaway");
  assert.ok(!/[A-Za-z0-9]{32,}/.test(value), "the default secret looks like real key material");
});

test("the wizard proves it finished, not merely that it started", () => {
  // `report.steps` recorded seven "Create organization" clicks and never a completion in the run
  // that found the CSRF secret. A pass whose fixture cannot say "the organization exists" cannot
  // distinguish a working installation from a half-built one, and every org-scoped screen after it
  // then reports a cascade instead of a cause.
  assert.ok(
    walkSrc.includes("Your installation is ready"),
    "the wizard does not look for its own completion screen",
  );
  assert.ok(
    /data-setup-step/.test(walkSrc),
    "the wizard reads the current step from the page rather than assuming it moved",
  );
  // The retry loop must be bounded AND must record that it gave up. An unbounded retry on a step
  // that cannot succeed is what turned one missing variable into seven identical requests.
  const loop = walkSrc.slice(walkSrc.indexOf("async function runWizard"));
  const body = loop.slice(0, loop.indexOf("\nasync function ") + 1);
  assert.match(body, /for \(let wait = 0; wait < \d+; wait \+=\s*1\)/, "the step retry is unbounded");
  assert.match(body, /no current step at iteration/, "a step that will not move is not reported");
});

test("the pass records whether the owner ended up with a tenant", () => {
  // The single assertion that turns "the wizard ran" into "the installation exists". Without it
  // the cascade is discovered by counting refusals, which is the slowest possible reader.
  assert.ok(
    /wizard: organization/.test(walkSrc) || /verifyInstallation/.test(walkSrc) || /needs_setup/.test(walkSrc),
    "nothing in the pass verifies the installation's post-wizard state",
  );
  // `signedIn` alone is not that: an owner with no organization signs in perfectly well, and then
  // every org-scoped route answers 400. The pass recorded `signedIn: true` on that exact run.
  assert.ok(
    /organization_required|resolve_organization/.test(walkSrc) || walkSrc.includes("onboarding"),
    "the pass never looks at the tenant the owner belongs to",
  );
});

test("a scoped pass cannot reach the wizard out of band", () => {
  // `--only=wizard` re-runs the first-run flow on its own. It must not ALSO be reachable from a
  // scoped pass, or a QA pass and a first-run pass could reset the same database underneath each
  // other on a shared stack.
  assert.ok(
    walkSrc.includes('--only=wizard'),
    "the first-run scope is documented in the header but not implemented",
  );
  const wizardIdx = walkSrc.indexOf('process.argv.includes("--only=wizard")');
  // The block ends in `process.exit(...)`, not a `return` — it has already written its own
  // summary and closed the browser, so falling through is impossible by construction.
  const scopeBody = walkSrc.slice(wizardIdx, wizardIdx + 2600);
  assert.match(scopeBody, /process\.exit\(/, "the wizard scope does not end the pass");
  // And it must actually ANSWER something: writing summary.json and falling through would be a
  // scope that ends the pass without reporting the one question it exists to answer.
  assert.match(scopeBody, /wizardFinished/, "the wizard scope does not record whether setup finished");
  assert.match(scopeBody, /onboardingFailures/, "the wizard scope does not report the onboarding refusals");
});

run();
