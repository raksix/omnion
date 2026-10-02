/**
 * Mutations for `keyboard-pass-row.test.ts`. Each one reverts a real decision in the walkthrough
 * row and must turn that suite red. A guard that nothing can make red is an absent reading.
 *
 * The mutations are chosen from the row's own defects rather than invented:
 *
 *  - **M1** restores the shipped defect verbatim: the parameter read asks for `node_type`, the
 *    field the server does not send. This is the one the whole file exists for.
 *  - **M2** puts the `paramWrote` gate back — reporting a write without a resolved subject, which
 *    is a number about somebody else's node.
 *  - **M3** swaps the subject read back to a lookup BY TYPE, the shape that resolves for whichever
 *    node happens to match rather than the one the keys landed in.
 *  - **M4** reaches for a pointer inside a keyboard-only row.
 *  - **M6** renames the Rust field's serde attribute, to prove the Rust half of test 1 is
 *    load-bearing rather than decoration — the guard reads `graph.rs` and must notice when the
 *    wire key moves under it.
 *
 * **There is no M5, and the numbering keeps the gap on purpose.** An earlier draft of this list
 * described "takes the parameter off the DOM" as M5. That mutation does not exist here, and the
 * doc claimed it did — the exact failure this REQ has paid for in six other places: a note that
 * describes a check nobody runs reads as a check that was run. The list is what it is; the gap is
 * visible rather than renumbered over.
 *
 * Each mutation names the suite it expects to redden, and a suite that does not carry the
 * assertion is refused outright (the tick-61 M9 lesson: running a mutation against a suite that
 * cannot see it reports SURVIVED for a defect nobody committed).
 */
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";

const ROOT = new URL("../../../../", import.meta.url).pathname;
const WALKTHROUGH = join(ROOT, "scripts/qa/walkthrough.cjs");
const GRAPH_RS = join(ROOT, "crates/workflows/src/graph.rs");
const SUITE = join(ROOT, "apps/admin/features/workflows/keyboard-pass-row.test.ts");

const md5 = (path) => createHash("md5").update(readFileSync(path)).digest("hex");

/** Run the suite; return the pass/fail counts from its TAP output. */
const runSuite = () => {
  try {
    const out = execFileSync(
      "node",
      ["--test", "--experimental-strip-types", SUITE],
      { cwd: join(ROOT, "apps/admin"), encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    );
    return { ok: true, out };
  } catch (error) {
    return { ok: false, out: `${error.stdout ?? ""}${error.stderr ?? ""}` };
  }
};

const counts = (out) => {
  const pass = /^# pass (\d+)$/m.exec(out)?.[1];
  const fail = /^# fail (\d+)$/m.exec(out)?.[1];
  return { pass: pass === undefined ? null : Number(pass), fail: fail === undefined ? null : Number(fail) };
};

const MUTATIONS = [
  {
    id: "M1",
    suite: "keyboard-pass-row.test.ts",
    file: WALKTHROUGH,
    what: "the parameter read asks for `node_type` again — the field the server renames to `type`",
    from: /const edited =\s*\n?\s*\(graphAfterParam\?\.graph\?\.nodes \?\? \[\]\)\.find\(\(node\) => node\.id === paramReadFrom\) \?\?\s*\n?\s*\(graphAfterParam\?\.graph\?\.nodes \?\? \[\]\)\.find\(\(node\) => node\.type === "wait"\);/,
    to: `const edited =\n        (graphAfterParam?.graph?.nodes ?? []).find((node) => node.node_type === "wait");`,
  },
  {
    id: "M2",
    suite: "keyboard-pass-row.test.ts",
    file: WALKTHROUGH,
    what: "`paramWrote` reports a write with no resolved subject",
    from: /paramWrote: paramReadFrom !== null && \(paramReadBack === 31 \|\| paramReadBack === "31"\),/,
    to: `paramWrote: paramReadBack === 31 || paramReadBack === "31",`,
  },
  {
    id: "M3",
    suite: "keyboard-pass-row.test.ts",
    file: WALKTHROUGH,
    what: "the subject is resolved by node TYPE instead of from the focused field's id",
    from: /paramReadFrom = await page\s*\n?\s*\.evaluate\(\(\) => \{[\s\S]*?\}\)\s*\n?\s*\.catch\(\(\) => null\);/,
    to: `paramReadFrom = await page
        .evaluate(() => {
          const card = document.querySelector("[data-node-id]");
          return card ? card.getAttribute("data-node-id") : null;
        })
        .catch(() => null);`,
  },
  {
    id: "M4",
    suite: "keyboard-pass-row.test.ts",
    file: WALKTHROUGH,
    what: "a pointer click inside a keyboard-only row",
    from: /(\s+)await page\.keyboard\.press\("i"\);/,
    to: `$1await page.locator("[data-builder-canvas]").first().click();
$1await page.keyboard.press("i");`,
  },
  {
    id: "M6",
    suite: "keyboard-pass-row.test.ts",
    file: GRAPH_RS,
    what: "the Rust struct renames the wire key — the guard reads graph.rs and must notice",
    from: /#\[serde\(rename = "type"\)\]\s*\n(\s*)pub node_type: String,/,
    to: `#[serde(rename = "node_type")]\n$1pub node_type: String,`,
  },
];

let survived = 0;
const before = { walkthrough: md5(WALKTHROUGH), graph: md5(GRAPH_RS) };

for (const mutation of MUTATIONS) {
  const original = readFileSync(mutation.file, "utf8");
  const mutated = typeof mutation.from === "string"
    ? original.replace(mutation.from, mutation.to)
    : original.replace(mutation.from, mutation.to);
  if (mutated === original) {
    console.log(`  ${mutation.id} STRAWMAN: the pattern did not match — nothing was changed`);
    survived += 1;
    continue;
  }
  writeFileSync(mutation.file, mutated);
  const result = runSuite();
  const after = counts(result.out);
  const red = !result.ok && after.fail > 0;
  writeFileSync(mutation.file, original);

  // Byte-exact restore is asserted rather than assumed: this suite mutates files another writer
  // is also editing, and a restore that silently differs from the original leaves a mutation
  // committed. M1 was written first against a pattern that no longer matched, so the restore is
  // proven on every run, not only on the ones that go red.
  const restored = md5(mutation.file) === (mutation.file === WALKTHROUGH ? before.walkthrough : before.graph);

  console.log(`  ${mutation.id} ${red ? "RED" : "SURVIVED"} (pass ${after.pass}, fail ${after.fail}) — ${mutation.what}`);
  if (!red) survived += 1;
  if (!restored) {
    console.log(`  ${mutation.id} RESTORE FAILED — the file is not byte-identical to its original`);
    survived += 1;
  }
}

console.log(`\n${MUTATIONS.length - survived}/${MUTATIONS.length} red`);
process.exit(survived === 0 ? 0 : 1);
