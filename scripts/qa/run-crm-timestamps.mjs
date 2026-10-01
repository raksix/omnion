// The browser half of the timestamp defect, in a form a gate can assert on.
//
// Why this is a script and not a Rust test: the defect was a *serialisation* — the server
// answered a string a browser could not parse — and every unit test in the crate kept its
// timestamps as OffsetDateTime and never crossed the boundary. A Rust test can hold a
// timestamp forever without ever producing the eight bytes a `Date` object receives, so a
// whole class of wrong answers is unreachable from it. The one consumer that decides whether
// the answer is right is node's `Date`, so the gate asks node.
//
// The correction this file needed on its first run is the reason the driver exists. The first
// version asserted that the module *mentions* Rfc3339 and that a test asserting a `T` exists
// — and it went green with the formatter's body reduced to `match Err(()) { … }`, which
// returns a panic-free, completely wrong answer while every textual assertion still holds.
// **A gate that reads the source and a gate that runs the code look identical from the
// outside and are opposites:** the first proves the fix was written down, the second proves
// it works. So the driver below depends on the product crate by PATH and prints what the real
// function returns, and every behavioural assertion is made on that output.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";

const MODULE = "modules/crm-intake/src/timestamp.rs";
const ROUTE = "apps/api/src/routes/crm_intake.rs";
const DRIVER_DIR = "/tmp/crm-ts-driver";
const ROOT = process.cwd();

