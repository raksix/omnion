/**
 * The mutation harness for `table-mode-row.test.ts`.
 *
 * A green row test proves only that the assertions run. What makes them worth anything is
 * whether the DEFECT they were written for turns them red — and this REQ has now produced
 * thirteen readings that were green against a defect entirely unchanged, so "the test passes"
 * is not evidence on its own. Every mutation below breaks the product/harness in the specific
 * way the matching assertion exists to catch, and the run is only correct if all of them fail.
 *
 * The eight earlier mutations in this REQ share one cause: a name that appears in two places
 * satisfies an assertion made about one of them. So each mutation here targets a CONSTRUCT —
 * the read, the selector, the conjunction — rather than a keyword.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const WALKTHROUGH = `${ROOT}scripts/qa/walkthrough.cjs`;
const TEST = `${ROOT}apps/admin/features/workflows/table-mode-row.test.ts`;

const original = readFileSync(WALKTHROUGH, "utf8");

const MUTATIONS = [
  {
    name: "M1 the reverse read goes back to fetching the server",
    // The defect this whole file was written for. Reverting the canvas read to the fetch the
    // row used before must turn tests 1 AND 2 red.
    //
    // The anchor moved in tick 59 and this harness was not updated with it, so **M1 has not
    // run for two ticks** — the three "MUTATION DID NOT APPLY" lines in its own output are
    // the only thing that ever said so, and a harness prints them next to real failures. The
    // row's reduction is now `const holder = perNode.find(…)`; the read above it is the
    // per-card `page.evaluate` that walks the cards. Anchoring on the reduction's own name
    // and rewriting the *whole* read is deliberately avoided here: a broad regex is how this
    // file's harness once edited a different row and reported an honest result for a test it
    // never touched. So M1 replaces the per-card evaluate's *body* — the fetch is the thing
    // being restored, and the body is a single expression.
    from: /const read = await page\n\s*\.evaluate\(\n\s*\(\{ id, value \}\) => \{[\s\S]*?\n\s*\{ id: nodeId, value: "qa\.table\.edited" \},\n\s*\)/,
    to: `const read = await page.evaluate(async ({ id, value }) => {
    const current = await (await fetch(\`/api/v1/workflows/\${id}/graph\`, { credentials: "same-origin" })).json();
    return {
      mounted: true,
      found: Object.values(current.graph.nodes ?? {}).some((n) =>
        Object.values(n.params ?? {}).includes(value),
      ),
      fields: [],
    };
  }, { id: nodeId, value: "qa.table.edited" })`,
  },
  {
    name: "M2 the read counts any parameter rather than the committed value",
    // `Object.values(n.params).length > 0` satisfies "a value changed" and tells you nothing
    // about whether the TABLE's value survived. Test 2's "names the VALUE" half.
    from: 'found: fields.some((f) => f.value === value)',
    to: 'found: fields.some((f) => f.value !== null && f.value !== "")',
  },
  {
    name: "M3 the read looks for the field marker but not the panel",
    // `data-inspector` is a PREFIX of `data-inspector-field`, so a read that scans every field
    // on the page satisfies an assertion about the panel. The prefix trap in its purest form.
    //
    // The `panel` variable is what carries the scoping; a read that dropped it and used
    // `document` directly would satisfy an assertion about the panel page-wide. Anchored on
    // the declaration so the replacement keeps the variable alive and the assertions about
    // `panel.querySelectorAll` still have something to bite on.
    from: 'const panel = document.querySelector(`[data-inspector="${id}"]`);',
    to: 'const panel = document;',
  },
  {
    name: "M4 the wait for the inspector is dropped for a fixed delay",
    // Every assertion below the read is green against a page that has not drawn. The delay is
    // the quiet version of the tick-57 race. The wait is now per-card (the row clicks each
    // card in turn), so the anchor is the per-card wait and not the old page-wide one.
    from: 'await page.waitForSelector(`[data-inspector="${nodeId}"]`, { timeout: 4000 }).catch(() => {});',
    to: "await page.waitForTimeout(1500);",
  },
  {
    name: "M5 the forward direction is read off the graph instead of the table",
    // The other half of "in either mode". A table holding its own private copy of the graph
    // passes the reverse read and fails exactly here.
    from: /const labelsAfterCanvasSave = await page\.\$\$eval\("\[data-table-label\]"[\s\S]*?\}\);/,
    to: `const labelsAfterCanvasSave = await page.evaluate(async (id) => {
    const current = await (await fetch(\`/api/v1/workflows/\${id}/graph\`, { credentials: "same-origin" })).json();
    return (current.graph.nodes ?? []).map((n) => n.label);
  }, workflowId);`,
  },
  {
    name: "M6 the create note drops the server's refusal message",
    // Three ticks each guessed at a 422 payload because the note recorded only a status, and
    // the whole block below returned early looking like an empty table.
    from: 'refusal: created.status >= 400 ? (created.body?.error?.message ?? "").slice(0, 200) : null,',
    to: "refusal: null,",
  },
  {
    name: "M7 the run refusal accepts any 4xx as 'not ready'",
    // 403 and 404 are 'not allowed' and 'unknown', and a run that is refused for either is not
    // the claim the criterion makes about an unfinished rule.
    from: "refused: runResponse.status === 400",
    to: "refused: runResponse.status >= 400 && runResponse.status < 500",
  },
  {
    name: "M8 the problems panel is asked whether it is clean, not whether it found the parameter",
    // `notClaimingClean` alone is satisfied by a panel that renders nothing at all.
    from: 'namesTheMissingParameter: problemsAfterUnfinishedSave.listed.includes("missing_parameter"),',
    to: "namesTheMissingParameter: null,",
  },
];

let failures = 0;
for (const mutation of MUTATIONS) {
  const pattern =
    mutation.from instanceof RegExp ? mutation.from : new RegExp(escape(mutation.from));
  const mutated = original.replace(pattern, mutation.to);
  if (mutated === original) {
    console.log(`✗ ${mutation.name} — MUTATION DID NOT APPLY (the anchor moved)`);
    failures += 1;
    continue;
  }
  writeFileSync(WALKTHROUGH, mutated);
  const run = spawnSync(
    process.execPath,
    ["--test", "--experimental-strip-types", TEST],
    { cwd: ROOT, encoding: "utf8" },
  );
  const passed = run.status === 0;
  if (passed) {
    console.log(`✗ ${mutation.name} — SURVIVED (the suite is still green)`);
    failures += 1;
  } else {
    const named = [...run.stdout.matchAll(/^not ok \d+ - (.+)$/gm)].map((m) => m[1].trim());
    console.log(`✓ ${mutation.name} — red: ${named.slice(0, 3).join("; ") || "(suite failed)"}`);
  }
}
writeFileSync(WALKTHROUGH, original);
assert.equal(readFileSync(WALKTHROUGH, "utf8"), original, "the walkthrough must be restored");
console.log(`\n${MUTATIONS.length - failures}/${MUTATIONS.length} mutations red`);

// A mutation that cannot apply is a mutation that is not being measured, and reporting it as
// a failure is the difference between "the anchor moved" and "the harness is lying".
function escape(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
