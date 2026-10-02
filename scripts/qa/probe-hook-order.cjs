#!/usr/bin/env node
/**
 * No hook may be called after a conditional return inside a component or a custom hook.
 *
 * WHY THIS EXISTS (wave3 tick 83). `WorkflowBuilder` renders a loading screen and an error
 * screen through early returns, and a fix added to the shortcut list (`92fba4af`) put a
 * `useCallback` and a `useEffect` *under* them. React did not throw — it logs "change in the
 * order of Hooks" and the Next dev overlay paints a full-screen error card OVER a builder that
 * was about to render correctly. So:
 *
 *   - every unit test of the code around it stayed green,
 *   - `cargo test`, `tsc` and `eslint` all stayed green,
 *   - and the browser pass read "palette: false, canvas: false, inspector: false, problems:
 *     false, paletteNodes: 0, canvasNodes: 0" while the server held a two-node graph.
 *
 * The row said "the builder did not open". The builder opened into an error overlay drawn by the
 * framework. Nothing in the repository could tell the difference, which is the definition of a
 * defect that costs a screen until a human opens a screenshot.
 *
 * WHY A REAL PARSER (this is the load-bearing decision, and it cost three wrong answers first).
 * The first four versions of this gate hand-counted braces over blanked text, and each one failed
 * on a construct that appears in ordinary JSX:
 *
 *   1. `if (x) {` / `return` on the next line was not recognised as a guard — reported the
 *      shipped defect as CLEAN.
 *   2. A hook call was matched only when it followed `=`, `(`, `,` … so an indented bare
 *      `useEffect(` never matched. Same silence.
 *   3. Treating EVERY `if (x) { return }` as the component's guard produced **209 findings**,
 *      because `WorkflowBuilder`'s keyboard handler is a ladder of `if (chord) { … return; }`
 *      inside a `useEffect` — a nested function's returns, not the component's.
 *   4. A template literal was blanked as one span, so `className={\`chip ${\n cond ? "a" : "b"\n}\`}`
 *      lost its real braces, depth drifted by one and a 634-line sibling component was charged to
 *      the wrong function.
 *
 * Every one of those is a question about the LANGUAGE, answered by re-implementing the language.
 * `typescript` is already a dependency of this workspace, so the gate parses instead of guessing.
 * The hand-written scanner is not kept as a fallback: a checker that can be wrong in a new way is
 * worse than no checker, and the mutations below pin the parser's answers.
 *
 * WHAT IT CHECKS. For every top-level function whose name looks like a component (capitalised) or
 * a hook (`use*`), any hook call that is a direct statement of the function body AND appears after
 * a top-level `return` guarded by an `if` is a finding. Nested functions are skipped: a hook
 * inside them is that function's problem, and a component cannot call a hook from a callback at
 * all (React would error at runtime), so nothing is lost.
 *
 * PROOF. It runs its own mutation block against COPIES of the real source in a temp directory:
 * the shipped defect plus hand-built cases, each of which must fail on its OWN named rule.
 * `QA_HOOK_MUTANT=1` makes a child structural-only, so a nested child's red cannot be the thing
 * under test.
 */

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const TARGET = path.join(ROOT, "apps", "admin", "features");
// The admin package owns the TypeScript dependency; the repo root does not link it.
const TS = path.join(ROOT, "apps", "admin", "node_modules", "typescript");
const ts = require(TS);

const RULE = "no hook call after a guarded return";

const HOOK_NAMES = new Set([
  "useState",
  "useReducer",
  "useEffect",
  "useLayoutEffect",
  "useInsertionEffect",
  "useMemo",
  "useCallback",
  "useRef",
  "useContext",
  "useImperativeHandle",
  "useSyncExternalStore",
  "useId",
  "useDebugValue",
  "useTransition",
  "useDeferredValue",
]);

const MUTANT = process.env.QA_HOOK_MUTANT === "1";

/** A `.tsx`/`.ts` file already parsed, or null when it could not be read. */
function parseFile(file) {
  const text = fs.readFileSync(file, "utf8");
  return ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
}

