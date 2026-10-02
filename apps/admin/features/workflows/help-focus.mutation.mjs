/**
 * Mutation harness for the ⌘/ focus-restore guards (`builder-help-dialog.test.ts`, tick 78).
 *
 * ## Why this file exists
 *
 * The defect it guards is a *focus* defect, and nothing in a Node test can observe focus: there
 * is no `document`, no layout, no focus ring. The guards are therefore source assertions, and a
 * source assertion that is written loosely is worse than no guard — it is green, it costs a
 * second of CI, and it stops the next writer from deleting the fix. Tick 77 measured that in
 * this very REQ: a case gated on a constant `false` passed 11/11 on a `Tab` that did nothing.
 *
 * So each mutation below removes ONE part of the fix and names the assertion that must go red.
 * A mutation that still passes is not a weak guard, it is a guard that is not measuring the
 * thing it claims — and it is reported as a failure of this harness, loudly, because that is
 * the signal worth stopping on.
 *
 * The run also asserts the file is restored **byte-exact** (md5) on the way out, including on
 * the failing paths, so a red mutation can never be left behind in the worktree as if it were
 * the shipped code.
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const VIEW = fileURLToPath(new URL("./builder-view.tsx", import.meta.url));
const TEST = fileURLToPath(new URL("./builder-help-dialog.test.ts", import.meta.url));

const original = readFileSync(VIEW, "utf8");
const md5 = (text) => createHash("md5").update(text).digest("hex");
const pristine = md5(original);

/** Each mutation names the assertion it must turn red. */
const MUTATIONS = [
  {
    id: "M1",
    what: "never capture the opener, so there is nothing to return to",
    find: `if (!helpState.current.open) {
          helpReturnFocus.current =
            (document.activeElement as HTMLElement | null) ?? canvasRef.current;
        }
`,
    replace: "",
    expect: "opening the list must remember where focus came from",
  },
  {
    id: "M2",
    what: "capture the opener on every press, so a CLOSE overwrites it with the dialog button",
    find: `if (!helpState.current.open) {
          helpReturnFocus.current =`,
    replace: `if (true) {
          helpReturnFocus.current =`,
    expect: "the opener must be captured only on the way OPEN",
  },
  {
    id: "M3",
    what: "restore focus without re-checking that the remembered element still exists",
    find: "const target = back.isConnected ? back : canvasRef.current;",
    replace: "const target = back;",
    expect: "the restore must fall back to the canvas when the remembered element is gone",
  },
  {
    id: "M4",
    what: "restore focus INLINE in the close handler — the version that reads correct and fails",
    find: `const closeHelp = useCallback(() => {
    setHelpOpen(false);
  }, []);`,
    replace: `const closeHelp = useCallback(() => {
    setHelpOpen(false);
    const back = helpReturnFocus.current;
    helpReturnFocus.current = null;
    (back.isConnected ? back : canvasRef.current)?.focus();
  }, []);`,
    expect: "the close handler must not focus inline",
  },
  {
    id: "M5",
    what: "drop the effect entirely, leaving nothing to restore focus",
    find: `useEffect(() => {
    if (helpOpen) {
      return;
    }
    const back = helpReturnFocus.current;
    if (!back) {
      return;
    }
    helpReturnFocus.current = null;
    const target = back.isConnected ? back : canvasRef.current;
    target?.focus();
  }, [helpOpen]);`,
    replace: "",
    // **Two acceptable reds, and the reason is the shape of the mutation rather than a softened
    // bar.** Deleting the effect does not disable one assertion — it deletes the *text* every
    // assertion in this test reads out of that effect, so the FIRST of them to run is the one
    // that reports (here the `isConnected` fallback, which sits inside the effect and is
    // checked before the effect's own shape). Accepting the set is not "any red will do": each
    // entry is a specific assertion whose subject the mutation deletes, and a red on any
    // *other* message still fails the harness. A single expected string here would have made
    // the harness report a WEAK guard for a guard that is doing its job.
    expect: [
      "the focus restore must run inside a useEffect keyed on helpOpen",
      "the restore must fall back to the canvas when the remembered element is gone",
    ],
  },
  {
    id: "M6",
    what: "let the effect steal focus while the list is OPEN (drop its guard)",
    find: `useEffect(() => {
    if (helpOpen) {
      return;
    }`,
    replace: `useEffect(() => {
    if (false) {
      return;
    }`,
    expect: "the restore must be gated on the list being CLOSED",
  },
  {
    id: "M7",
    what: "close one exit directly, bypassing the shared closeHelp",
    find: "onClick={closeHelp}\n          data-builder-help-backdrop",
    replace: "onClick={() => setHelpOpen(false)}\n          data-builder-help-backdrop",
    expect: "the chord, the dialog's Escape and the backdrop/button all close through closeHelp()",
  },
  {
    id: "M8",
    what: "publish the state from an effect, so the opening keydown reads the PREVIOUS render",
    find: "  helpState.current.open = helpOpen;\n  helpState.current.close = closeHelp;",
    replace: "  // published nowhere",
    expect: "the ref must carry the current open state",
  },
  {
    id: "M9",
    what: "close through the ref's no-op, so the canvas Escape path cannot close the list",
    find: `if (helpState.current.open) {
          event.preventDefault();
          helpState.current.close();`,
    replace: `if (helpState.current.open) {
          event.preventDefault();`,
    expect: "the canvas Escape path must read the current state from the ref and close through it",
  },
];

const runTest = () => {
  try {
    execFileSync(
      process.execPath,
      ["--test", "--experimental-strip-types", TEST],
      { stdio: ["ignore", "pipe", "pipe"], timeout: 240_000 },
    );
    return { ok: true, out: "" };
  } catch (error) {
    return {
      ok: false,
      out: `${error.stdout ?? ""}${error.stderr ?? ""}`,
    };
  }
};

const expectedMessages = (expect) => (Array.isArray(expect) ? expect : [expect]);

let failures = 0;
for (const mutation of MUTATIONS) {
  if (!original.includes(mutation.find)) {
    console.log(`SKIP ${mutation.id}: anchor not found — the file moved under the guard`);
    failures += 1;
    continue;
  }
  const mutated = original.replace(mutation.find, mutation.replace);
  writeFileSync(VIEW, mutated);
  let result;
  try {
    result = runTest();
  } finally {
    writeFileSync(VIEW, original);
  }
  // The restore must be byte-exact, not merely equivalent.
  const restored = md5(readFileSync(VIEW, "utf8"));
  if (restored !== pristine) {
    console.log(`FAIL ${mutation.id}: the file was NOT restored byte-exact`);
    failures += 1;
    continue;
  }
  if (result.ok) {
    console.log(`FAIL ${mutation.id}: STILL GREEN — "${mutation.what}"`);
    failures += 1;
    continue;
  }
  const wanted = expectedMessages(mutation.expect);
  const named = wanted.find((message) => result.out.includes(message));
  console.log(
    `${named ? "ok  " : "WEAK"} ${mutation.id}: ${mutation.what}` +
      (named ? "" : " — went red, but not on any named assertion"),
  );
  if (!named) {
    failures += 1;
  }
}

// The pristine tree must be green too: a guard set that only proves itself against mutations is
// not evidence that the shipped code passes.
const clean = runTest();
console.log(clean.ok ? "ok   baseline: the unmutated file passes" : "FAIL baseline");
if (!clean.ok) {
  failures += 1;
}

console.log(`\n${MUTATIONS.length - failures}/${MUTATIONS.length} mutations behaved as named`);
process.exit(failures === 0 ? 0 : 1);
