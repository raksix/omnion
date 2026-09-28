#!/usr/bin/env node
/*
 * The deliberate-failure gate, exercised without a browser.
 *
 * The gate decides what the QA report calls a defect, and it decides it from two rules that only
 * interact: an allowance is positional (it opens when the pass registers) and it is closed (it
 * stops excusing when the pass says the provocation is over). A regression here is silent in the
 * worst direction — either a provoked 500 is filed as a defect, or a real one is swallowed — so it
 * is tested here rather than discovered in a 50-minute run.
 */
const assert = require("node:assert/strict");

// ---- the gate, copied from walkthrough.cjs so this test cannot drift from it ----------------
const expectedRefusals = [];
const netFailures = [];
const consoleLog = [];

function expectRefusal(match, reason, statuses) {
  expectedRefusals.push({ match, reason, statuses, consoleFrom: consoleLog.length, netFrom: netFailures.length, closed: false, claimed: 0 });
}
function endRefusalWindow(match) {
  // The window is bounded by *position*, not by the moment the roll-up happens to read it. A
  // boolean `closed` read at roll-up time would retract the allowance from entries the pass had
  // already provoked — the provocation really did happen, so the report must not call it a defect
  // afterwards. The pass says "everything up to here is mine" and that stays true.
  const entry = [...expectedRefusals].reverse().find((e) => e.match === match && e.netTo === undefined);
  if (entry) {
    entry.netTo = netFailures.length;
    entry.consoleTo = consoleLog.length;
  }
  return entry;
}
function allowedStatus(n) {
  return n.status === 0 || !n.status || [400, 401, 403].includes(n.status) || (n.status >= 500 && n.status < 600);
}
function netRollup() {
  const out = [];
  // Each entry is settled exactly once: the counter is read, not re-incremented, because this
  // helper is called repeatedly by the cases below and a roll-up that can be run twice must not
  // double-count what it swallowed.
  for (const entry of expectedRefusals) entry.claimed = 0;
  netFailures.forEach((n, index) => {
    const deliberate = expectedRefusals.find(
      (entry) =>
        index >= entry.netFrom &&
        (entry.netTo === undefined || index < entry.netTo) &&
        String(n.url || "").includes(entry.match) &&
        (entry.statuses ? entry.statuses.includes(n.status) : allowedStatus(n)),
    );
    if (deliberate) { deliberate.claimed += 1; out.push("refused"); return; }
    out.push("finding");
  });
  return out;
}
function consoleRollup() {
  const out = [];
  for (const entry of expectedRefusals) entry.claimed = 0;
  consoleLog.forEach((f, index) => {
    const lineStatus = Number((f.text.match(/status of (\d{3})/) || [])[1] || 0);
    const deliberate = /status of (40[013]|5\d\d)|ERR_CONNECTION_REFUSED|ERR_NETWORK/.test(f.text)
      ? expectedRefusals.find(
          (entry) =>
            index >= entry.consoleFrom &&
            (entry.consoleTo === undefined || index < entry.consoleTo) &&
            (entry.statuses ? entry.statuses.includes(lineStatus) : true),
        )
      : null;
    if (deliberate) { deliberate.claimed += 1; out.push("refused"); return; }
    out.push("finding");
  });
  return out;
}

// ---- the cases ------------------------------------------------------------------------------

// 1. the whole point: a provoked 500 is not a defect
expectRefusal("/ai/providers", "provoked");
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["refused"], "a registered 500 is excused");
assert.equal(expectedRefusals[0].claimed, 1);

// 2. one provocation, several requests — the screen loads twice, all belong to it
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["refused", "refused"], "a window covers every request it provoked");
assert.equal(expectedRefusals[0].claimed, 2, "each swallowed request is counted");

// 3. the window is positional: a failure from BEFORE the registration is still a defect
expectedRefusals.length = 0;
netFailures.length = 0;
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
expectRefusal("/ai/providers", "provoked");
assert.deepEqual(netRollup(), ["finding"], "an earlier failure is not excused by a later registration");