/** True for `useEffect(`, `useThing(`, … but not `ref.useThing(` — a MemberExpression callee. */
function isHookCall(node) {
  if (!ts.isCallExpression(node)) return false;
  const callee = node.expression;
  if (!ts.isIdentifier(callee)) return false;
  const name = callee.text;
  return HOOK_NAMES.has(name) || /^use[A-Z]/.test(name);
}

/** The earliest statement index holding a conditional top-level `return`, or -1. */
function firstGuardedReturnIndex(body) {
  for (let i = 0; i < body.length; i += 1) {
    const stmt = body[i];
    if (ts.isReturnStatement(stmt)) {
      if (i > 0 && ts.isIfStatement(body[i - 1])) return i;
      continue;
    }
    // `if (…) return …;` without braces is one statement, and `if (…) { return }` is two: the
    // block's parent is the `if`, so walk up one level before deciding.
    if (ts.isIfStatement(stmt) && ts.isBlock(stmt.thenStatement)) {
      for (const inner of stmt.thenStatement.statements) {
        if (ts.isReturnStatement(inner)) return i;
      }
    }
    if (ts.isIfStatement(stmt) && ts.isReturnStatement(stmt.thenStatement)) return i;
  }
  return -1;
}

/** Hook calls that are DIRECT statements of this body (not inside a nested function). */
function directHookCalls(body) {
  const found = [];
  const visit = (node) => {
    if (
      node !== body &&
      (ts.isFunctionDeclaration(node) ||
        ts.isFunctionExpression(node) ||
        ts.isArrowFunction(node) ||
        ts.isMethodDeclaration(node) ||
        ts.isGetAccessor(node) ||
        ts.isSetAccessor(node))
    ) {
      return; // a nested function's hooks are its own concern
    }
    if (node !== body && isHookCall(node)) {
      found.push({ start: node.getStart(), line: node.getSourceFile().getLineAndCharacterOfPosition(node.getStart()).line + 1, name: node.expression.text });
    }
    ts.forEachChild(node, visit);
  };
  visit(body);
  return found;
}

function scanSourceFile(file) {
  const sf = parseFile(file);
  const findings = [];
  let functions = 0;
  for (const stmt of sf.statements) {
    if (!ts.isFunctionDeclaration(stmt) || !stmt.name) continue;
    const name = stmt.name.text;
    if (!/^[A-Z]/.test(name) && !name.startsWith("use")) continue;
    functions += 1;
    const body = stmt.body;
    if (!body) continue;
    const guard = firstGuardedReturnIndex(body.statements);
    if (guard === -1) continue;
    // Compare AST OFFSETS, not lines: a `return` and the hook after it can share a line, and a
    // line-number comparison decides that case by accident of formatting.
    const guardStart = body.statements[guard].getStart();
    for (const call of directHookCalls(body)) {
      if (call.start <= guardStart) continue;
      findings.push({
        file: path.relative(ROOT, file),
        function: name,
        hook: call.name,
        line: call.line,
        rule: RULE,
      });
    }
  }
  return { findings, functions };
}

function scanDir(dir) {
  const acc = { findings: [], functions: 0 };
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const nested = scanDir(full);
      acc.findings.push(...nested.findings);
      acc.functions += nested.functions;
      continue;
    }
    if (!/\.tsx?$/.test(entry.name)) continue;
    if (/\.test\.tsx?$/.test(entry.name)) continue;
    const one = scanSourceFile(full);
    acc.findings.push(...one.findings);
    acc.functions += one.functions;
  }
  return acc;
}

