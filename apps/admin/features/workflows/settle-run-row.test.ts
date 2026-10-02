/**
 * `settleRun` is the helper that decides whether the run-from-here note is reading a
 * finished run or one that is still in flight. This file executes it.
 *
 * ## What it found
 *
 * The helper's own doc comment described a stop condition the code did not have:
 *
 * > The stop condition is *the run stopped moving*, never *N milliseconds passed* … It also
 * > waits for the run to stop MOVING **after having observed it start**, so a run that never
 * > starts is the failure it reports rather than a success.
 *
 * The code asked for two identical readings and nothing else. Those two conditions are not
 * the same condition, and the difference is the whole point: **a run waiting to be claimed
 * has not moved either.** Two polls 500 ms apart on a freshly accepted run both read
 * `pending|1:pending,2:pending`, the bytes are identical, and the old helper returned
 * `settled: true` — reporting a run that had not executed a single step as finished.
 *
 * That is tick 61's reading, verbatim: `startedFrom: "wait-3"`, `statuses: ["pending"]`,
 * `pillsPainted: 0`, `inRunButNotPainted: [wait-3, act-3, end-3]`. The helper was written to
 * kill that race and re-created it. Every gate below the run in that note reads the same
 * `steps` array, so a run that never started produces five readings that each say "the
 * product is missing this" and none of them can say why.
 *
 * ## Why the fix is not "wait longer"
 *
 * Longer is the same guess the 2500 ms sleep was, and it fails on exactly the box this pass
 * ran on: one shared machine, three sibling passes, a run that claims a step somewhere
 * between 2.5 s and 9 s. `settleGraph` — the graph-side twin, thirty lines below this helper —
 * already required *stability AND movement*, and its own comment says why: a graph that has
 * not been written yet is stable. Two helpers with one job, one requiring the witness and
 * one not, is the tick-87 shape one level down: a gate that reports green on the state it
 * was written to catch.
 *
 * ## Why these tests RUN the helper instead of reading its source
 *
 * Every other instrument test in this directory asserts on source text, and this REQ has a
 * documented run of those assertions being satisfied by a *mention* of the construct rather
 * than a use of it. A source check here would pass on a helper whose condition is commented
 * out, renamed or moved behind a caller that never reaches it. So the function is extracted
 * by brace matching and evaluated, and the assertions are about what it RETURNS.
 *
 * The extraction is itself asserted. A brace matcher that stops early or overshoots yields a
 * `SyntaxError` — loud, not silent — but a matcher that stops at the *first* `}` inside a
 * template literal or a default object would produce a different function body, so the
 * matched length and the presence of the closing brace of the function are both checked.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const WALKTHROUGH = readFileSync(
  new URL("../../../../scripts/qa/walkthrough.cjs", import.meta.url),
  "utf8",
);

/**
 * Pull `settleRun`'s source out of the walkthrough by matching braces from its signature.
 *
 * The walkthrough is CommonJS with no exports, so the helper cannot be imported; executing the
 * real text is the alternative to a source assertion, and it is worth the twenty lines. Brace
 * counting is enough here because the body contains no brace-bearing string, template literal
 * or regex — asserted below rather than assumed, because that is the assumption a brace
 * matcher makes and this REQ keeps paying for unstated ones.
 */
