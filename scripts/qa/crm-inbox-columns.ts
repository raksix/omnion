/**
 * The inbox's two missing columns, driven over the real module.
 *
 *   node --experimental-strip-types scripts/qa/crm-inbox-columns.ts <repo-root>
 *
 * ## What this exists for
 *
 * REQ-117's inbox row has read `Received, Contact, Source, Product interest, Owner, SLA,
 * Status, Duplicate hint` since the request was written. The table shipped **six** of them and
 * the two that are absent are the two a triage answer actually needs:
 *
 * * `Source` — the screen holds a source filter whose words have been there since the same
 *   tick, so "which source" was answerable by *filtering* and not by *reading a row*. An
 *   operator who filtered to one source and then walked the table could not tell what a row in
 *   front of them was without going back to the filter.
 * * `Duplicate hint` — the matched key and score have been rendered for a long time on
 *   `/crm/leads/duplicates`, one screen further along. The question "is this the same person
 *   who wrote to me last week?" is asked while standing on the inbox.
 *
 * ## Why the driver rather than a `.test.ts`
 *
 * Same reason as `crm-source-editor.ts`: `apps/admin` has no test runner, and the two rules
 * under test are pure functions of a row. Node's `--experimental-strip-types` erases the
 * annotations and keeps the values, which is everything a behaviour test of a pure function
 * needs — and it imports the **real** module by path, so this gate measures the product and
 * not a copy of it.
 *
 * ## The two traps this gate is shaped around
 *
 * 1. **`decision = "linked"` is not a duplicate.** It is the *good* outcome of the dedupe pass:
 *    the lead now belongs to an existing contact. A hint that fires on it tells an operator
 *    twenty ordinary leads are suspicious, so the verdict is checked for the word `duplicate`
 *    and not for "any decision".
 * 2. **The two columns carrying the fact are not redundant.** `status = "duplicate"` is the
 *    row's terminal state (the filter chip and every terminal-status gate read it);
 *    `decision = "duplicate"` is the dedupe pass's verdict, which a `keep separate` in the
 *    queue can leave behind on a row the status still calls duplicate. Reading only one of them
 *    produces a column that is right for one policy and silent for the other, so the gate
 *    asserts **each arm separately** and then the union.
 *
 * Exits 0 when every case holds, 1 otherwise — and prints each verdict either way, so a
 * driver that stopped running cannot be mistaken for a driver that passed.
 */
import {
  duplicateHint,
  isDuplicateVerdict,
  sourceLabel,
} from "../../apps/admin/lib/crm-intake.ts";

type Case = { name: string; ok: boolean; detail: string };

const cases: Case[] = [];
const check = (name: string, ok: boolean, detail = ""): void => {
  cases.push({ name, ok, detail });
};

const ROSTER = new Map<string, string>([
  ["11111111-1111-1111-1111-111111111111", "Website quote form"],
  ["22222222-2222-2222-2222-222222222222", "Pricing page popup"],
]);

// --- 1. Source column ------------------------------------------------------------------------
// The roster is what the screen already holds for the filter, so the cell needs no join. The
// three cases are: a source the roster knows, a source the roster does not, and no source at
// all — and they must give three different answers, which is what the assertions below pin.
check(
  "a lead from a known source is named, not keyed",
  sourceLabel(ROSTER, "11111111-1111-1111-1111-111111111111") === "Website quote form",
  `got ${String(sourceLabel(ROSTER, "11111111-1111-1111-1111-111111111111"))}`,
);
check(
  "a source the roster does not hold is named by its short id",
  // A deleted source, or one another tab created after this page loaded, is a real row with a
  // real cause. A dash would read as "no source" and send the operator to the form side.
  sourceLabel(ROSTER, "99999999-9999-9999-9999-999999999999") ===
    "Source 99999999",
  `got ${String(sourceLabel(ROSTER, "99999999-9999-9999-9999-999999999999"))}`,
);
check(
  "an imported lead with no source has an empty cell, not an invented one",
  // `source_id` is null for an imported lead for a reason that is not a fault; the screen
  // has no basis for the word "Imported" and a fabricated one is worse than blank.
  sourceLabel(ROSTER, null) === null && sourceLabel(ROSTER, undefined) === null,
  `got ${String(sourceLabel(ROSTER, null))}`,
);

