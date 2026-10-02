/**
 * The stack-liveness gate, which threw away three ticks of passes in a row.
 *
 * ## What it found
 *
 * `stackGone` is the last gate the walkthrough runs. If it says the stack is gone, the pass
 * throws away every click it made — 1,844 of them on the run that died — and exits non-zero
 * with `QA_FINDINGS=0`. It fired three times on healthy stacks, and the reason is two lines:
 *
 * ```js
 * const res = await context.request.get(`${URL_ADMIN}/login`, { timeout: 8000 });
 * return !res || res.status() >= 500;
 * } catch {
 *   return true;
 * }
 * ```
 *
 * Three separate mistakes live in that block, and only the first is the obvious one:
 *
 * 1. **A timeout is not death.** Under load — six writers on this box — a Next render takes
 *    longer than 8 seconds routinely, the `catch` fired, and a perfectly healthy stack was
 *    reported gone. This is the one that cost the passes.
 * 2. **A 5xx is not death either, and this half is backwards.** `res.status() >= 500` calls a
 *    *response* fatal. A 500 is a render that threw and the process that came back up to send
 *    it; on a memory-pressured stack that is the stack working. The check treats "answered"
 *    as "gone" — the opposite of what a liveness probe is for.
 * 3. **The response is tested but the exception is not.** A refused connection and a timeout
 *    both land in `catch`, and only the first is proof of death. Nothing distinguished them.
 *
 * ## Why a source-reading test and not an exported predicate
 *
 * The obvious fix is to export the predicate and unit-test it, and that is deliberately not
 * what this does. An exported helper is a *second* copy of the rule, and the bug this directory
 * keeps meeting is a gate that reads well and decides wrong — a second implementation would be
 * free to drift from the one that runs. So the assertions below read the harness's own source
 * and name the CONSTRUCTS that do the work, in the block that performs them.
 *
 * The window is `const stackGone` up to `report.assertStackAlive`, which is the narrowest span
 * that contains every construct under test. A wider window would let a construct named
 * elsewhere in a 15,000-line file satisfy an assertion about this one — the failure this file's
 * siblings already document twice.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/** `stackGone`'s own source: the declaration through the assertion wrapper that follows it. */
const LIVENESS = (() => {
  const from = WALKTHROUGH.indexOf("const stackGone = async () => {");
  assert.notEqual(from, -1, "walkthrough.cjs must define stackGone");
  const to = WALKTHROUGH.indexOf("report.assertStackAlive", from);
  assert.notEqual(to, -1, "walkthrough.cjs must define report.assertStackAlive after stackGone");
  return WALKTHROUGH.slice(from, to);
})();

test("a status is never read as proof of death", () => {
  // A liveness probe asks whether the server ANSWERED. A 500 is an answer — the render threw and
  // a live process sent it — and the old `res.status() >= 500` reported that process as gone,
  // which is backwards. If any status comparison comes back, the gate is broken again.
  assert.equal(
    LIVENESS.match(/\.status\(\)/g),
    null,
    "stackGone must not read the response status: a 5xx is an answer, not a dead stack",
  );
});

test("a timeout is retried on a wider budget instead of deciding death", () => {
  // The defect. The first attempt must be followed by MORE attempts, because the failure being
  // fixed is a busy box — one 8s probe losing the race is not a verdict.
  const timeouts = [...LIVENESS.matchAll(/for \(const timeout of \[([^\]]+)\]/g)].flatMap((m) =>
    m[1].split(",").map((n) => Number(n.trim())),
  );
  assert.ok(timeouts.length >= 3, `expected a retry ladder, found budgets: ${JSON.stringify(timeouts)}`);
  assert.ok(timeouts[0] < timeouts[1] && timeouts[1] < timeouts[2], `the ladder must widen: ${JSON.stringify(timeouts)}`);
  // The catch branch is the one that used to decide death; it must not decide it on a timeout.
  const catchBody = LIVENESS.slice(LIVENESS.indexOf("} catch (err) {"));
  assert.equal(
    /timeout/i.test(catchBody) && /return true/.test(catchBody.slice(0, catchBody.indexOf("// Anything else"))) ,
    true,
    "the catch branch must distinguish a refused connection from a timeout before returning true",
  );
});

test("a refused connection is fatal immediately, without spending the ladder", () => {
  // Retrying a port nothing is listening on buys 48 seconds and still finds nothing, and the
  // whole point of the ladder is that it is for DELAY. The refusal has to be named and returned
  // before the loop continues, not after the last attempt.
  assert.match(LIVENESS, /ECONNREFUSED/, "a refused connection must be recognised by name");
  assert.match(
    LIVENESS,
    /ECONNREFUSED[\s\S]{0,200}return true/,
    "the refusal must return true inside the catch, not after the loop ends",
  );
});

test("the retry ladder still ends in death, so the gate cannot go permanently green", () => {
  // The opposite failure, and the one a naive fix invites: retry forever and a dead stack is
  // never reported, so every finding after it gets published against a server that is gone.
  // Three unanswered attempts must still mean gone.
  //
  // The end of the ladder is found by MATCHING BRACES, not by the last `}` in the window: that
  // is the function's own closing brace, so a text search located the boundary outside the block
  // under test and this assertion passed for the wrong reason on the first run.
  const ladderAt = LIVENESS.indexOf("for (const timeout of");
  assert.notEqual(ladderAt, -1, "the retry ladder must exist");
  let depth = 0;
  let loopEnd = -1;
  for (let i = LIVENESS.indexOf("{", ladderAt); i < LIVENESS.length; i += 1) {
    if (LIVENESS[i] === "{") depth += 1;
    else if (LIVENESS[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        loopEnd = i;
        break;
      }
    }
  }
  assert.notEqual(loopEnd, -1, "the retry ladder must be a closed block");
  const tail = LIVENESS.slice(loopEnd);
  assert.match(
    tail,
    /return true/,
    "after the ladder is exhausted the gate must still report the stack gone",
  );
});
