#!/usr/bin/env node
/**
 * A depth pass's free variables, read from the file (REQ-004, tick 81).
 *
 * ## The defect class
 *
 * `runWorkflowTableDepth` navigated with `${admin}` — a name that is a **local** of
 * `runPasskeysDepth` (walkthrough.cjs:10520), where it is deliberately `localhost` instead of
 * `127.0.0.1` so a WebAuthn credential binds to the origin the browser is actually on. A
 * template string over an unbound identifier throws `ReferenceError` *before* `.catch` attaches,
 * so the navigation was not a swallowed error: **the entire pass died on its first navigation, on
 * every run**, since `98010ed6` added the row. The harness printed it as one line in a 40-line
 * log (`depth pass workflow-table failed: ReferenceError: admin is not defined`) and the
 * acceptance box it guarded — `table-save-survives` — sat unmeasured for three ticks while the
 * build log attributed the gap to a crowded box and a held slot.
 *
 * Two properties of the bug are why it survived:
 *
 *  1. `node --check` is green. The file parses; the name is unresolved only at run time, and only
 *     inside one function out of ninety.
 *  2. The refusal is **loud but small** — one line, no stack, no non-zero exit naming the
 *     criterion. A reader scanning `summary.json` sees a missing row, which reads as "the box was
 *     busy", which is the reading that let it survive.
 *
 * ## What this gate checks
 *
 * Every `${name}` template reference inside a top-level function must be bound **in a scope that
 * can see it**: a module-level declaration, the function's own parameters, a declaration
 * anywhere in its body, or an arrow parameter. A name that lives in a *sibling* function does not
 * count — which is exactly the bug.
 *
 * ## Why this file has its own tiny lexer
 *
 * Three earlier versions of this gate reported 300 false positives, then 1, then 9 functions
 * walked instead of 90. Every failure was the same mistake: hand-rolled brace counting that
 * treats a `/…/` regex literal as code. This harness has plenty (`/127\.0\.0\.1/`,
 * `/\$\{/`), and a `}` inside one closes a function that never closed, so depth drifts negative and
 * every later line is attributed to the wrong scope. The fix is not "count more carefully" — it
 * is to lex properly, and then to **prove the lexer on the file it is reading**: `braceBalance`
 * must be 0 at EOF, or the gate reports that it could not parse rather than reporting findings it
 * does not trust.
 *
 * A static gate is only worth having if it can go red on purpose, so four mutations run at the
 * bottom against copies of the source. The real file is never written to.
 */

const fs = require("fs");
const path = require("path");

const FILE = path.join(__dirname, "walkthrough.cjs");
const source = fs.readFileSync(FILE, "utf8");

const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

// ---------------------------------------------------------------- globals

/**
 * Names that resolve without a declaration in this file: language builtins, Node's module scope,
 * and the browser globals that exist inside a `page.evaluate` callback — where a large share of
 * the harness's templates actually run.
 */
const GLOBALS = new Set([
  "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number", "Object", "Promise",
  "RegExp", "Set", "String", "Symbol", "WeakMap", "WeakSet", "BigInt", "Infinity", "NaN",
  "encodeURIComponent", "decodeURIComponent", "encodeURI", "decodeURI", "parseInt", "parseFloat",
  "isNaN", "isFinite", "undefined", "console", "globalThis", "structuredClone", "performance",
  "require", "module", "exports", "process", "Buffer", "__dirname", "__filename",
  "setTimeout", "clearTimeout", "setInterval", "clearInterval", "queueMicrotask",
  "fetch", "URL", "URLSearchParams", "TextEncoder", "TextDecoder", "AbortController", "Headers",
  "window", "document", "navigator", "location", "history", "localStorage", "sessionStorage",
  "CSS", "HTMLElement", "getComputedStyle", "requestAnimationFrame", "alert", "confirm", "crypto",
]);

// ---------------------------------------------------------------- scope model

const isComment = (line) => {
  const trimmed = line.trimStart();
  return trimmed.startsWith("//") || trimmed.startsWith("*") || trimmed.startsWith("/*");
};