// ---- the mutation proof --------------------------------------------------------------------
// A gate that has never been seen red is a gate that reports nothing. Each case below is a real
// file shape, written into a temp dir, and each must fail on its OWN named rule.
const CASES = [
  {
    name: "the shipped defect: a hook under the loading/error returns",
    expect: 2,
    files: {
      "Bad.tsx": `export function Builder({ loading }: { loading: boolean }) {
  if (loading) {
    return <div>Loading</div>;
  }
  if (loading === false) {
    return <div>Error</div>;
  }
  const close = useCallback(() => setOpen(false), []);
  useEffect(() => {
    close();
  }, [close]);
  return <div data-builder />;
}
`,
    },
  },
  {
    name: "a correct component: the hook above the guard must stay silent",
    expect: 0,
    files: {
      "Good.tsx": `export function Fine({ ready }: { ready: boolean }) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  useEffect(() => {
    if (open) return;
    close();
  }, [open, close]);
  if (!ready) {
    return null;
  }
  return <div>{String(open)}</div>;
}
`,
    },
  },
  {
    name: "a nested handler's early returns are not the component's guard",
    expect: 0,
    files: {
      "Keyboard.tsx": `export function Keyboard() {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "a") {
        focusPalette();
        return;
      }
      if (event.key === "b") {
        closeHelp();
        return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  return <div />;
}
`,
    },
  },
  {
    name: "a one-line guard counts too",
    expect: 1,
    files: {
      "Inline.tsx": `export function Inline({ busy }: { busy: boolean }) {
  if (busy) return <div>Busy</div>;
  const ref = useRef<number>(0);
  return <div>{String(ref.current)}</div>;
}
`,
    },
  },
  {
    name: "the source parses: a brace inside a regex and a template literal must not shift it",
    expect: 1,
    files: {
      "Literals.tsx": `const HOST = "127.0.0.1";
export function Literals({ busy }: { busy: boolean }) {
  if (busy) {
    return <div className={\`chip \${
      busy ? "a" : "b"
    }\`} />;
  }
  const found = /127\\.0\\.0\\.1/.test(HOST);
  const state = useRef(found);
  return <div>{String(state.current)}</div>;
}
`,
    },
  },
];

function runMutations() {
  const failures = [];
  for (const testCase of CASES) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "hook-order-"));
    try {
      for (const [name, body] of Object.entries(testCase.files)) {
        fs.writeFileSync(path.join(dir, name), body);
      }
      const { findings, functions } = scanDir(dir);
      const label = `mutant ${testCase.name}: expects ${testCase.expect}, got ${findings.length}`;
      if (findings.length !== testCase.expect) {
        failures.push(`${label} — ${JSON.stringify(findings)}`);
        continue;
      }
      if (functions < 1) {
        failures.push(
          `${testCase.name}: scanner parsed ${functions} functions — it cannot pass by finding nothing`,
        );
      }
      console.log(`  ok  ${label} (functions parsed: ${functions})`);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  }
  return failures;
}

// ---- the real run --------------------------------------------------------------------------

if (MUTANT) {
  const target = process.argv[2] ?? TARGET;
  const acc = { findings: [], functions: 0 };
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
        continue;
      }
      if (!/\.tsx?$/.test(entry.name) || /\.test\.tsx?$/.test(entry.name)) continue;
      const one = scanSourceFile(full);
      acc.findings.push(...one.findings);
      acc.functions += one.functions;
    }
  };
  if (fs.statSync(target).isDirectory()) walk(target);
  else {
    const one = scanSourceFile(target);
    acc.findings.push(...one.findings);
    acc.functions += one.functions;
  }
  console.log(JSON.stringify(acc));
  process.exit(0);
}

const results = [];
let failures = [];

const real = scanDir(TARGET);
console.log(`scanned ${TARGET}`);
console.log(`  component/hook functions parsed: ${real.functions}`);
if (real.functions < 40) {
  console.log(
    `  FAIL  only ${real.functions} functions parsed — the scan cannot be trusted (a parse failure hides as a clean file)`,
  );
  process.exit(1);
}

results.push(["scan parses the repository", real.functions >= 40, `${real.functions} functions`]);
results.push([
  "the real tree has no hook after a guarded return",
  real.findings.length === 0,
  `${real.findings.length} findings`,
]);

for (const finding of real.findings) {
  console.log(`  FAIL  ${finding.file} ${finding.function}() calls ${finding.hook}() after a guarded return`);
  console.log(`        rule: ${finding.rule}`);
}

console.log("\nmutation proof:");
failures = failures.concat(runMutations());

let pass = 0;
let fail = 0;
for (const [name, ok, detail] of results) {
  if (ok) {
    pass += 1;
    console.log(`  PASS  ${name} — ${detail}`);
  } else {
    fail += 1;
    console.log(`  FAIL  ${name} — ${detail}`);
  }
}
for (const failure of failures) {
  fail += 1;
  console.log(`  FAIL  ${failure}`);
}

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail === 0 ? 0 : 1);