function extractFunction(source: string, name: string): string {
  const signature = `async function ${name}(`;
  const start = source.indexOf(signature);
  assert.notEqual(start, -1, `${name} is not in the walkthrough — the helper was renamed or removed`);

  // Step one: skip the PARAMETER LIST. This is the failure the first draft of this file had,
  // and it is the same family the directory documents twice already ("window ends at the wrong
  // place") arriving through the door of a brace matcher. `settleRun`'s third parameter is
  // `{ attempts = 40, interval = 500 } = {}` — a brace pair inside the signature — so a matcher
  // that counts braces from the opening paren returns at THAT `}`, and `new Function` is handed
  // a function with a parameter list and no body. It fails loudly, which is the only reason it
  // was found in a minute rather than shipping: the extracted "body" was 84 characters, not
  // 400, and the length assertion below is what made the number visible.
  const parenStart = source.indexOf("(", start);
  let parenDepth = 0;
  let bodyStart = -1;
  for (let at = parenStart; at < source.length; at += 1) {
    const character = source[at];
    if (character === "(") parenDepth += 1;
    else if (character === ")") {
      parenDepth -= 1;
      if (parenDepth === 0) {
        bodyStart = at + 1;
        break;
      }
    }
  }
  assert.notEqual(bodyStart, -1, `${name}'s parameter list never closes`);
  // The `)` is followed by a space, so the assertion has to skip whitespace rather than
  // compare the very next character. Asserting "the next character is `{`" is a claim about
  // formatting, and the extraction is a claim about the body.
  let braceAt = bodyStart;
  while (braceAt < source.length && /\s/.test(source[braceAt])) braceAt += 1;
  assert.equal(source[braceAt], "{", `${name} is not followed by a body — the extraction above is reading the wrong function`);

  // Step two: from the body's own brace, match to its partner.
  let depth = 0;
  for (let at = braceAt; at < source.length; at += 1) {
    const character = source[at];
    if (character === "{") depth += 1;
    else if (character === "}") {
      depth -= 1;
      if (depth === 0) {
        const text = source.slice(start, at + 1);
        // `match` is nullable under `strict`, and a null here would throw a TypeError instead of
        // naming the construct — which is the same "a loud crash is not a diagnosis" shape as
        // the extractor itself.
        assert.equal(text.match(/\{/g)?.length ?? 0, text.match(/\}/g)?.length ?? 0, "the extracted body is unbalanced");
        return text;
      }
    }
  }
  throw new Error(`${name} has no closing brace in the walkthrough`);
}

const SETTLE_RUN_SOURCE = extractFunction(WALKTHROUGH, "settleRun");

// eslint-disable-next-line no-new-func -- the point of the file: run the harness's own helper
const settleRun = new Function(`return (${SETTLE_RUN_SOURCE});`)();

/** The shape the walkthrough's `readRun` resolves, narrowed to what the helper reads. */
type FakeRun = {
  status: string;
  steps: { step_no: number; status: string }[];
};

/** A run as the walkthrough's `readRun` builds it. */
function run(status: string, stepStatuses: string[]): FakeRun {
  return {
    status,
    steps: stepStatuses.map((stepStatus: string, index: number) => ({ step_no: index + 1, status: stepStatus })),
  };
}

/** Feed the helper a fixed sequence, then the last value forever. */
function reader(sequence: (FakeRun | null)[]): () => Promise<FakeRun | null> {
  let at = 0;
  return async () => {
    const value = sequence[Math.min(at, sequence.length - 1)];
    at += 1;
    return value;
  };
}

/** Reads are instant here; the interval only has to be short enough to keep the test quick. */
const FAST = { attempts: 8, interval: 1 };

test("a run the engine never claimed is NOT settled", async () => {
  // The tick-61 reading, exactly: accepted by the server, no step claimed, and identical on
  // every poll. Two identical readings is the old stop condition, and it returned `true`.
  const result = await settleRun(null, reader([run("pending", ["pending", "pending"])]), FAST);

  assert.equal(result.settled, false, "a run that never left the queue cannot be reported as finished");
  assert.equal(result.started, false, "and it must say it never started, rather than leaving that to be inferred");
});

test("the OLD stop condition reports that same run as settled — the control", async () => {
  // The control that makes the test above a measurement. It is the previous implementation
  // verbatim, and it has to be green-to-be-wrong on the same input, or the first test is
  // asserting a property of the test rather than of the fix.
  const oldStopCondition = async (readRun: () => Promise<FakeRun | null>) => {
    const read = async () => {
      const current = await readRun();
      return `${current?.status ?? "?"}|${(current?.steps ?? [])
        .map((step: { step_no: number; status: string }) => `${step.step_no}:${step.status}`)
        .join(",")}`;
    };
    let previous = null;
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const current = await read();
      if (current === previous) return { settled: true };
      previous = current;
      await new Promise((resolve) => setTimeout(resolve, 1));
    }
    return { settled: false };
  };

  const result = await oldStopCondition(reader([run("pending", ["pending", "pending"])]));

  assert.equal(result.settled, true, "the old logic is expected to be wrong here — if it is not, this control is not testing the old logic");
});

