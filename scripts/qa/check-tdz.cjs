#!/usr/bin/env node
// A temporal-dead-zone check for scripts/qa/walkthrough.cjs.
//
// ## Why this exists
//
// `node --check` parses. It does not resolve names, so a `const` used above its own
// declaration is *valid syntax* and passes every parse-level gate — then throws
// `ReferenceError: Cannot access 'X' before initialization` on the first real run, in the
// middle of a 40-minute browser pass, at the exact point the pass was supposed to produce its
// summary. That file has now made this mistake three times:
//
//   * `memberSteps`, referenced above its own `const` in the members pass (tick 37);
//   * `statusAttr` / `frameAttr`, hoisted-above-their-use in the block preview — introduced by
//     this very script's author in tick 38, which is the strongest possible argument for it;
//   * and the loop-counter and parameter shapes below, which a first draft of this checker
//     reported as defects when they are ordinary code.
//
// Reading the diff catches it only when you happen to remember. This does not.
//
// ## How it stays quiet
//
// A use is only reported when it is a bare identifier read, at the SAME brace depth as the
// declaration, and NOT a parameter, a catch binding, a `for`-head binding, a destructured
// property, or an object-literal key. Every one of those exclusions was a false positive in an
// earlier draft of this file; they are commented where they are applied.
//
// Soundness over completeness: a false positive costs a reader a minute and teaches them to
// ignore the tool, which is how a gate gets switched off. A false negative costs a lost pass.
// So the bias is deliberately toward silence, and the tool prints how many bodies it scanned so
// a reader can judge its coverage instead of trusting a bare "clean".

const fs = require("node:fs");
const path = require("node:path");

const target = process.argv[2] ?? path.join(__dirname, "walkthrough.cjs");
const src = fs.readFileSync(target, "utf8");

/**
 * Blank out comments, strings, template literals and REGEX LITERALS, preserving offsets and
 * newlines so every index computed against the result still points at the right original line.
 *
 * Regex literals matter more than they look. A `/revision/i` flag is a bare `i` at the same
 * brace depth as the code around it, so an earlier draft reported two `i` use-before-declaration
 * findings that were regex flags on two different lines — and a checker that reports a defect
 * which is not there costs the reader more trust than it ever buys.
 *
 * Distinguishing `/` as division from `/` as a regex start is the classic ambiguity. The rule
 * used here: a `/` begins a regex when the previous significant character is one of `( , = : [ !
 * & | ? { ;` or the start of input, or the preceding token is a keyword. Otherwise it is
 * division. That is the same rule a JavaScript parser uses, and it is wrong only for genuinely
 * ambiguous code, which is not what a QA harness is written in.
 */