// 4. the window closes: the next failure is a real one again
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["finding", "refused"]);
endRefusalWindow("/ai/providers");
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["finding", "refused", "finding"], "a closed window excuses nothing");

// 5. the match is by URL, so an unrelated failure in the same window is not swallowed
expectedRefusals.length = 0;
netFailures.length = 0;
expectRefusal("/ai/providers", "provoked");
netFailures.push({ url: "/api/v1/ai/models", status: 500 });
assert.deepEqual(netRollup(), ["finding"], "a different endpoint is never excused by another pass's window");

// 6. a status the gate does not allow is never excused even inside a window
expectedRefusals.length = 0;
netFailures.length = 0;
expectRefusal("/ai/providers", "provoked");
netFailures.push({ url: "/api/v1/ai/providers", status: 404 });
assert.deepEqual(netRollup(), ["finding"], "a 404 is not a status a pass may provoke");

// 7. a dropped connection has no status at all
expectedRefusals.length = 0;
netFailures.length = 0;
expectRefusal("/ai/providers", "provoked");
netFailures.push({ url: "/api/v1/ai/providers", status: 0, error: "net::ERR_CONNECTION_REFUSED" });
assert.deepEqual(netRollup(), ["refused"], "a refused connection is excusable");

// 8. the console side: 500 and a refused connection are matched, a 404 is not
expectedRefusals.length = 0;
netFailures.length = 0;
consoleLog.length = 0;
expectRefusal("/ai/providers", "provoked");
consoleLog.push({ text: "Failed to load resource: the server responded with a status of 500 (Internal Server Error)" });
consoleLog.push({ text: "Failed to load resource: net::ERR_CONNECTION_REFUSED" });
assert.deepEqual(consoleRollup(), ["refused", "refused"], "a provoked 500 and a dropped connection are excused on the console too");
endRefusalWindow("/ai/providers");
consoleLog.push({ text: "Failed to load resource: the server responded with a status of 404 (Not Found)" });
assert.deepEqual(consoleRollup(), ["refused", "refused", "finding"], "a 404 console line is never excused");

// 9. an unregistered 500 anywhere is still a high finding — the allowance is opt-in
expectedRefusals.length = 0;
netFailures.length = 0;
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["finding"], "with no registration, a 500 is a finding");

// 10. a narrowed registration excuses only what it named. The generic form filler types
// placeholders and submits, so its POST is refused with a 400 — and a 500 in that same window is
// the API crashing on input it should have rejected, which is a defect the report must keep.
expectedRefusals.length = 0;
netFailures.length = 0;
expectRefusal("/api/v1/", "a placeholder form is refused", [400, 401, 403, 422]);
netFailures.push({ url: "/api/v1/ai/providers", status: 400 });
assert.deepEqual(netRollup(), ["refused"], "the 400 the filler provoked is excused");
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(
  netRollup(),
  ["refused", "finding"],
  "a 500 inside a 4xx-only window is still a finding — narrowing must not become a rubber stamp",
);

// 11. and the same narrowing applies to the console line, not only to the request
expectedRefusals.length = 0;
netFailures.length = 0;
consoleLog.length = 0;
expectRefusal("/api/v1/", "a placeholder form is refused", [400, 401, 403, 422]);
consoleLog.push({ text: "Failed to load resource: the server responded with a status of 400 (Bad Request)" });
consoleLog.push({ text: "Failed to load resource: the server responded with a status of 500 (Internal Server Error)" });
assert.deepEqual(
  consoleRollup(),
  ["refused", "finding"],
  "the console side honours the same narrowed vocabulary",
);

// 12. a default registration is unchanged by the narrowing: a 5xx provocation still excuses a 500
expectedRefusals.length = 0;
netFailures.length = 0;
expectRefusal("/ai/providers", "provoked");
netFailures.push({ url: "/api/v1/ai/providers", status: 500 });
assert.deepEqual(netRollup(), ["refused"], "a registration with no vocabulary still excuses a 500");

console.log("qa-refusal-gate: 12/12 PASS");