test("a run that starts and then finishes IS settled and finished", async () => {
  const result = await settleRun(
    null,
    reader([run("pending", ["pending", "pending"]), run("running", ["running", "pending"]), run("succeeded", ["succeeded", "skipped"])]),
    FAST,
  );

  assert.equal(result.started, true, "the run was observed moving");
  assert.equal(result.settled, true, "so a later identical reading is a stopped run");
  assert.equal(result.finished, true, "and this one actually reached a terminal state");
});

test("a HUNG run reads settled but NOT finished — and this is why there are three fields", async () => {
  // The second hole, found by this file rather than by reading. A run the engine claims and
  // then wedges leaves `running|1:running,2:pending` on every poll, so the two readings are
  // identical and the helper reports `settled: true` — correctly, by its own contract ("the
  // run stopped moving"). Every gate in the note then reads a half-finished `steps` array and
  // reports it as a finished run: pills missing, prefix unwritten, panel closed, all of them
  // describing a product that is working.
  //
  // So `settled` is not the word the note needs. It is the WORD the helper was documented
  // with, and the distinction between "stopped" and "finished" is the whole reading.
  const result = await settleRun(null, reader([run("running", ["running", "pending"])]), FAST);

  assert.equal(result.settled, true, "it did stop moving — that is the helper's documented contract");
  assert.equal(result.finished, false, "and that is the fact that separates a hung run from a finished one");
  assert.equal(result.started, true, "it started, so it is neither of the never-started case");
});

test("the note emits all three, because a two-field answer cannot say which failure this is", async () => {
  // Three runs the note must tell apart, and before this tick two fields could not:
  //   never claimed  ->  settled: false, started: false   (the tick-61 reading)
  //   claimed, hung  ->  settled: true,  started: true    (a half-written steps array)
  //   finished       ->  settled: true,  started: true, finished: true
  assert.match(WALKTHROUGH, /runSettled\s*=\s*settled\.settled/);
  assert.match(WALKTHROUGH, /runStarted\s*=\s*settled\.started/);
  assert.match(WALKTHROUGH, /runFinished\s*=\s*settled\.finished/);

  const noteIndex = WALKTHROUGH.indexOf("runSettled,");
  assert.notEqual(noteIndex, -1, "the note no longer emits runSettled at all");
  const window = WALKTHROUGH.slice(noteIndex, noteIndex + 60);
  assert.match(window, /runStarted/, "runStarted is assigned but never reported");
  assert.match(window, /runFinished/, "runFinished is assigned but never reported — a hung run and a finished one are then the same note");
});

test("a run that never produced a record at all is not settled", async () => {
  // `readRun` answers `null` when the execution list is empty, which is what a click that did
  // nothing looks like. Two `null` readings are identical too.
  const result = await settleRun(null, reader([null]), FAST);

  assert.equal(result.settled, false);
  assert.equal(result.started, false);
});

test("a run whose steps all read pending but whose status is terminal IS settled", async () => {
  // The witness is not only a step status: `settleRun` reads the run's own status, and a
  // terminal status is the engine saying it finished even if a step row was never updated.
  const result = await settleRun(null, reader([run("succeeded", ["pending", "pending"])]), FAST);

  assert.equal(result.started, true, "a terminal run status is evidence the engine took it");
});

test("the extraction is an extraction and not a truncation", async () => {
  assert.match(SETTLE_RUN_SOURCE, /^\s*async function settleRun\(/);
  assert.match(SETTLE_RUN_SOURCE, /hasStarted/, "the witness check is inside the extracted body");
  assert.ok(SETTLE_RUN_SOURCE.length > 400, `the extracted body is suspiciously short (${SETTLE_RUN_SOURCE.length} chars) — the matcher probably stopped early`);
});