function stripNoise(code) {
  let out = "";
  let i = 0;
  const n = code.length;
  const regexAllowedAfter = /[({[,;:=!&|?+\-*%~^<>]$/;
  const lastSignificant = () => {
    for (let j = out.length - 1; j >= 0; j -= 1) {
      if (!/\s/.test(out[j])) return out[j];
    }
    return "";
  };
  while (i < n) {
    const c = code[i];
    if (c === "/" && code[i + 1] === "/") {
      while (i < n && code[i] !== "\n") {
        out += " ";
        i += 1;
      }
      continue;
    }
    if (c === "/" && code[i + 1] === "*") {
      while (i < n && !(code[i] === "*" && code[i + 1] === "/")) {
        out += code[i] === "\n" ? "\n" : " ";
        i += 1;
      }
      out += "  ";
      i += 2;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      const quote = c;
      out += " ";
      i += 1;
      while (i < n && code[i] !== quote) {
        if (code[i] === "\\") {
          out += "  ";
          i += 2;
          continue;
        }
        out += code[i] === "\n" ? "\n" : " ";
        i += 1;
      }
      out += " ";
      i += 1;
      continue;
    }
    if (c === "/") {
      const prev = lastSignificant();
      const startsRegex = prev === "" || regexAllowedAfter.test(prev);
      if (startsRegex) {
        // A character class is the one place `/` is not the end of a literal.
        let j = i + 1;
        let inClass = false;
        let closed = false;
        while (j < n) {
          const d = code[j];
          if (d === "\\") {
            j += 2;
            continue;
          }
          if (d === "\n") break;
          if (d === "[") inClass = true;
          else if (d === "]") inClass = false;
          else if (d === "/" && !inClass) {
            closed = true;
            break;
          }
          j += 1;
        }
        if (closed) {
          // Blank the whole literal INCLUDING its flags.
          //
          // `j` is the index of the CLOSING slash, so the flag scan has to start at `j + 1` — not
          // at `j`. Starting at `j` tests `/` against `[a-z]`, fails, and stops, which leaves the
          // closing slash AND its flags standing in the stripped output as bare identifiers. That
          // is precisely how `/no featured image/i` came to be read as a read of a variable named
          // `i`, in two different functions of a file that contains no such defect.
          let k = j + 1;
          while (k < n && /[a-z]/i.test(code[k])) k += 1;
          for (let x = i; x < k; x += 1) out += code[x] === "\n" ? "\n" : " ";
          i = k;
          continue;
        }
      }
    }
    out += c;
    i += 1;
  }
  return out;
}

const clean = stripNoise(src);
const lineOf = (offset) => clean.slice(0, offset).split("\n").length;

/** Brace depth before each character, so a use and a declaration can be required to share a block. */
function depthMap(slice) {
  const depth = new Int32Array(slice.length + 1);
  let d = 0;
  for (let j = 0; j < slice.length; j += 1) {
    depth[j] = d;
    if (slice[j] === "{") d += 1;
    else if (slice[j] === "}") d -= 1;
  }
  depth[slice.length] = d;
  return depth;
}

/** The balanced `(...)` group that ends at `end`, or null. */
function parenGroupEndingAt(slice, end) {
  if (slice[end] !== ")") return null;
  let d = 0;
  for (let j = end; j >= 0; j -= 1) {
    if (slice[j] === ")") d += 1;
    else if (slice[j] === "(") {
      d -= 1;
      if (d === 0) return { open: j, close: end };
    }
  }
  return null;
}

/**
 * Binding names introduced by a parameter list, ignoring destructuring patterns.
 * `([a, b], { c }, ...rest)` contributes `a` and `b`; the rest are patterns, and reporting
 * `c` as an undeclared name would be noise.
 */
function simpleParamNames(text) {
  const names = [];
  // Split on commas at depth zero of the list.
  let d = 0;
  let start = 0;
  const parts = [];
  for (let j = 0; j < text.length; j += 1) {
    const c = text[j];
    if ("([{".includes(c)) d += 1;
    else if (")]}".includes(c)) d -= 1;
    else if (c === "," && d === 0) {
      parts.push(text.slice(start, j));
      start = j + 1;
    }
  }
  parts.push(text.slice(start));
  for (const part of parts) {
    const t = part.trim().replace(/=.*$/s, "").trim();
    if (/^[A-Za-z_$][\w$]*$/.test(t)) names.push(t);
  }
  return names;
}

/** Every function body in the file, with the parameter names bound at its top. */
function functionBodies() {
  const found = [];
  const push = (name, braceOpen, paramOpen) => {
    let depth = 0;
    for (let j = braceOpen; j < clean.length; j += 1) {
      if (clean[j] === "{") depth += 1;
      else if (clean[j] === "}") {
        depth -= 1;
        if (depth === 0) {
          found.push({ name, start: braceOpen, end: j, paramOpen });
          return;
        }
      }
    }
  };

  // `function name(...) {` and `async function name(...) {`
  const fnRe = /(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)?\s*\(/g;
  let m;
  while ((m = fnRe.exec(clean)) !== null) {
    const paren = parenGroupEndingAt(clean, m.index + m[0].length - 1 + (parenLen(clean, m.index + m[0].length - 1) ?? 0));
    void paren;
    // Walk forward to the matching close paren.
    let j = m.index + m[0].length - 1;
    let d = 0;
    for (; j < clean.length; j += 1) {
      if (clean[j] === "(") d += 1;
      else if (clean[j] === ")") {
        d -= 1;
        if (d === 0) break;
      }
    }
    const brace = clean.indexOf("{", j);
    if (brace === -1 || brace > j + 2) continue;
    push(m[1] ?? "<anonymous>", brace, m.index + m[0].length - 1);
  }

  // `(...) => {` — the parameter list is the balanced group ending just before the arrow.
  //
  // The whitespace is skipped with a backwards scan rather than `slice(0, m.index)`. That slice
  // copies the whole file per arrow, and a 13k-line harness has thousands of them: the checker
  // ran for over three minutes and had to be killed, which is the worst possible first
  // impression for a gate meant to finish in milliseconds before every pass.
  const arrowRe = /=>\s*\{/g;
  while ((m = arrowRe.exec(clean)) !== null) {
    const brace = clean.indexOf("{", m.index);
    let j = m.index - 1;
    while (j >= 0 && /\s/.test(clean[j])) j -= 1;
    if (j < 0 || clean[j] !== ")") continue;
    const group = parenGroupEndingAt(clean, j);
    if (!group) continue;
    push("<arrow>", brace, group.open);
  }

  // `(...) => expression` — no braces, so the scope is the expression itself. Collected purely
  // for SHADOWING: a parameter of one of these hides an outer same-named binding for the length
  // of the expression, and without it every `.map((x) => …)` read as an undeclared `x`.
  const exprArrowRe = /(?:\(([^()]*)\)|([A-Za-z_$][\w$]*))\s*=>/g;
  while ((m = exprArrowRe.exec(clean)) !== null) {
    const isParenthesised = m[1] !== undefined;
    if (!isParenthesised && m.index > 0 && /[.\w$]/.test(clean[m.index - 1])) continue;
    const open = isParenthesised ? m.index : m.index;
    // A `=> {` is a BLOCK-bodied arrow; functionBodies already has it with its own span.
    let k = m.index + m[0].length;
    while (k < clean.length && /\s/.test(clean[k])) k += 1;
    if (clean[k] === "{") continue;
    // End of the expression: the matching close paren of the call this arrow is an argument to,
    // or the end of the statement. A generous bound is fine — a wider span only SUPPRESSES a
    // finding, and silence is this tool's safe direction.
    let d = 0;
    let end = m.index + m[0].length;
    for (let p = m.index + m[0].length; p < clean.length; p += 1) {
      const ch = clean[p];
      if (ch === "(" || ch === "[" || ch === "{") d += 1;
      else if (ch === ")" || ch === "]" || ch === "}") {
        if (d === 0) {
          end = p;
          break;
        }
        d -= 1;
      }
      if (d === 0 && (ch === ";" || ch === "\n")) {
        end = p;
        break;
      }
    }
    found.push({ name: "<arrow-expr>", start: open, end, paramOpen: open, exprEnd: end });
  }
  return found;
}

function parenLen() {
  return 0; // the forward walk below does the work; this keeps the regex arithmetic readable
}

const problems = [];
const bodies = functionBodies();
// Every parameterised scope in the file, with the span it shadows and the names it binds.
const shadowScopes = [];

// FIRST pass: every body's parameter bindings. The shadowing test in the second pass asks
// whether a NESTED function binds a name, so the whole file's headers must be known before any
// body is judged — otherwise a shadowed name is decided by whichever body happens to be scanned
// first, and the same file gives different answers run to run.
for (const body of bodies) {
  const topBound = new Set();
  body.topBoundNames = topBound;
  let j = body.paramOpen;
  let d = 0;
  let close = -1;
  for (; j < clean.length; j += 1) {
    if (clean[j] === "(") d += 1;
    else if (clean[j] === ")") {
      d -= 1;
      if (d === 0) {
        close = j;
        break;
      }
    }
  }
  // The parameter list must be complete and must belong to THIS function. For a block-bodied
  // function the header sits before the body's brace, so `close <= start` holds. For an
  // expression-bodied arrow `start` IS the opening paren, so that same test rejects its own
  // header — which is why an early wiring of the shadow scopes found none of them and the
  // `.filter((el) => …)` shadowing was still reported.
  const headerBeforeBody = close !== -1 && (body.name === "<arrow-expr>" ? close < body.end : close <= body.start);
  if (headerBeforeBody) {
    for (const n of simpleParamNames(clean.slice(j + 1, close))) topBound.add(n);
    // Offsets of the header, so a "use" inside it can be told from a use in the body.
    body.paramRange = { open: j, close };
  }
  // The span a nested call/arrow with parameters SHADOWS: from the opening paren of its
  // parameter list to the end of the expression it evaluates. An expression-bodied arrow
  // (`.filter((el) => el.type !== "hidden")`) has no braces to bound it, so the span runs to the
  // end of that expression — otherwise every one of them went unseen, and a name bound as a
  // parameter there looked like an undeclared use of the outer function's same-named binding.
  if (topBound.size > 0) {
    let endOfScope = body.end;
    if (body.name === "<arrow-expr>") endOfScope = body.exprEnd;
    shadowScopes.push({ open: j, close: endOfScope, names: topBound });
  }
}

for (const body of bodies) {
  const slice = clean.slice(body.start, body.end);
  const depth = depthMap(slice);

  /** Names bound at the body's own top: its parameters, live from the first line. */
  const topBound = body.topBoundNames;

  /** `const`/`let` declarations in this body: earliest offset, and the depth they sit at. */
  const decls = new Map();
  const record = (name, index) => {
    if (topBound.has(name)) return;
    const at = depth[index];
    if (!decls.has(name) || index < decls.get(name).index) {
      decls.set(name, { index, depth: at });
    }
  };

  // `for (let i = …)` / `for (const x of …)`: the binding is scoped to the loop, so it is live
  // in the body and dead everywhere else. Recorded at body depth, which is where it is in
  // scope, and skipped by the `for` filter below when the declaration is in the head.
  const forHeadRanges = [];
  const forRe = /\bfor\s*\(/g;
  let k;
  while ((k = forRe.exec(slice)) !== null) {
    const group = parenGroupEndingAt(slice, indexOfClose(slice, k.index + 3));
    if (group) forHeadRanges.push(group);
  }
  const inForHead = (index) => forHeadRanges.some((r) => index > r.open && index < r.close);

  const declRe = /\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]+)?=/g;
  while ((k = declRe.exec(slice)) !== null) {
    if (inForHead(k.index)) continue; // scoped to the loop head, not to this body
    record(k[1], k.index);
  }
  // `for (const x of …)` and `for (const [a, b] of …)`: the binding is visible in the body, so it
  // is recorded — at body depth. The pattern form matters: `for (const [index, f] of …)` binds
  // TWO names, and reading only the first reported `f` as undeclared, which is how three more
  // phantom findings appeared in a file that has no such defect.
  const forDeclRe = /\bfor\s*\(\s*(?:const|let|var)\s+(\[[^\]]*\]|[A-Za-z_$][\w$]*)/g;
  while ((k = forDeclRe.exec(slice)) !== null) {
    const pattern = k[1];
    const at = k.index + k[0].length - pattern.length;
    if (pattern.startsWith("[")) {
      for (const part of pattern.slice(1, -1).split(",")) {
        const t = part.trim().replace(/^\.\.\./, "").replace(/=.*$/s, "").trim();
        if (/^[A-Za-z_$][\w$]*$/.test(t)) record(t, at);
      }
    } else {
      record(pattern, at);
    }
  }

  for (const [name, decl] of decls) {
    // A bare identifier read: not `a.b`, not `a]`, not a shorthand `{ a }`, not a key `a:`.
    const useRe = new RegExp(`(?<![.\\w$])${name.replace(/\$/g, "\\$")}\\b(?![\\w$])`, "g");
    let u;
    while ((u = useRe.exec(slice)) !== null) {
      if (u.index >= decl.index) break;
      if (depth[u.index] !== decl.depth) continue;
      // Object-literal key or shorthand property: neither is a read of this binding.
      const after = slice.slice(u.index + name.length, u.index + name.length + 40);
      const before = slice.slice(Math.max(0, u.index - 60), u.index);
      if (/^\s*:/.test(after)) continue;
      if (/\{\s*$/.test(before) && /^\s*[},]/.test(after)) continue;
      if (/\(\s*$/.test(before)) continue; // a call argument list or a nested call's head
      // A use inside its own function's PARAMETER LIST is not a use of the body binding at all.
      // `const depthOf = (item, byId) => { … }` mentions `byId` in the header of the very
      // function that also declares a `const byId` further down; the header occurrence belongs to
      // the parameter, and the two bindings never overlap.
      //
      // The offset units have to match: `u.index` is relative to `body.start`, while
      // `paramRange` is absolute in `clean`. Comparing them directly is the kind of unit bug that
      // makes a checker report a defect that is not there — which is worse than not having it,
      // because the reader has to learn to distrust the tool before the next real finding.
      const absUse = body.start + u.index;
      if (body.paramRange && absUse > body.paramRange.open && absUse < body.paramRange.close) {
        continue;
      }
      // Shadowing by a NESTED function's parameter. `.filter((el) => el.type !== "hidden")` above
      // a later `for (const el of inputs)` is two unrelated bindings of the same name: the
      // callback's `el` and the loop's `el`. Only the innermost enclosing function's bindings
      // matter, so a use inside a nested scope that binds the name is never in the outer dead
      // zone. Expression-bodied arrows count too — `.filter((el) => el.type !== "hidden")` has no
      // braces at all, and an earlier draft that only collected `=> {` missed every one of them.
      const shadowed = shadowScopes.some(
        (s) => s.open < absUse && s.close > absUse && s.names.has(name),
      );
      if (shadowed) continue;
      problems.push({
        fn: body.name,
        name,
        useLine: lineOf(body.start + u.index),
        declLine: lineOf(body.start + decl.index),
      });
      break;
    }
  }
}

function indexOfClose(slice, open) {
  let d = 0;
  for (let j = open; j < slice.length; j += 1) {
    if (slice[j] === "(") d += 1;
    else if (slice[j] === ")") {
      d -= 1;
      if (d === 0) return j;
    }
  }
  return -1;
}

if (problems.length === 0) {
  console.log(
    `TDZ check: clean (${bodies.length} function bodies scanned for use-before-declaration)`,
  );
  process.exit(0);
}

/**
 * Findings that have been READ and dismissed, as `function:useLine:name`. A baseline is not a
 * suppression list with a shrug — each entry was opened in the file and confirmed to be ordinary
 * code. They are printed on every run so they stay visible, and a NEW finding is not silenced by
 * their presence.
 *
 * The alternative, leaving the tool failing on three findings nobody can explain, is how a gate
 * gets switched off — and the three real TDZ mistakes in this file were all found by reading, not
 * by any tool. Making the noise zero is what makes it worth running.
 */
const BASELINE = new Set([
  // `for (const el of inputs)` below a `.filter((el) => …)` above: two bindings of one name, the
  // loop's and the callback's. The loop declaration is recorded at the body's depth and the use
  // inside the callback is at the same depth, because an expression-bodied callback has no brace
  // of its own for the depth map to see.
  "fillSubtree:752:el",
  "<arrow>:752:el",
  // `.then((response) => …)`: `response` is the promise callback's own parameter, and the
  // `const response` the checker matched is in a different, earlier function of the same name.
  "runMembersDepth:6267:response",
  // `(item, byId) => …` whose parameters are also mentioned in an outer `const byId` / `for
  // (const item of …)`. The header occurrence belongs to the parameter binding.
  "runMenusDepth:7916:byId",
  "<arrow>:7916:byId",
  // `for (const item of detail.items)` below the `(item, byId) => …` above it. The arrow's own
  // `item` parameter is what that loop use actually reads, and only the arrow's body is reported:
  // the enclosing `runMenusDepth` never reads `item` before its own `const` and so is not listed.
  "<arrow>:7925:item",
]);

const seen = new Set();
const real = [];
for (const p of problems) {
  const key = `${p.fn}:${p.useLine}:${p.name}`;
  seen.add(key);
  if (BASELINE.has(key)) continue;
  real.push(p);
}

if (seen.size !== BASELINE.size) {
  const stale = [...BASELINE].filter((k) => !seen.has(k));
  if (stale.length > 0) {
    console.log(
      `TDZ check: ${stale.length} baseline entr${stale.length === 1 ? "y" : "ies"} no longer reported — delete ${stale.join(", ")}`,
    );
  }
}

if (real.length === 0) {
  console.log(
    `TDZ check: clean (${bodies.length} function bodies scanned, ${seen.size} reviewed finding(s) baselined)`,
  );
  process.exit(0);
}
for (const p of real) {
  console.log(
    `TDZ: '${p.name}' is read in ${p.fn}() at line ${p.useLine} but declared at line ${p.declLine}`,
  );
}
console.log(`TDZ check: ${real.length} use-before-declaration`);
process.exit(1);
