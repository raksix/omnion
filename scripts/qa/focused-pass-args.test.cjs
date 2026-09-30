#!/usr/bin/env node
// The focused-pass filter must understand both argument spellings.
//
// `run.sh` builds `--only="$QA_ONLY"`; `arg()` used to match only the two-word form, so
// `indexOf("--only")` was -1, the filter fell back to "all" and every focused pass a
// writer ran walked the whole route list. This checks the parser directly, without a
// browser and without a server, because the failure it guards is silent by construction:
// a filter that matches everything yields a complete, entirely green report.
const fs = require("fs");
const path = require("path");

const src = fs.readFileSync(
  path.join(__dirname, "walkthrough.cjs"),
  "utf8",
);
const start = src.indexOf("function arg(");
const end = src.indexOf("const URL_ADMIN");
if (start < 0 || end < 0) throw new Error("arg() not found — the parser was moved or renamed");
const body = src.slice(start, end);

// Evaluate the parser with an argv we control, rather than this process's own.
// The returned wrapper closes over `argv`, so each case re-evaluates with its own
// vector — calling one shared instance with a second argument would silently keep
// reading the first vector, and every case would report the same answer.
const parserFor = (argv) => {
  const fn = new Function(
    "argv",
    `${body.replace(/process\.argv/g, "argv")}\nreturn function (name, fallback) { return arg(name, fallback); };`,
  );
  return fn(argv);
};

const cases = [
  // [argv, expected, why]
  [["--only=a,b"], "a,b", "the equals spelling run.sh actually emits"],
  [["--only", "a,b"], "a,b", "the two-word spelling"],
  [["--only=a,b", "--url", "http://x"], "a,b", "equals first, url after"],
  [["--url", "http://x", "--only=a,b"], "a,b", "equals last"],
  [["--url", "http://x"], "all", "no filter at all is a full pass"],
  [["--only"], "all", "a flag with no value is not a filter"],
  [["--only", "--url", "http://x"], "all", "the next token being a flag means no value"],
  // A pass carrying only `--url` and no filter is a FULL pass, and the `only` lookup
  // must not wander into the neighbouring token to invent one.
  [["--url", "http://x:3105"], "all", "another arg's value is not a filter"],
];

let failed = 0;
for (const [argv, expected, why] of cases) {
  const got = parserFor(argv)("only", "all");
  const ok = got === expected;
  if (!ok) failed += 1;
  console.log(
    `${ok ? "ok  " : "FAIL"} --only ${JSON.stringify(argv)} -> ${JSON.stringify(got)} (${why})`,
  );
}

const urlOk =
  parserFor(["--url=http://y:1"])("url", "d") === "http://y:1" &&
  parserFor(["--url", "http://z:2"])("url", "d") === "http://z:2";
if (!urlOk) {
  console.log("FAIL url() regressed on one of its two spellings");
  failed += 1;
} else {
  console.log("ok   url() parses both spellings");
}

console.log(failed === 0 ? "\nfocused-filter parser: all cases pass" : `\n${failed} case(s) failed`);
process.exit(failed === 0 ? 0 : 1);