let failures = 0;
const ok = (name, cond, detail = "") => {
  if (cond) {
    console.log(`  ok   ${name}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
};

console.log("crm timestamp wire format (running the real formatter)");

// --- 1. the module declares the canonical spelling ------------------------------------------
const mod = readFileSync(MODULE, "utf8");
ok("module declares rfc3339", /pub fn rfc3339\(/.test(mod));
ok("module declares rfc3339_opt", /pub fn rfc3339_opt\(/.test(mod));
ok("Rfc3339 is the format", /Rfc3339/.test(mod));

// --- 2. the driver: a real crate that depends on the product crate --------------------------
// `path` is the same dependency edge the API has, so what is asserted on below is the
// shipped function's return value and not a copy of its rule written down here.
mkdirSync(`${DRIVER_DIR}/src`, { recursive: true });
writeFileSync(
  `${DRIVER_DIR}/Cargo.toml`,
  [
    "[package]",
    'name = "crm-ts-driver"',
    'version = "0.1.0"',
    'edition = "2021"',
    "",
    "[dependencies]",
    'time = { version = "0.3", features = ["formatting", "parsing", "macros"] }',
    `omnion-module-crm-intake = { path = "${ROOT}/modules/crm-intake" }`,
    "",
  ].join("\n"),
);
// Written only when absent, and only when it is the gate's own. A hand-edited driver is how
// the negative control is run, and a gate that rewrites its fixture on every execution cannot
// be contradicted by anything — including its author.
const DRIVER_MAIN = `${DRIVER_DIR}/src/main.rs`;
const DRIVER_MARKER = "crm timestamp wire format (running the real formatter)";
if (!existsSync(DRIVER_MAIN) || readFileSync(DRIVER_MAIN, "utf8").includes(DRIVER_MARKER)) {
  writeFileSync(
    DRIVER_MAIN,
  `use omnion_module_crm_intake::timestamp;
use time::macros::datetime;

fn main() {
    // Deliberately NOT a round hour: a formatter that truncated to the hour would pass a
    // midnight fixture.
    let at = datetime!(2026-10-01 15:32:24 UTC);
    let produced = timestamp::rfc3339(at);
    let display = at.to_string();

    // Both failure shapes are computed here rather than assumed, so the driver does not
    // become the fixture that hides the defect. Exit codes, because a wrong answer must not
    // print something that looks like a right one.
    if produced == display {
        eprintln!("the formatter is the Display impl");
        std::process::exit(3);
    }
    if produced.is_empty() {
        eprintln!("the formatter answered an empty string");
        std::process::exit(4);
    }
    print!("{produced}");
}
`,
  );
}

// A dedicated target dir: the worktree's own is a symlink to the running pass's build, and
// this gate must not disturb a sibling's binary.
const runDriver = () => {
  const out = execFileSync(
    "bash",
    ["-lc", `export PATH="$HOME/.cargo/bin:$PATH"; cd ${DRIVER_DIR} && CARGO_TARGET_DIR=/tmp/crm-ts-target cargo run --quiet 2>&1`],
    { encoding: "utf8", env: { ...process.env, PATH: `${process.env.HOME}/.cargo/bin:${process.env.PATH}` } },
  );
  return out.trim();
};

// A non-zero exit from the driver is a DETECTED DEFECT, not a build failure, and conflating
// the two is a lie in the opposite direction: the negative control printed "the formatter is
// the Display impl" and exited 3, and this reported "the driver did not build" — a sentence
// about the gate's own plumbing standing where the product's wrongness belongs. A gate that
// cannot be told apart from a broken toolchain is a gate whose failures get ignored. So the
// driver's own verdict is read out of stderr and turned into the assertion, and a genuine
// build failure is the only thing that reports "did not build".
let produced;
let driverVerdict = null;
try {
  produced = runDriver();
} catch (e) {
  const out = String(e.stdout || "") + String(e.stderr || "");
  if (/the formatter is the Display impl/.test(out)) {
    driverVerdict = "display";
  } else if (/the formatter answered an empty string/.test(out)) {
    driverVerdict = "empty";
  } else {
    console.log(`  FAIL the driver did not build — ${out.slice(0, 300)}`);
    process.exit(1);
  }
  produced = "";
}
console.log(`  (the shipped formatter produced: ${produced})`);

// --- 3. node, the engine the panel's `new Date()` runs on ------------------------------------
const nodeSaysInvalid = (value) => {
  const out = execFileSync(
    "node",
    [
      "-e",
      "const d=new Date(process.argv[1]);process.stdout.write(String(Number.isNaN(d.getTime())))",
      value,
    ],
    { encoding: "utf8" },
  ).trim();
  return out === "true";
};

// The defect, still reproducible. If the Display spelling ever started parsing, this gate
// would be measuring nothing and had better say so.
const BAD = "2026-10-01 15:32:24.365355685 +00:00:00";
ok("the defect is still reproducible", nodeSaysInvalid(BAD), `${BAD} parsed, so this gate proves nothing`);

ok("the shipped output is a parseable date", !nodeSaysInvalid(produced), `${produced} is Invalid Date`);
ok("the shipped output carries the T separator", produced.includes("T"), produced);
ok("the shipped output is not the Display spelling", !produced.startsWith("2026-10-01 15:32:24.365355685 "), produced);
ok("the formatter is not the Display impl", driverVerdict !== "display", "the driver says it is");
ok("the formatter is not empty", driverVerdict !== "empty", "the driver says it is");
// Both directions: a formatter that stopped returning a date would pass "not the Display
// spelling" and fail the parse assertion above, and neither assertion alone is the test.
ok("an empty string is not a date", nodeSaysInvalid(""), "sanity check on the probe itself");

// --- 4. no route may serialise an instant through Display again -----------------------------
const route = readFileSync(ROUTE, "utf8");
const COLUMNS = [
  "received_at", "created_at", "updated_at", "escalated_at", "converted_at",
  "first_response_at", "first_response_due_at", "last_received_at", "next_before",
];
const survivors = [];
route.split("\n").forEach((line, i) => {
  if (!line.includes(".to_string()")) return;
  for (const col of COLUMNS) {
    if (new RegExp(`\\b${col}\\.to_string\\(\\)`).test(line) && !line.includes("timestamp::")) {
      survivors.push(`${i + 1}: ${line.trim().slice(0, 90)}`);
    }
  }
});
ok("no instant crosses the wire through Display", survivors.length === 0, survivors.join(" | "));

// --- 5. and the rewrites are actually there -------------------------------------------------
const viaFormatter = (route.match(/timestamp::rfc3339/g) || []).length;
ok("the route formats its timestamps through the module", viaFormatter >= 10, `only ${viaFormatter} sites`);
ok("the import is present", /use omnion_module_crm_intake::timestamp;/.test(route));

console.log(failures === 0 ? "OK all assertions hold" : `FAILED ${failures} assertion(s)`);
process.exit(failures === 0 ? 0 : 1);