// --- 2. Duplicate hint: the four stored shapes ------------------------------------------------
check(
  "a linked verdict is NOT a duplicate hint",
  // The trap above, asserted on the exact shape the dedupe pass writes on a match.
  duplicateHint({ decision: "linked", status: "assigned", dedupe_key: "email" }) === null &&
    isDuplicateVerdict({ decision: "linked", status: "assigned" }) === false,
  `got ${JSON.stringify(duplicateHint({ decision: "linked", status: "assigned" }))}`,
);
const byVerdict = duplicateHint({
  decision: "duplicate",
  status: "new",
  dedupe_key: "ayse@company.com",
  dedupe_score: 0.95,
});
check(
  "a stored duplicate verdict shows the matched key and its score",
  byVerdict?.from === "verdict" && byVerdict.key === "ayse@company.com" && byVerdict.score === 0.95,
  `got ${JSON.stringify(byVerdict)}`,
);
const byStatus = duplicateHint({ decision: null, status: "duplicate", dedupe_key: "5551234" });
check(
  "a duplicate with no stored verdict is still a duplicate, and says which column said so",
  // The arm a `status`-only reader would miss, and the reason the gate asserts both arms.
  byStatus?.from === "status" && byStatus.key === "5551234" && byStatus.score === null,
  `got ${JSON.stringify(byStatus)}`,
);
check(
  "a stored verdict wins over the status when both are present",
  // A row whose status is duplicate AND whose verdict is duplicate is the common case; the
  // hint must not read as "unknown provenance".
  duplicateHint({ decision: "duplicate", status: "duplicate" })?.from === "verdict",
  `got ${JSON.stringify(duplicateHint({ decision: "duplicate", status: "duplicate" }))}`,
);

// --- 3. the negative arms, which is where a permissive predicate hides -----------------------
for (const status of ["new", "assigned", "contacted", "qualified", "converted", "spam", "rejected"]) {
  check(
    `a "${status}" lead carries no duplicate hint`,
    duplicateHint({ decision: "created", status }) === null,
    `got ${JSON.stringify(duplicateHint({ decision: "created", status }))}`,
  );
}
check(
  "a row with no verdict at all is silent rather than guessing",
  duplicateHint({}) === null && duplicateHint({ decision: null, status: null }) === null,
  `got ${JSON.stringify(duplicateHint({}))}`,
);
check(
  "spam and rejected are not duplicates",
  // Both are "discarded" and neither is "somebody we already have" — collapsing them into one
  // hint is how a morning of triage ends up investigating twenty spam rows as a data problem.
  isDuplicateVerdict({ decision: "spam", status: "spam" }) === false &&
    isDuplicateVerdict({ decision: "rejected", status: "rejected" }) === false,
  "spam/rejected read as duplicates",
);
check(
  "a duplicate score of zero is a score, not an absence",
  // `0.0` is a real (terrible) confidence. A truthiness check would drop it and the cell would
  // read "matched somebody" with no number — the one thing the REQ forbids.
  duplicateHint({ decision: "duplicate", dedupe_score: 0 })?.score === 0,
  `got ${JSON.stringify(duplicateHint({ decision: "duplicate", dedupe_score: 0 }))}`,
);

// --- report ------------------------------------------------------------------------------------
let failures = 0;
for (const item of cases) {
  if (item.ok) {
    console.log(`  ok   ${item.name}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${item.name}${item.detail ? ` — ${item.detail}` : ""}`);
  }
}
console.log(`crm inbox columns: ${cases.length - failures}/${cases.length}`);
process.exit(failures === 0 ? 0 : 1);