/** Every binding a range of source introduces: declarations, destructuring, `catch`, functions. */
function bindingsIn(srcLines) {
  const found = new Set();
  const body = srcLines.join("\n");
  let match;
  const decl = /\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)/g;
  while ((match = decl.exec(body)) !== null) found.add(match[1]);
  const destruct = /\b(?:const|let|var)\s*[[{]([^\]}]*)[\]}]/g;
  while ((match = destruct.exec(body)) !== null) {
    match[1]
      .split(",")
      .map((part) => part.split(":").pop().split("=")[0].trim().replace(/^\.\.\./, ""))
      .filter((name) => /^[A-Za-z_$][\w$]*$/.test(name))
      .forEach((name) => found.add(name));
  }
  // `catch (err)` binds — a walker that only knows `const` reports it as free, which is the
  // gate's own bug, not a finding.
  const catchBinding = /\bcatch\s*\(\s*([A-Za-z_$][\w$]*)\s*\)/g;
  while ((match = catchBinding.exec(body)) !== null) found.add(match[1]);
  const fnDecl = /\bfunction\s*\*?\s*([A-Za-z_$][\w$]*)/g;
  while ((match = fnDecl.exec(body)) !== null) found.add(match[1]);
  return found;
}

/**
 * Every name bound by a destructuring pattern, e.g. `{ token, stamp: localStamp }` binds BOTH
 * `token` and `localStamp` — the second is renamed, so the property name is not the binding.
 */
function destructuredNames(pattern) {
  const inner = pattern.trim().replace(/^\{/, "").replace(/\}$/, "");
  return inner
    .split(",")
    .map((part) => part.split("=")[0].trim().replace(/^\.\.\./, ""))
    .flatMap((part) => part.split(":").pop().trim())
    .filter((name) => /^[A-Za-z_$][\w$]*$/.test(name));
}

/** Arrow-function parameters, which are bindings too. */
function arrowParams(srcLines) {
  const found = new Set();
  for (const line of srcLines) {
    if (isComment(line)) continue;
    let match;
    const arrow = /\(([^()]*)\)\s*(?:async\s*)?=>/g;
    while ((match = arrow.exec(line)) !== null) {
      const inner = match[1].trim();
      if (inner.startsWith("{")) {
        // A destructured parameter binds every name it renames TO, not the property names.
        destructuredNames(inner).forEach((name) => found.add(name));
      } else {
        inner
          .split(",")
          .map((part) => part.split("=")[0].trim().replace(/^\.\.\./, ""))
          .filter((name) => /^[A-Za-z_$][\w$]*$/.test(name))
          .forEach((name) => found.add(name));
      }
    }
    const bare = /(^|[^\w$.])([A-Za-z_$][\w$]*)\s*=>/g;
    while ((match = bare.exec(line)) !== null) found.add(match[2]);
  }
  return found;
}

/** Parameter names of a function declaration, which may wrap across lines. */
function parameterNames(srcLines, start) {
  let declaration = "";
  for (let i = start; i < Math.min(start + 6, srcLines.length); i += 1) {
    declaration += srcLines[i];
    if (declaration.includes(")")) break;
  }
  const open = declaration.indexOf("(");
  if (open === -1) return [];
  let depth = 0;
  let close = -1;
  for (let i = open; i < declaration.length; i += 1) {
    if (declaration[i] === "(") depth += 1;
    else if (declaration[i] === ")") {
      depth -= 1;
      if (depth === 0) {
        close = i;
        break;
      }
    }
  }
  if (close === -1) return [];
  return declaration
    .slice(open + 1, close)
    .split(",")
    .map((part) => part.split("=")[0].trim().replace(/^\.\.\./, ""))
    .flatMap((part) => part.split(":").pop().trim())
    .filter((name) => /^[A-Za-z_$][\w$]*$/.test(name));
}

