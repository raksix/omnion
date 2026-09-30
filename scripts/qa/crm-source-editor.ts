/**
 * The source editor's refresh rule, driven over the real module.
 *
 *   node --experimental-strip-types scripts/qa/crm-source-editor.ts <repo-root>
 *
 * ## Why a driver and not a `.test.ts`
 *
 * `apps/admin` has no test runner — no vitest, no jest, not one `*.test.ts` — and the function
 * under test is twenty lines of pure code with no imports. Adding a runner to a workspace that
 * has chosen not to have one is a bigger change than the fix it would guard, and the panel's
 * only real gate is `tsc --noEmit` plus the browser walkthrough. So the rule is driven directly:
 * Node's `--experimental-strip-types` erases the annotations and keeps the values, which is
 * everything a behaviour test of a pure function needs.
 *
 * The point of the exercise is that the rule now has a **reader** which can fail. Before this,
 * the same decision was an inline `setState` callback that no gate could reach, which is how it
 * came to be the exact inverse of the comment above it for the module's whole life.
 *
 * Exits 0 when every case holds, 1 on the first failure — and prints each verdict either way,
 * so a driver that stopped running cannot be mistaken for a driver that passed.
 */
import { editorAfterRefresh } from "../../apps/admin/lib/crm-intake.ts";

type Case = { name: string; ok: boolean; detail: string };

const cases: Case[] = [];
const check = (name: string, ok: boolean, detail = ""): void => {
  cases.push({ name, ok, detail });
};

/** The editor state, narrowed to the field the rule reads — as it is in the screen. */
const held = { id: "src-1", name: "Website quote form (half typed)" };

/** A list row, as `fetchIntakeSources` returns it. */
const rows = [
  { id: "src-1", name: "Website quote form" },
  { id: "src-2", name: "Support form" },
];

// 1 · A closed editor has nothing to protect, and must stay closed. The old code returned
// `null` here, which is *correct by accident* — and the reason the two arms were worth
// separating is that its sibling arm was wrong.
{
  const verdict = editorAfterRefresh(null, rows);
  check("a closed editor stays closed", verdict.action === "keep" && verdict.editing === null);
}

// 2 · THE defect. An open editor keeps exactly the object it was holding, by identity — not a
// row rebuilt from the server. A rule that answered "keep" with the *fetched* row would pass
// every check below that only looks at `action`, so this is asserted on the value.
{
  const verdict = editorAfterRefresh(held, rows);
  check(
    "an open editor keeps its in-flight edits",
    verdict.action === "keep" && verdict.editing === held,
    verdict.action === "keep" && verdict.editing !== held
      ? "it returned a DIFFERENT object: the in-flight edits were replaced"
      : `action=${verdict.action}`,
  );
  check(
    "the kept object is byte-for-byte the one the operator was typing into",
    JSON.stringify(verdict.editing) === JSON.stringify(held),
    "the value changed even though the action says keep",
  );
}

// 3 · The one case that must close. Somebody deleted the source on another tab: keeping the
// draft would leave an editor writing to a row that is not there, and the 404 it earns reads
// as a broken screen rather than as "this source is gone".
{
  const verdict = editorAfterRefresh(held, [{ id: "src-2", name: "Support form" }]);
  check("an editor whose source vanished closes", verdict.action === "close");
}

// 4 · A negative control. An id belonging to another organization is not this editor's source,
// and an editor must never adopt it. This is the shape the tenancy guard takes everywhere else
// on this branch: the same answer for "not yours" as for "not there".
{
  const verdict = editorAfterRefresh({ id: "theirs" }, [{ id: "ours" }]);
  check("another tenant's source never adopts this editor", verdict.action === "close");
}

// 5 · A negative control on the *matching* half. A rule that compared names, or that used a
// prefix (`startsWith`), would pass 1–4 and then close the editor of a source whose name
// happens to be a prefix of another's.
{
  const near = editorAfterRefresh({ id: "src-1" }, [{ id: "src-11", name: "Website quote form" }]);
  const exact = editorAfterRefresh({ id: "src-1" }, [{ id: "src-1", name: "anything else entirely" }]);
  check(
    "an id match on name alone never counts",
    near.action === "close" && exact.action === "keep",
    `near-miss said ${near.action}, exact said ${exact.action}`,
  );
}

// 6 · An empty list with an open editor. Not exotic: it is what the screen shows the instant a
// delete succeeds, and what an installation whose only source was removed elsewhere shows.
{
  const verdict = editorAfterRefresh(held, []);
  check("an empty list closes an open editor", verdict.action === "close");
}

let failed = 0;
for (const entry of cases) {
  if (entry.ok) {
    console.log(`PASS ${entry.name}`);
  } else {
    failed += 1;
    console.log(`FAIL ${entry.name}${entry.detail ? ` — ${entry.detail}` : ""}`);
  }
}
console.log(`${cases.length} case(s), ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
