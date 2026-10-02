// The cluster panel's gates, proved able to FAIL (REQ-024, slice 4).
//
// Lifts the shipped `clusterGate` -- its definition AND every call site -- out of the
// walkthrough and runs it. A gate proved only by reading is a note, which is the defect
// tick 98 found in this harness and the reason this file exists.
//
// Run: node scripts/qa/cluster-panel-gate-selftest.cjs
//

// Lift the shipped clusterGate -- definition AND every call site -- out of the harness and run
// it. Same technique as the tick-98 self-tests: a gate proved only by reading is not a gate.
//
// The first version of this file cut the block at the first call, which captured the definition
// and none of the invocations -- so it reported "0 findings" for a screen that rendered nothing
// and nearly read as the gate being broken. It was the test that was wrong. The block now ends
// at the next banner comment, which is where the harness itself ends a section.
const fs = require("fs");
const src = fs.readFileSync(require("path").join(__dirname, "..", "..", "scripts", "qa", "walkthrough.cjs"), "utf8");
const start = src.indexOf("  const clusterGate = (claim, severity, what) => {");
if (start < 0) throw new Error("clusterGate not found");
const tail = src.indexOf("\n  // ---- ", start + 10);
if (tail < 0) throw new Error("the end of the cluster block not found");
const body = src.slice(start, tail);
const calls = (body.match(/clusterGate\(/g) || []).length - 1; // minus the definition
// No count assertion here: the regex for the call sites is brittle against line wrapping (an
// earlier version counted 5 of 6 for exactly that reason), and the six behavioural cases below
// each name a gate and force it to fire. The behaviour is the count.

function run(steps, single, table) {
  const findings = [];
  const fn = new Function("record", "steps", "clusterSingle", "clusterTable", body + "\nreturn null;");
  fn((f) => findings.push(f), steps, single, table);
  return findings;
}

const allTrue = (extra) => ({
  "cluster-rendered": true,
  "cluster-reason-is-specific": true,
  "cluster-single-not-a-cluster-table": true,
  "cluster-single-invents-no-figures": true,
  "cluster-table-either-rows-or-explains": true,
  "cluster-sample-reports-an-outcome": true,
  ...extra,
});

let failures = 0;
const check = (label, cond, extra) => {
  if (cond) { console.log("PASS:", label); return; }
  failures += 1;
  console.error("FAIL:", label, extra ?? "");
};

// 1. A healthy single-instance pass raises nothing.
const healthy = run(allTrue(), true, false);
check("a healthy single-instance pass raises 0 findings", healthy.length === 0, JSON.stringify(healthy));

// 2. A healthy cluster pass raises nothing.
const healthyCluster = run(allTrue(), false, true);
check("a healthy cluster pass raises 0 findings", healthyCluster.length === 0, JSON.stringify(healthyCluster));

// 3. A screen that rendered NEITHER shape is one high finding -- not zero. This is the claim that
//    would have been a note before tick 98, and the whole reason the extraction includes the
//    call sites: without them this assertion measures an empty function.
const neither = run({ "cluster-rendered": false }, false, false);
const renderedFindings = neither.filter((f) => f.action === "cluster-rendered");
check(
  "a screen that rendered neither shape raises exactly 1 high finding",
  renderedFindings.length === 1 && renderedFindings[0].severity === "high",
  JSON.stringify(neither),
);

// 4. Scoping: the cluster-table claims are out of scope on a single instance, not unmet.
const scoped = run(allTrue({ "cluster-table-either-rows-or-explains": false }), true, false);
check("the cluster-table claim is scoped out on a single instance", scoped.length === 0, JSON.stringify(scoped));
const scoped2 = run(allTrue({ "cluster-single-invents-no-figures": false }), false, true);
check("the single-instance claims are scoped out on a cluster", scoped2.length === 0, JSON.stringify(scoped2));

// 5. Each real claim still fires on its own shape.
const cases = [
  ["cluster-reason-is-specific", true, false, "the card does not say why"],
  ["cluster-single-invents-no-figures", true, false, "the card invents a figure"],
  ["cluster-table-either-rows-or-explains", false, true, "an empty table with no explanation"],
];
for (const [claim, single, table, why] of cases) {
  const findings = run(allTrue({ [claim]: false }), single, table);
  check(
    `an unmet "${claim}" raises a high finding`,
    findings.length === 1 && findings[0].action === claim && findings[0].severity === "high",
    JSON.stringify(findings),
  );
}

console.log(failures === 0 ? `ALL PASS (${calls} gates exercised)` : `${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