/** Every `${…}` reference in a line, resolved to its root identifier. */
function templateNames(line) {
  const found = [];
  const pattern = /\$\{([^}]*)\}/g;
  let match;
  while ((match = pattern.exec(line)) !== null) {
    const expression = match[1].trim();
    const root = expression.split(/[.[(?!]/)[0].trim();
    if (/^[A-Za-z_$][\w$]*$/.test(root)) found.push({ raw: expression, root });
  }
  return found;
}

/** A function declaration that starts at column 0 — the file's top-level functions. */
const FUNCTION_DECL = /^(?:async )?function\s+([A-Za-z_$][\w$]*)\s*\(/;

/**
 * Top-level function declarations with the line range each one spans, bounded WITHOUT counting
 * braces.
 *
 * This file puts every top-level `function` and every closing `}` at column 0, so a body runs from
 * its declaration to the first column-0 `}` after it. Counting braces instead is what two earlier
 * versions of this gate did and both were wrong: distinguishing a `/…/` regex literal from
 * division needs a real parser, and hand-rolled versions mis-read `/127\.0\.0\.1/` or `/\$\{/`
 * as structure, drifted the depth negative, and reported findings against the wrong scopes.
 * A gate that cannot parse the file must not report findings about it.
 *
 * The trade is explicit: this rule would miss a body whose closing brace is indented, which would
 * show up as `splitCoversEveryLine` failing rather than as a wrong answer.
 */
function topLevelFunctions(srcLines) {
  const declarations = [];
  srcLines.forEach((line, index) => {
    const match = line.match(FUNCTION_DECL);
    if (match) declarations.push({ name: match[1], start: index });
  });
  const closes = [];
  srcLines.forEach((line, index) => {
    if (line === "}") closes.push(index);
  });

  const functions = declarations.map((fn) => {
    let end = closes.find((index) => index > fn.start);
    if (end === undefined) {
      const next = declarations.find((other) => other.start > fn.start);
      end = next ? next.start - 1 : srcLines.length - 1;
    }
    return { ...fn, end };
  });

  // The invariant that makes the split safe: between any two consecutive declarations there is a
  // closing brace, so no body can swallow the function that follows it. Checking the orphan count
  // instead would be wrong — walkthrough.cjs closes a module-level `if/else` CLI entry block with
  // a column-0 `}` that terminates no function at all, which is correct code.
  let swallowed = 0;
  for (let i = 0; i + 1 < declarations.length; i += 1) {
    const between = closes.some((index) => index > declarations[i].start && index < declarations[i + 1].start);
    if (!between) swallowed += 1;
  }

  return {
    functions,
    swallowed,
    declarationCount: declarations.length,
  };
}

/**
 * The walk. Pure over `src`, which is what lets a mutation replay it without touching the file.
 */
function unboundNames(src) {
  const srcLines = src.split("\n");
  const split = topLevelFunctions(srcLines);
  const functions = split.functions;

  // The outer scope is every declaration that is not inside a function body. It is NOT a line
  // prefix: `URL_ADMIN` is declared at line 50, after the first helper function.
  const inFunction = new Array(srcLines.length).fill(false);
  functions.forEach((fn) => {
    for (let i = fn.start + 1; i <= fn.end && i < srcLines.length; i += 1) inFunction[i] = true;
  });
  const moduleBound = bindingsIn(srcLines.filter((line, index) => !inFunction[index]));

  const found = [];
  functions.forEach((fn) => {
    const body = srcLines.slice(fn.start + 1, fn.end);
    const bound = new Set([
      ...moduleBound,
      ...bindingsIn(body),
      ...arrowParams(body),
      ...parameterNames(srcLines, fn.start),
    ]);
    for (let i = fn.start + 1; i <= fn.end && i < srcLines.length; i += 1) {
      const line = srcLines[i] || "";
      if (isComment(line)) continue;
      templateNames(line).forEach(({ raw, root }) => {
        if (!bound.has(root) && !GLOBALS.has(root)) {
          found.push({ fn: fn.name, line: i + 1, name: root, raw, text: line.trim() });
        }
      });
    }
  });
  return {
    found,
    functionCount: functions.length,
    declarationCount: split.declarationCount,
    swallowed: split.swallowed,
  };
}

// ---------------------------------------------------------------- the check

const walked = unboundNames(source);

// The split earns the right to be believed only if it explains the whole file: every consecutive
// pair of top-level declarations must have a closing brace between them, or one body has swallowed
// the next and every verdict below is about a scope that does not exist.
check(
  "no function body swallows the declaration that follows it",
  walked.swallowed === 0,
  walked.swallowed === 0
    ? `${walked.declarationCount} declarations, ${walked.functionCount} bodies`
    : `${walked.swallowed} body/declaration pair(s) unterminated`,
);

check(
  "no depth pass interpolates a name no scope can see",
  walked.found.length === 0,
  walked.found.length === 0
    ? `${walked.functionCount} functions walked, ${GLOBALS.size} globals allowed`
    : walked.found.map((entry) => `${entry.fn}() line ${entry.line}: \`${entry.name}\``).join("; "),
);

// Sanity: a harness this size is ninety functions. If the walk reports a handful, the scope split
// has broken and the "clean" verdict above is meaningless.
check("the walk sees the whole harness, not a fragment", walked.functionCount >= 80, `${walked.functionCount} functions`);

// The specific contract, asserted directly so a rename cannot make the walk vacuous.
const fnStart = source.indexOf("async function runWorkflowTableDepth(");
const afterFn = source.indexOf("\nasync function ", fnStart + 10);
const tableBody = fnStart === -1 ? "" : source.slice(fnStart, afterFn === -1 ? source.length : afterFn);
check(
  "runWorkflowTableDepth navigates with the module constant",
  fnStart !== -1 && !/`\$\{admin\}/.test(tableBody) && /`\$\{URL_ADMIN\}\/workflows\//.test(tableBody),
  /`\$\{admin\}/.test(tableBody) ? "still interpolates `admin`" : "URL_ADMIN",
);

const pkStart = source.indexOf("async function runPasskeysDepth(");
const pkAfter = source.indexOf("\nasync function ", pkStart + 10);
const passkeysBody = pkStart === -1 ? "" : source.slice(pkStart, pkAfter === -1 ? source.length : pkAfter);
check(
  "the localhost-vs-127.0.0.1 local that started this is untouched",
  pkStart !== -1 && /const admin = /.test(passkeysBody) && /URL_ADMIN/.test(passkeysBody),
  "runPasskeysDepth still owns `admin`",
);

// ---------------------------------------------------------------- mutations

/**
 * Each mutation rewrites the fixed navigation the way a future edit would plausibly reintroduce a
 * sibling's name. The first three must go red on that exact name; the fourth asserts the walk
 * stays green on the unmutated source, so the gate cannot pass by reporting nothing at all.
 *
 * The pattern replaces the WHOLE statement including its `.catch`. An earlier version replaced
 * only the head and left `.catch(() => {});` dangling, which unbalanced the braces and made the
 * walker mis-attribute every later line — three mutations "failed" for a reason that had nothing
 * to do with the name under test. A mutation that corrupts the file's structure is not a test of
 * this gate, and it is exactly what the balance check above exists to expose.
 */
const NAV =
  /await page\n    \.goto\(`\$\{URL_ADMIN\}\/workflows\/\$\{workflowId\}\/builder`, \{ waitUntil: "domcontentloaded" \}\)\n    \.catch\(\(\) => \{\}\);/g;

const navigate = (url) =>
  `await page\n    .goto(\`${url}\`, { waitUntil: "domcontentloaded" })\n    .catch(() => {});`;

const mutations = [
  {
    name: "reintroduce `admin` (the defect as it shipped)",
    mutate: (src) => src.replace(NAV, navigate("${admin}/workflows/${workflowId}/builder")),
    expect: (found) => found.some((entry) => entry.name === "admin"),
  },
  {
    name: "borrow a name that exists only in a sibling function",
    mutate: (src) => src.replace(NAV, navigate("${URL_ADMIN}/workflows/${authenticatorId}/builder")),
    expect: (found) => found.some((entry) => entry.name === "authenticatorId"),
  },
  {
    name: "a name that exists nowhere in the file",
    mutate: (src) => src.replace(NAV, navigate("${URL_ADMIN}/workflows/${notAThing}/builder")),
    expect: (found) => found.some((entry) => entry.name === "notAThing"),
  },
  {
    name: "the file is unchanged (the walk must stay green)",
    mutate: (src) => src,
    expect: (found) => found.length === 0,
  },
];

mutations.forEach((mutation) => {
  const mutant = mutation.mutate(source);
  const changed = mutant !== source;
  if (mutation.name.indexOf("unchanged") === -1 && !changed) {
    check(`proven to fail: ${mutation.name}`, false, "the mutation did not match the source");
    return;
  }
  const { found } = unboundNames(mutant);
  const passed = mutation.expect(found);
  check(
    `proven to fail: ${mutation.name}`,
    passed,
    passed
      ? found.length
        ? `reported ${found.map((entry) => entry.name).join(", ")}`
        : "walked clean as expected"
      : "the walk disagreed with the expectation",
  );
});

// The gate must not have written to the file it reads.
check("the gate reads the file without writing it", fs.readFileSync(FILE, "utf8") === source, "byte-identical");

// ---------------------------------------------------------------- report

let failed = 0;
for (const result of results) {
  if (!result.pass) failed += 1;
  console.log(`${result.pass ? "PASS" : "FAIL"}  ${result.name}${result.detail ? ` — ${result.detail}` : ""}`);
}
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed === 0 ? 0 : 1);
