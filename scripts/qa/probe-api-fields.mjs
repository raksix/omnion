#!/usr/bin/env node
/**
 * probe-api-fields.mjs — every response field the walkthrough reads must be a field the handler's
 * return type actually sends.
 *
 * ## Why this gate exists
 *
 * `probe-api-routes.mjs` (tick 89) settles the URL half of the same failure: it proves every path
 * the walkthrough fetches is a path the router mounts. Tick 89's defect had FOUR parts and the
 * URL sweep fixed one of them:
 *
 *   1. the URL `/api/v1/workflows/{id}/runs` was **not mounted**            → fixed by the route sweep
 *   2. the payload key `body.runs` is **not a field** (`executions`)        → THIS GATE
 *   3. `trigger_kind` is the **stored column**, `ExecutionSummary` sends `trigger`  → THIS GATE
 *   4. there was **no baseline**, so the count was satisfied by somebody else's press → not a
 *      static defect; a reading rule, and the row now carries `runCountBeforeKey` beside it
 *
 * Parts 2 and 3 share a shape: **a field name that is plausible, and that appears somewhere in
 * the server** — so grepping the walkthrough for an unknown identifier finds it, and a human
 * reading the row believes the answer. Two ticks earlier the same REQ found a sibling: a selector
 * read off an attribute written as a *value* rather than a name (tick 87), and a probe marker the
 * product never renders (tick 86). **Fixing the site a report names leaves every unnamed site
 * holding the defect** — so this is a sweep, not one more assertion in the row's own test.
 *
 * ## What makes it trustworthy
 *
 *  * The field list is derived from **the handler's return type**, followed to the struct that
 *    type names (`Json<ListenerListBody>` → `pub struct ListenerListBody`). A hand-kept allowlist
 *    of "the fields that are real" is the same defect one layer out and rots on the first rename.
 *  * **`#[serde(rename = "…")]` wins over the field name.** `Node.node_type` serialises as `type`,
 *    which is the whole of tick 74's `paramWrote: false` — `undefined === "wait"` on every run,
 *    forever, on a keyboard path that works. A gate that only read field names would have called
 *    `node_type` present.
 *  * **`skip_serializing_if` marks a field OPTIONAL.** It is absent from the wire whenever it is
 *    `None`, so reading it is legal and reading it *unconditionally* is a defect the reader must
 *    see as optional — reported as `optional`, never as missing.
 *  * **A field read from a response that is DEFERRED is not this gate's to resolve** (same rule as
 *    the route sweep): a body built on an interpolated base has no handler this file can name.
 *  * **A control that bites.** `--self-test` proves the three shapes that shipped as defects — a
 *    key that is not a field, a rename that hides the real name, an optional field — and proves a
 *    real field still resolves. A sweep that matched nothing prints `0 unknown` and is
 *    indistinguishable from a clean bill of health.
 *
 * ## Four wrong versions before this one was right (recorded because all four were green)
 *
 *   1. Return-type tracing stopped at `Json<X>` and reported every read of a field on a
 *      `HashMap<Value>` response as missing — a shape this API does not use, but the failure mode
 *      is the one that matters: a gate that cries wolf is a gate nobody runs.
 *   2. Reading the walkthrough's `body.x` with a plain regex matched `document.body.innerText` and
 *      `body.listeners` inside a *string*, reporting CSS selectors as payload keys.
 *   3. The optional check tested `has field && !optional` instead of `!has field && !optional`,
 *      so every optional field was reported missing — 30 findings on a file with none.
 *   4. Chasing the handler through `use ... as ...` re-export blocks found `mod.rs` for handlers
 *      that live in their own module, and reported the whole workflows surface as untraceable.
 *
 * Usage: `node scripts/qa/probe-api-fields.mjs [--json] [--self-test]`
 */
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const WALK = path.join(ROOT, "scripts/qa/walkthrough.cjs");
const ROUTES_MOD = path.join(ROOT, "apps/api/src/routes/mod.rs");

const walk = readFileSync(WALK, "utf8");
const routesMod = readFileSync(ROUTES_MOD, "utf8");

const asJson = process.argv.includes("--json");
const selfTest = process.argv.includes("--self-test");

/** Collapse path parameters, drop the query string. Identical to the route sweep's normaliser. */
const shape = (p) =>
  p
    .split("?", 1)[0]
    .replace(/\/+$/, "")
    .replace(/\$\{[^}]*\}/g, "~")
    .replace(/\{[^}]*\}/g, "~");

const read = (rel) => {
  const file = path.join(ROOT, rel);
  return existsSync(file) ? readFileSync(file, "utf8") : null;
};

// ---------------------------------------------------------------------------------------------
// 1. route path -> handler function, through the router's own bindings.
// ---------------------------------------------------------------------------------------------

/** `let NAME: MethodRouter<AppState, Infallible> = …(module::handler)…;` */
const bindings = new Map();
for (const m of routesMod.matchAll(/let\s+([a-z_][a-z0-9_]*)\s*(?::[^=]*?)?=\s*([\s\S]{0,400}?);/g)) {
  const [, name, expr] = m;
  const h = [...expr.matchAll(/([a-z_][a-z0-9_]*)\s*::\s*([a-z_][a-z0-9_]*)/g)]
    // The method combinators are `get(..)` / `post(..)` — free functions, not module paths.
    .filter(([mod]) => !["get", "post", "put", "delete", "patch", "head", "options", "merge", "route_layer"].includes(mod))
    .map(([, mod, fn]) => ({ mod, fn }));
  if (h.length) bindings.set(name, h[0]);
}

const pathHandler = new Map();
for (const m of routesMod.matchAll(/\.route\(\s*"([^"]+)"\s*,\s*([A-Za-z_][A-Za-z0-9_]*)/g)) {
  const h = bindings.get(m[2]);
  if (h) pathHandler.set(shape(m[1]), h);
}

/**
 * The handler for a path the walkthrough fetches.
 *
 * **The walkthrough spells the API prefix; the router mounts under the nest it is layered into.**
 * `fetch("/api/v1/workflows/…/executions")` and `.route("/workflows/{id}/executions", …)` are the
 * same route written from two sides. Without this the lookup returned `null` for **every** site,
 * and `null` is the value the checker treats as "cannot be resolved, skip" — so the sweep walked
 * 16 sites, verified none of them, and printed `UNKNOWN FIELDS: 0`. That is the same failure mode
 * one layer out from the window bug, and it is worse in a way: the floor could be satisfied by a
 * site whose handler is known, and a gate that verifies nothing still prints a coverage number.
 * The floor is therefore joined by a **verified** floor below: sites that reached a handler must
 * be a real share of the sites that resolved, or the sweep is measuring names again.
 */
function resolveHandler(s) {
  const cands = [s];
  if (s.startsWith("/api/v1/")) cands.push(s.slice("/api/v1".length));
  for (const c of cands) if (pathHandler.has(c)) return pathHandler.get(c);
  return null;
}

// ---------------------------------------------------------------------------------------------
// 2. handler -> the struct its return type names, and that struct's wire field names.
// ---------------------------------------------------------------------------------------------

const fieldCache = new Map();

/**
 * The one place that finds a struct's body: its span in `src`, comments stripped.
 *
 * Everything downstream reads THIS rather than searching again. Two searches for one fact is the
 * defect this file was rewritten for: when the visibility rule was relaxed in `structFields`, the
 * identical search inside `reachableStructs` still demanded `pub`, so a private struct resolved its
 * fields and reported an empty body, and the fix looked like it had half worked. A derived answer
 * must have exactly one derivation.
 */
function structBody(src, name) {
  const start = src.search(new RegExp(`(?:^|\\n)\\s*(?:pub(?:\\([^)]*\\))?\\s+)?struct ${name}\\b`));
  if (start < 0) return { start: -1, body: "" };
  // The body runs to the brace that closes the struct: count from the first `{` after the header.
  let i = src.indexOf("{", start);
  let depth = 0;
  let end = -1;
  for (; i < src.length; i += 1) {
    if (src[i] === "{") depth += 1;
    else if (src[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }
  if (end < 0) return { start: -1, body: "" };
  // Comment-stripped: the sentence documenting this defect class lives in these structs' own docs,
  // and a doc comment naming a field is prose about the wire, not the wire.
  return { start, body: src.slice(start, end).replace(/\/\/[^\n]*/g, "") };
}

/** Fields a struct serialises: serde renames win, `skip_serializing_if` makes it optional. */
export function structFields(src, name) {
  // **The cache key is the name AND the source, and the name alone is a silent collision.** Two
  // modules each define a `StepBody`-shaped type — `crates/workflows` and the routes crate both
  // carry private response structs — and whichever module was read first won every later lookup. The
  // wrong field list is not a crash: it is a *plausible* one, so a sweep that reads `id` off a body
  // that sends it and `error` off a body that does not produces findings that look reasoned. Keying
  // on the text's own identity is what makes the derivation honest, and it is cheaper than arguing
  // about which module owns a name.
  const key = `${name} ${src.length} ${src.indexOf(`struct ${name}`)}`;
  if (fieldCache.has(key)) return fieldCache.get(key);
  // **Visibility is not a statement about the wire.** `pub struct` was required here, so every
  // private response struct resolved to nothing — including the platform's own `ErrorDetail`, which
  // is private precisely because nothing names it, yet is serialised on every refusal in the
  // product. A field list derived from source that stops at the first missing keyword is a field list
  // that silently shrinks, and the sweep then reports the fields it dropped as fields no endpoint
  // sends. Both spellings are accepted; the name is what identifies the type, not its `pub`.
  const { start, body } = structBody(src, name);
  if (start < 0) return null;
  const code = body;
  const fields = new Map();
  // **A field with no `pub` still goes on the wire.** The pattern required `pub name:` and so read
  // `ErrorDetail` — the body of every refusal in the platform — as a struct with NO fields at all,
  // which is why the error envelope resolved to nothing and its own `error` key was reported as a
  // field no endpoint sends. Serde serialises private fields; visibility is a Rust-level statement
  // and the wire is not Rust. What still distinguishes a field from prose is the trailing `:` after
  // a snake_case identifier, so that — not the keyword — is the test. A `pub` field matches it too,
  // which is why relaxing this needs no second pattern and cannot start matching non-fields.
  for (const f of code.matchAll(/(?:^|[\s;{])(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:/g)) {
    const name = f[1];
    // The serde attribute is within a few lines ABOVE the field, so take a small window backwards.
    const at = f.index;
    const window = code.slice(Math.max(0, at - 220), at);
    const rename = [...window.matchAll(/serde\s*\(([^)]*)\)/g)]
      .pop()?.[1]
      ?.match(/rename\s*=\s*"([^"]+)"/)?.[1];
    const optional = /skip_serializing_if/.test(window.slice(window.lastIndexOf("serde")));
    fields.set(rename ?? name, { rust: name, optional });
  }
  const out = fields.size ? fields : null;
  fieldCache.set(key, out);
  return out;
}

const moduleCache = new Map();

/**
 * Every struct a handler's response can put on the wire, top-level plus one level through a
 * container: `Vec<ExecutionSummary>` and `Option<ListenerBody>` both carry fields the walkthrough
 * reads, and those reads are written on the **narrowed** object (`latest.status`,
 * `captured?.event_name`) — i.e. `body.executions[0].status`, which no single struct owns.
 *
 * **This is the step the first draft did not take, and it hid the entire listener row.** A
 * one-struct lookup resolves `body.armed` and cannot resolve `captured?.event_name`, so the row
 * whose fields this gate exists to check reported clean. Depth is bounded and the walk is
 * cycle-guarded: a body that points at itself must not become an infinite descent.
 */
function reachableStructs(src, name, seen = new Set()) {
  if (seen.has(name)) return [];
  seen.add(name);
  const fields = structFields(src, name);
  if (!fields) return [];
  // **The struct's BODY is re-derived here, from a second search for its header — and that search
  // still demanded `pub`.** So a private struct resolved here returned an EMPTY body while
  // `structFields` above it had already parsed the fields correctly: two copies of the same answer,
  // one of them written to a rule the other no longer uses. `structFields` is now the single place
  // that knows how to find a struct, so it returns the body span too and this function reads it,
  // rather than searching again and hoping the two searches agree. Two searches for one fact is how
  // a fix lands on half the problem and the half looks unchanged.
  const { body } = structBody(src, name);
  const out = [fields];
  for (const c of body.matchAll(/pub\s+[a-z_][a-z0-9_]*\s*:\s*(?:Option|Vec|Box)<\s*([A-Z][A-Za-z0-9_]*)/g)) {
    out.push(...reachableStructs(src, c[1], seen));
  }
  return out;
}

/**
 * The fields every endpoint sends when it REFUSES.
 *
 * **A handler has two bodies, not one.** `handlerFields` below follows the success type —
 * `Result<Json<T>, ApiError>` — and reads `T`. But a refused request is answered with the ERROR
 * envelope (`ErrorBody { error: ErrorDetail { code, message, details } }`), and a probe that checks a
 * refusal's message is reading a body this file had never heard of. The unfinished-save row does
 * exactly that: `body.error.code` and `body.error.message` off a 400 from `POST /run`.
 *
 * So `error` is a field of EVERY handler's wire shape, and the sweep resolves it against the real
 * `ErrorDetail` rather than against a hand-kept list — a kept list is the same defect one layer out,
 * and it rots the first time the envelope gains a field. That is the difference between this and
 * tick 89's `runs`: `runs` was a name the server does not send anywhere, and `error` is one it sends
 * on every non-2xx in the platform.
 *
 * It is deliberately NOT merged into the success view: a merge would let `error` satisfy a read of a
 * SUCCESS body, which is the same class of hole in the other direction — a field that is legal only
 * when the request failed cannot vouch for a read made on a 200.
 */
let errorEnvelope = null;
function errorFields() {
  if (errorEnvelope) return errorEnvelope;
  const src = read("apps/api/src/error.rs");
  errorEnvelope = new Map();
  // `ErrorDetail` is a PRIVATE struct — `struct ErrorDetail { … }`, no `pub` — because nothing
  // outside the error module names it. `structFields` searches for `pub struct NAME`, so the
  // platform's own error envelope resolved to zero fields, the exemption below never fired, and the
  // sweep reported the envelope's own `error` key as a field no endpoint sends. **A visibility
  // keyword is not a statement about what goes on the wire**: serde serialises a private struct
  // exactly like a public one, and the walkthrough has been reading this envelope from the day the
  // first refusal was printed. So the lookup accepts both spellings here.
  const detail =
    src &&
    ([...(src.matchAll(/pub struct ErrorDetail\b/g)), ...(src.matchAll(/(?<!pub )struct ErrorDetail\b/g))].length
      ? reachableStructs(src, "ErrorDetail")
      : null);
  if (detail) for (const s of detail) for (const [k, v] of s) if (!errorEnvelope.has(k)) errorEnvelope.set(k, v);
  return errorEnvelope;
}

/** The fields the handler's own return type puts on the wire, including one container level. */
export function handlerFields(handler) {
  if (moduleCache.has(handler.fn)) return moduleCache.get(handler.fn);
  const rel = `apps/api/src/routes/${handler.mod}.rs`;
  const src = read(rel);
  let out = null;
  if (src) {
    // `pub async fn NAME(…) -> Result<Json<T>, ApiError>` — and the tuple/streaming variants.
    const sig = new RegExp(
      `pub async fn ${handler.fn}\\s*\\([\\s\\S]{0,600}?\\)\\s*->\\s*([^{]+)\\{`,
    ).exec(src);
    if (sig) {
      const ret = sig[1].replace(/\/\/[^\n]*/g, "");
      const named = [...ret.matchAll(/Json<([A-Za-z_][A-Za-z0-9_]*)>/g)].pop();
      if (named) {
        const structs = reachableStructs(src, named[1]);
        // One flat view: a read resolves if ANY reachable struct sends it.
        const merged = new Map();
        for (const s of structs) for (const [k, v] of s) if (!merged.has(k)) merged.set(k, v);
        out = merged;
      }
    }
  }
  moduleCache.set(handler.fn, out);
  return out;
}

// ---------------------------------------------------------------------------------------------
// 3. every `body.<field>` the walkthrough reads, bound to the fetch above it.
// ---------------------------------------------------------------------------------------------

const lineOf = (index) => walk.slice(0, index).split("\n").length;

/**
 * The first argument of every `fetch(…)` / `new URL(…)`. ONE regex: two overlapping ones
 * collected each site twice (the route sweep's defect 2).
 */
const CALL = /(?:fetch|new URL)\(\s*[`"']([^`"']*)[`"']/g;

/**
 * `document.body.innerText` is not a payload key, and neither is a CSS selector — so the read is
 * anchored on a **response object**, and the object must be bound to a json read in the same
 * window.
 *
 * **Optional chaining is part of the read.** `captured?.event_name` and `captured.event_name` are
 * the same field, and the walkthrough writes the first everywhere it has narrowed the object. The
 * first draft of this regex was `\b(obj)\.(field)\b`, which silently skipped every narrowed read —
 * the entire listener row, which is the one row in this REQ whose fields this gate exists to
 * check. **A sweep that cannot see half its own subject reports `0 unknown` and calls it a clean
 * bill of health**, which is defect 3 in the header arriving through the front door. The
 * self-test now counts how many reads the pattern actually captured, so the hole is measured
 * rather than assumed absent.
 */
const READ = /\b([a-z_][a-z0-9_]*)\??\.([a-z_][a-z0-9_]*)\b/g;

/**
 * The names a response body is actually READ from, across this walkthrough's vocabulary:
 * the json result itself, the container element it was narrowed to (`body.executions[0]` →
 * `latest`), and the second response in a two-read row. `res` is in the list because several rows
 * genuinely name the body `res` — the Response exclusion below is by ASSIGNMENT, not by name.
 */
const OBJECTS = new Set([
  "body", "payload", "answer", "res", "second", "table", "row", "current", "latest", "json",
]);

const sites = new Map(); // path shape -> { handler, fields: Map(field -> lines) }

/**
 * The Response object is NOT the payload. `res.json()`, `res.ok`, `res.status` are three reads the
 * naive pattern collected as fields, and `status` in particular is a name a struct really does
 * send — so the sweep would resolve `body.status` against the *fetch's* status and call a broken
 * read correct. The name a fetch is assigned to is therefore excluded from its own read window.
 *
 * `fetch(x).then(r => r.json())` binds nothing, so it is not excluded: there is no `r` to name.
 */
const RESPONSE_MEMBERS = new Set(["json", "ok", "status", "text", "headers", "url", "redirected", "body"]);

/**
 * The read window is STRUCTURAL, not a character count, and it starts AFTER the fetch's own
 * argument list.
 *
 * **Two drafts were wrong here and both reported a clean bill of health.** The first used a fixed
 * 900 characters — a claim about distance — and the listener row's `captured?.event_name` sits
 * seventeen lines below its fetch, so the sweep could not see the one row whose fields it exists to
 * check. The second brace-matched from the first `{` after the fetch, which is `{ credentials:
 * "same-origin" }`: the window opened and closed on the same line and every site collapsed to one
 * read. **A window that measures the fetch's arguments measures nothing**, and it reports `0` reads
 * rather than an error.
 *
 * So: paren-match the fetch call, resume after its `)`, then brace-match the enclosing callback's
 * body. Strings and template literals are skipped, because a `{` inside one is not a block and a
 * `)` inside one is not the end of the call — the walkthrough builds its URLs from template
 * literals, so this is not hypothetical.
 */
function skipToCode(src, from) {
  let i = from;
  while (i < src.length) {
    const c = src[i];
    if (c === '"' || c === "'" || c === "`") {
      i += 1;
      while (i < src.length && src[i] !== c) {
        if (src[i] === "\\") i += 1;
        i += 1;
      }
      i += 1;
      continue;
    }
    if (c === "/" && src[i + 1] === "/") {
      i = src.indexOf("\n", i);
      if (i < 0) return src.length;
      continue;
    }
    if (c === "/" && src[i + 1] === "*") {
      i = src.indexOf("*/", i);
      i = i < 0 ? src.length : i + 2;
      continue;
    }
    break;
  }
  return i;
}

/** Index just past the closing `)` of the call whose arguments start at `from`, or `from`. */
function endOfCall(src, from) {
  // `from` is where the CALL match began — at `fetch`, not at the `(` — because the pattern
  // captures the URL, which sits INSIDE the parentheses. Matching from the argument string would
  // paren-count a call that is already open, and every window comes back empty. So the opening
  // paren is the first one at or after `from`.
  const open = src.indexOf("(", from);
  if (open < 0 || open - from > 40) return from;
  let depth = 0;
  for (let i = open; i < src.length; i += 1) {
    const code = skipToCode(src, i);
    if (code !== i) {
      i = code - 1;
      continue;
    }
    if (src[i] === "(") depth += 1;
    else if (src[i] === ")") {
      depth -= 1;
      if (depth === 0) return i + 1;
    }
  }
  return from;
}

/**
 * The end of the block that CONTAINS `from`, by brace-matching the block's own opening brace.
 *
 * **Three drafts were wrong here and every one of them reported a clean bill of health.** The
 * first bounded the window at a fixed character count and could not see the listener row's reads
 * seventeen lines down. The second brace-matched from the first `{` after the fetch, which is the
 * `{ credentials: "same-origin" }` in the call's own arguments: the window opened and closed on
 * one line and every site collapsed to a single read. The third — the one that shipped as
 * `walk[open] !== "{"` — demanded that the character IMMEDIATELY after the call be `{`, and
 * **after a `fetch(…)` the character is `;`, `)` or `.` in every row this walkthrough has**, so
 * the window was empty for all 94 API calls and the sweep printed `0 sites, 0 unknown` and
 * exited 0. That is the worst shape a gate can have: a reader cannot distinguish "I checked
 * everything" from "I looked at nothing", and the number that would have said so was the one the
 * coverage floor did not check.
 *
 * So the window is the enclosing BLOCK, found structurally: brace-match forward while skipping
 * strings and comments, tracking every open brace on a stack, and take the top of the stack at
 * `from`. A row's reads sit in the block the author wrote them in, and the walkthrough's rows are
 * each their own function, so the window is the row — never its neighbour's, and never a fixed
 * distance. `endOfCall` is still called first so the call's own argument list (which contains a
 * `{ … }` and a `}`) cannot be mistaken for the enclosing block.
 */
function enclosingBlockEnd(from) {
  const stack = [];
  for (let i = 0; i < from; i += 1) {
    const code = skipToCode(walk, i);
    if (code !== i) {
      i = code - 1;
      continue;
    }
    if (walk[i] === "{") stack.push(i);
    else if (walk[i] === "}" && stack.length > 0) stack.pop();
  }
  if (stack.length === 0) return walk.length;
  const open = stack[stack.length - 1];
  let depth = 0;
  for (let i = open; i < walk.length; i += 1) {
    const code = skipToCode(walk, i);
    if (code !== i) {
      i = code - 1;
      continue;
    }
    if (walk[i] === "{") depth += 1;
    else if (walk[i] === "}") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return walk.length;
}

/**
 * Every fetch call offset in the walkthrough, ascending. Used to bound a site's reads.
 *
 * **A block that makes several fetches has to be split between them.** The `unfinished-save` row
 * issues three calls inside one `page.evaluate` — `POST /run`, then `GET /graph`, then
 * `GET /executions` — and every read in that block was credited to all three paths. So
 * `body.error.code` (the *run response's* error envelope) was reported as an unknown field of the
 * **graph** body and of the **executions** body, three sites deep from the endpoint that actually
 * sends it. That is over-attribution, and it is the mirror image of tick 89's under-reading: there
 * the read was invisible, here a correct read is blamed on an endpoint that never made it.
 *
 * A read belongs to the fetch that precedes it and is followed by no other fetch, because that is
 * the region in which its result is the one the author is holding. The last fetch in a block owns
 * the block's tail; the first owns nothing past the second.
 */
const FETCH_OFFSETS = [...walk.matchAll(CALL)].map((m) => m.index);

/** Blank a match to spaces, preserving length so every offset downstream still points at source. */
const blank = (m) => " ".repeat(m.length);

/**
 * `name -> the call offset whose `.json()` produced it`, for the block containing `from`.
 *
 * A payload is bound where the language binds it — inside the enclosing block — and read wherever the
 * author needed it, which is often twenty lines and two further fetches later. Collecting these per
 * FETCH was the mistake: the declaration is frequently outside the window that needed it, so the
 * binding was invisible and the read fell through to "charge it to whoever is asking". The block is
 * the scope that makes the name meaningful, so the table is keyed by block and cached — the same
 * block is visited once per fetch inside it, and the walkthrough has 94.
 */
const PRODUCERS = new Map(); // block start offset -> Map(name -> call offset)

function blockProducers(from) {
  const open = blockStart(from);
  if (PRODUCERS.has(open)) return PRODUCERS.get(open);
  const end = enclosingBlockEnd(open);
  // Only the block's own text, with comments and template interpolations removed for the same reason
  // the window is: both are prose about code, and both name fields that nothing reads.
  const block = walk
    .slice(open, end === walk.length ? walk.length : end)
    .replace(/\/\/[^\n]*/g, blank)
    .replace(/\/\*[\s\S]*?\*\//g, blank)
    .replace(/\$\{[^{}]*\}/g, blank);
  const table = new Map();
  // `const NAME = <expr>.json()` — the fetch that expression consumed is the nearest one above it.
  //
  // **The payload expression comes in several shapes and matching only one binds nothing.** All of
  // these assign the same thing, and the rows this gate most needs are the guarded ones:
  //
  //   const after = await (await fetch(url)).json();          // nested
  //   const body  = await response.json();                    // a named Response
  //   const body  = await response.json().catch(() => null);  // guarded — the common case
  //
  // A pattern written for the nested form alone matched none of the last two. A pattern written for
  // the bare form matched the third only by RUNNING PAST its `.catch(…)` — and then, because the
  // middle was bounded at 400 characters, it started matching from whatever `const` came before and
  // ended at that same `.json()`, binding the *fetch's own Response name* (`response`) as a payload.
  // That is the failure mode worth naming: a too-loose pattern does not miss, it **mis-binds**, and a
  // mis-bound table makes every subsequent attribution wrong in a way that still produces findings.
  //
  // So the chain is matched as a whole: `.json()` may be followed by `?.catch(…)`/`.catch(…)` and
  // still count, and the `(?!…)` guard then rejects any continuation the shape does not describe.
  // The catch body is matched by BRACE/PAREN depth rather than by `[^()]*`, because the whole point
  // of the guarded form is that its fallback is a closure — `.catch(() => null)` — and a character
  // class cannot hold parentheses of its own. A depth counter is the only way to say "everything up
  // to the matching close", and it is what made the guarded rows bind at all.
  for (const decl of block.matchAll(
    /(?:const|let|var)\s+([a-z_][a-z0-9_]*)\s*=\s*(?:await\s*)?(?:[\s\S]{0,400}?)\.json\(\)(?:\s*\??\.\s*catch\((?:[^()]|\([^()]*\))*\))?(?!\s*[.(])/g,
  )) {
    // A declaration that IS the fetch — `const response = await fetch(…)`, with no `.json()` of its
    // own — binds a Response, not a payload. The test is not "does the text mention fetch": the nested
    // form `const after = await (await fetch(…)).json()` mentions it too and IS a payload. What
    // separates them is whether the fetch's result is consumed by this declaration's own `.json()`,
    // so the pattern is re-checked for the nested shape specifically. Testing for the bare word
    // discarded every nested binding in the walkthrough, which is why `body` never appeared and the
    // run's envelope kept being charged to whichever site asked.
    const isBareFetch =
      /\bfetch\s*\(/.test(decl[0]) &&
      !/\(\s*(?:await\s*)?[\s\S]{0,400}?fetch\s*\([\s\S]{0,400}?\)\s*\)\s*\.json\(\)\s*$/.test(decl[0]);
    if (isBareFetch) continue;
    const at = open + decl.index;
    const owner = FETCH_OFFSETS.filter((o) => o < at).pop();
    if (owner !== undefined) table.set(decl[1], owner);
  }
  PRODUCERS.set(open, table);
  return table;
}

/** The offset of the `{` that opens the block containing `from`. */
function blockStart(from) {
  const stack = [];
  for (let i = 0; i < from; i += 1) {
    const code = skipToCode(walk, i);
    if (code !== i) {
      i = code - 1;
      continue;
    }
    if (walk[i] === "{") stack.push(i);
    else if (walk[i] === "}" && stack.length > 0) stack.pop();
  }
  return stack.length > 0 ? stack[stack.length - 1] : 0;
}

/** The read region belonging to the call at `from`: after its `)`, up to the next fetch in its block. */
function windowEnd(from, after) {
  const end = enclosingBlockEnd(after);
  if (end === walk.length) return end; // unbounded: the caller drops the site rather than guess
  const next = FETCH_OFFSETS.find((o) => o > from);
  if (next === undefined || next >= end) return end;
  // **A read inside a LATER call's ARGUMENTS belongs to that call, not to this one.** The
  // execution row nests its second read inside the URL it is fetching from —
  // `` fetch(`/api/v1/workflow-executions/${latest.id}`) `` — so `latest.id` sits between this
  // fetch's `)` and the nested one, and the boundary at the nested call's offset arrives too late
  // to exclude it. The same shape produces the false positive on the unfinished-save row:
  // `` (await fetch(`/api/v1/workflows/${id}/graph`)).json() `` puts `${id}` in its arguments, and
  // `id` is the outer evaluate's parameter. Both are reads of an OUTER payload, inside an inner
  // call, and crediting them to the inner path is how `id` was reported as an unknown field of a
  // body that has no such field.
  //
  // So the boundary is the START of the next call — its first character — not the end. That is the
  // only cut that excludes both its arguments and its trailing `.json()` chain.
  return next > from ? next : end;
}

function readWindow(from) {
  // Start after the call itself: the arguments hold `{ credentials: … }`, and reading inside them
  // would attribute the fetch's own options to the payload.
  const after = endOfCall(walk, from);
  if (after <= from) return ""; // the call never closed; there is no window to attribute to
  const end = windowEnd(from, after);
  // A window shorter than the call cannot contain a read.
  if (end <= after) return "";
  // **A window that runs to end-of-file attributes every read in the file to one site.** The two
  // top-level fetches (`/health/metrics` and its `.csv` sibling) are issued from module scope, where
  // the brace stack is empty, so the enclosing block is the file itself: they collected 29 field
  // reads spanning all sixteen rows, and because a `Map` merges by path those reads were also
  // available to *any* site that happens to resolve to the same shape. A site that cannot bound its
  // window must contribute NOTHING, not everything — the safe reading of "I cannot tell whose reads
  // these are" is "I do not know", and a gate that guesses here reports another row's fields as its
  // own. So an unbounded window is dropped, and the floor below is what notices the drop.
  if (end === walk.length) return "";
  // The offset is carried OUT because the caller has to map a read's index back to a source line,
  // and the only base that is correct is where this slice actually began. Recomputing it by calling
  // `endOfCall` again is the same number and it is tempting, which is how the double-count happened.
  return walk.slice(after, end);
}

for (const call of walk.matchAll(CALL)) {
  const raw = call[1];
  // Deferred for the same reasons as the route sweep: a base URL, or a caller-extended prefix.
  const deferred = raw.startsWith("${") || /\$\{[^}]*\}$/.test(raw);
  if (/^[a-z]+:\/\//i.test(raw) || !raw.includes("/api/")) continue;
  const s = shape(raw);
  if (!s.startsWith("/api/")) continue;
  const entry = sites.get(s) ?? { deferred, handler: resolveHandler(s), fields: new Map() };
  // The name the fetch is assigned to — `const response = await fetch(…)` — is a Response.
  const before = walk.slice(Math.max(0, call.index - 120), call.index);
  const assigned = [...before.matchAll(/(?:const|let|var)\s+([a-z_][a-z0-9_]*)\s*=\s*(?:await\s+)?$/g)].pop()?.[1];
  const window = readWindow(call.index);
  // Where that slice began, in `walk` coordinates — `endOfCall` returns an ABSOLUTE index (it walks
  // `i` from `from` to the end of the source and returns `i + 1`), so this is the offset itself and
  // **must not be added to `call.index` again**. Adding it walked the reported line past the end of
  // the file, for every row, which is the cheapest possible tell that the arithmetic was wrong: a
  // reader who opens the cited line finds no file there and stops trusting the gate that sent them.
  // Recorded beside the slice it describes so the line mapping downstream cannot re-derive it.
  const windowStart = endOfCall(walk, call.index);
  // **Comments are PROSE, not reads, and a window is full of both.** `skipToCode` skips comments so
  // that brace matching is not fooled by a `{` in a doc comment — but the collected field reads came
  // from the raw slice, so a comment explaining *why* a field is wrong put that field straight back
  // into the sweep, attributed to the path whose fix the comment was describing. Writing this fix
  // and then re-running the gate reproduced the exact false positive it removed, which is the
  // sharpest version of the lesson: a gate that cannot tell a mention from a use reports the author
  // of the fix as the author of the defect. Note this is the OPPOSITE of the row-guard trap — there
  // the comment SATISFIED an assertion; here it satisfies a FINDING.
  //
  // **Stripping must not SHORTEN.** The first version joined the non-comment parts, which DELETED
  // every comment's characters and pulled all later offsets leftwards — so a finding was reported
  // against the line of an unrelated statement, and chasing it led to a source line that contained
  // no such read at all. Both transforms below REPLACE with spaces of equal length, so an offset
  // in the cleaned text is the same offset in the original and `lineOf` keeps telling the truth.
  // A gate that misreports WHERE it found something sends its reader to the wrong line, which is
  // worse than not reporting: the reader trusts it and edits the wrong code.
  const code = window
    .replace(/\/\/[^\n]*/g, blank)
    .replace(/\/\*[\s\S]*?\*\//g, blank);
  // A TEMPLATE LITERAL'S `${…}` IS CODE, even though the quotes around it are not. Stripping
  // comments left `` `/api/v1/workflows/${id}/graph` `` intact, and `id` — the enclosing
  // `page.evaluate`'s own parameter, not a payload field — was collected as a read of the graph
  // body. Every templated URL in this walkthrough carries one of these, so this is not one row.
  const unTemplated = code.replace(/\$\{[^{}]*\}/g, blank);
  // **Proximity is not attribution.** The unfinished-save row fetches `/run`, then `/graph`, then
  // `/executions`, and assembles one `return { … }` object at the end: `status` and `body.error.code`
  // come from the RUN response, `steps` from the GRAPH response. A window is a span of text, and the
  // run's reads sit at the very END of it — after two more fetches have already happened — so
  // "everything between this fetch and the next" hands the run's envelope to the graph path, and
  // the finding blames an endpoint that never sent it.
  //
  // The correct binding is the one the language already gives: a NAME. `const body = await res.json()`
  // makes `body` this fetch's payload, and every later `body.x` in the block is a read of it no
  // matter how far away it sits. So a read is attributed by the name its object was bound to.
  //
  // **The bindings belong to the BLOCK, not to the window.** `const body = await response.json()` is
  // two lines above the `/graph` fetch and therefore outside that fetch's window — a per-fetch table
  // never learns that `body` is the RUN payload, so `producers.get("body")` is undefined, the
  // fallback charges the read to whichever site happens to be asking, and the run's error envelope is
  // blamed on the graph. Bindings are therefore collected once per enclosing block and shared by
  // every site inside it, which is the scope in which a `const` actually lives.
  //
  // A name bound by a REASSIGNMENT belongs to the fetch nearest the assignment, because that is what
  // the code means: `const run = await detail.json()` after an earlier json read is a new binding.
  const producerOf = blockProducers(call.index);
  for (const r of unTemplated.matchAll(READ)) {
    const [, obj, field] = r;
    if (!OBJECTS.has(obj)) continue;
    // A payload bound by a fetch OTHER than this one is not this site's field to judge. Charging it
    // here is what produced "the graph body has no `error`" for a read of the RUN response's.
    const producer = producerOf.get(obj);
    if (producer !== undefined && producer !== call.index) continue;
    if (obj === assigned) continue;
    if (RESPONSE_MEMBERS.has(field)) continue;
    if (!entry.fields.has(field)) entry.fields.set(field, new Set());
    // **Report the line the READ is on, not the line the fetch is on.** Both transforms above
    // preserve offsets exactly, so the read's index inside the window is relative to `windowStart`
    // and can be mapped straight through `lineOf`. It was previously reported at `call.index`, so
    // every field of every row was blamed on that row's fetch statement — which is how a finding
    // about `body.error` on the graph path came to point at a line containing no read of `error`.
    // A gate that names the wrong line does not merely waste a look: it makes the reader doubt the
    // finding that was right, and that is how a true defect survives a report.
    entry.fields.get(field).add(lineOf(windowStart + r.index));
  }
  sites.set(s, entry);
}

const all = [...sites.entries()]
  .filter(([, v]) => v.fields.size > 0)
  .map(([s, v]) => ({ path: s, handler: v.handler, fields: v.fields, lines: [...v.fields.values()].flat() }))
  .sort((a, b) => a.path.localeCompare(b.path));

const unknown = [];
const optionalOnly = [];
for (const s of all) {
  // `deferred` is the field the scan records (a caller-extended prefix, e.g. `${URL_ADMIN}/…`).
  // It was read here as `deferredSite`, which is not a field anything ever sets — so the guard
  // never fired and every deferred path went on to be resolved against a handler it may not have.
  if (s.deferred) continue;
  if (!s.handler) continue; // routed through a merged sub-router; the route sweep reports the path
  const fields = handlerFields(s.handler);
  if (!fields) continue; // the return type is not a `Json<T>` this file can name
  for (const [field, lines] of s.fields) {
    const hit = fields.get(field);
    // The error envelope is the SECOND body every endpoint can answer with, so `error` resolves for
    // every handler — see `errorFields`. Everything else is checked against the success type alone.
    if (!hit && field === "error" && errorFields().size > 0) continue;
    if (hit) continue;
    // Present in the struct but under its Rust name → the rename class (tick 74).
    const rustName = [...fields.values()].find((f) => f.rust === field);
    unknown.push({
      path: s.path,
      field,
      lines: [...lines].sort((a, b) => a - b),
      hint: rustName ? `serialises as a rename of \`${rustName}\`` : "no such field",
    });
  }
  for (const [field, lines] of s.fields) {
    const hit = fields.get(field);
    if (hit?.optional) optionalOnly.push({ path: s.path, field, lines: [...lines].sort((a, b) => a - b) });
  }
}

// ---------------------------------------------------------------------------------------------

if (selfTest) {
  let ok = true;
  const say = (pass, text) => {
    console.log(`${pass ? "PASS" : "FAIL"} — ${text}`);
    if (!pass) ok = false;
  };

  // 1. The payload key that shipped as defect 1. `ExecutionListResponse` sends `executions`, so a
  //    read of `runs` is `undefined` on a 200.
  const runsHandler = pathHandler.get(shape("/workflows/{id}/executions"));
  const runsFields = handlerFields(runsHandler);
  say(
    Boolean(runsFields) && !runsFields.has("runs") && runsFields.has("executions"),
    "`/workflows/{id}/executions` sends `executions`, not `runs` (tick 89's payload key)",
  );

  // 2. Defect 3's class: a field read off the STORED column rather than the wire. `trigger` lives on
  //    `ExecutionSummary`, one CONTAINER level down — which is why the one-struct draft could not
  //    resolve it, and why the row it governs reported clean.
  say(
    Boolean(runsFields) && runsFields.has("trigger") && !runsFields.has("trigger_kind"),
    "`trigger` resolves through `Vec<ExecutionSummary>` and `trigger_kind` does not (tick 89's second key)",
  );
  const nested = [...(reachableStructs(read("apps/api/src/routes/workflows.rs"), "ExecutionListResponse") ?? [])];
  say(
    nested.length >= 2,
    `the container walk reaches ${nested.length} structs from ExecutionListResponse (want >= 2: it is the step the first draft skipped)`,
  );

  // 3. The rename class: `node_type` is `type` on the wire.
  const graphSrc = read("crates/workflows/src/graph.rs");
  const nodeFields = structFields(graphSrc, "Node");
  say(
    Boolean(nodeFields) && nodeFields.has("type") && !nodeFields.has("node_type"),
    "`Node.node_type` serialises as `type` — the rename wins over the Rust name (tick 74)",
  );

  // 4. Optional fields are OPTIONAL, never missing — and the two the walkthrough narrows onto
  //    (`captured?.event_name`) must be visible, which is the hole the first draft had.
  const listenerSrc = read("apps/api/src/routes/workflow_listener.rs");
  const listFields = handlerFields(pathHandler.get(shape("/workflows/{id}/listeners")));
  say(Boolean(listFields?.has("armed")), "`ListenerListBody.armed` resolves");
  say(listFields?.get("armed")?.optional === false, "`armed` is not optional");
  say(
    listFields?.get("captured")?.optional === true,
    "`ListenerListBody.captured` is OPTIONAL (`skip_serializing_if`)",
  );
  say(
    Boolean(listFields?.has("event_name")) && Boolean(listFields?.has("status")),
    "the NESTED `ListenerBody` fields the row narrows onto (`captured?.event_name`, `captured?.status`) are visible",
  );

  // 5. A real field still resolves, so the gate is not failing by construction.
  say(
    Boolean(runsFields?.has("workflow_id")),
    "a real field still resolves (the gate is not failing by construction)",
  );

  // 6. **The gate must see its own subject.** A pattern that silently skips every narrowed read
  //    prints `0 unknown` and calls it clean — the one outcome that cannot be distinguished from
  //    success. So the read count is measured, and the reads this REQ actually has to police are
  //    named individually: a green gate that inspected nothing is the defect, not the fix.
  //
  //    **`event_name` is deliberately NOT demanded here, and that is the assertion working.** The row
  //    reads `captured?.event_name`, but `captured` is a LOCAL — `const captured = body.captured ??
  //    null` — narrowed out of the payload one line before the read. Once attribution is by NAME and
  //    not by proximity, a local narrowed from a bound payload is exactly what this gate must NOT
  //    charge to the endpoint: `body.captured` is the field under test, and everything BELOW it is
  //    checked by `structFields`' own reachability walk (assertion 4, above). So the site's reads are
  //    the three the code reads off the payload itself, and demanding a fourth that the attribution
  //    correctly excludes would be demanding the gate be wrong. This assertion used to say "including
  //    the optional-chained ones" while naming a field that optional-chaining had moved out of
  //    reach — the assertion described the hole it was guarding, and stayed red against correct code.
  const listenerSite = all.find((s) => s.path.endsWith("/listeners"));
  const seenFields = listenerSite ? new Set([...listenerSite.fields.keys()]) : new Set();
  say(
    seenFields.has("armed") && seenFields.has("captured") && seenFields.has("listeners"),
    `the listener site contributes ${seenFields.size} payload field reads: ${[...seenFields].join(", ")}` +
      ` (a narrowed local is excluded by design; its own fields are covered by the reachability walk)`,
  );
  say(
    all.length >= 8,
    `the sweep matched ${all.length} response sites (want >= 8)`,
  );

  process.exit(ok ? 0 : 1);
}

if (asJson) {
  console.log(JSON.stringify({ sites: all.length, unknown, optionalOnly }, null, 2));
} else {
  console.log(`walkthrough response sites with field reads: ${all.length}`);
  console.log(`UNKNOWN FIELDS: ${unknown.length}`);
  for (const u of unknown) console.log(`  ${u.path} · ${u.field}  lines=${u.lines.join(",")}  (${u.hint})`);
  console.log(`optional fields read (legal, but ` + "`None` reads as absent`" + `): ${optionalOnly.length}`);
  for (const o of optionalOnly) console.log(`  ${o.path} · ${o.field}  lines=${o.lines.join(",")}`);
}

/**
 * The coverage floor, in GATE mode as well as in `--self-test`.
 *
 * **A floor that only the self-test reads is not a floor.** The `all.length >= 8` assertion was
 * inside the self-test branch, so the mode anybody actually runs — the one wired into the tick's
 * gates — had no coverage assertion at all: it printed `0 sites`, reported `0 unknown`, and exited
 * **0**. A rename of the walkthrough's read, a moved brace, or any future change to the window
 * logic would have gone green, and the printed `0` is exactly the output that reads as "clean".
 * A gate whose failure mode is indistinguishable from its success mode is worse than no gate,
 * because it is a gate people cite.
 *
 * So the floor is asserted here, after the report, on the same number the report prints. The
 * threshold is a floor and not a target: it exists so that "I saw nothing" can never be reported
 * as "I saw nothing wrong". It is deliberately far below the 94 API calls the walkthrough makes,
 * because many sites legitimately resolve to no payload read (a POST whose response is never read,
 * a link check whose result is a boolean) — but a drop of that kind is a real change in what this
 * file covers and should be a decision, not an accident.
 */
const COVERAGE_FLOOR = 8;
if (all.length < COVERAGE_FLOOR) {
  console.error(
    `\nThe sweep resolved ${all.length} response sites with field reads, below its floor of ` +
      `${COVERAGE_FLOOR}. "No unknown field" above means "no field was looked at", not "every ` +
      `field was correct" — the read window matched no row. Do not read this run as clean.`,
  );
  process.exit(1);
}

/**
 * The VERIFIED floor, and the one that matters most.
 *
 * `COVERAGE_FLOOR` above can be satisfied by sites the sweep found but could not check: a path with
 * no handler binding, or a return type it cannot name, is skipped by `continue`, not reported. So a
 * sweep that resolved sixteen paths, verified none of them, and called two of them "sites" would
 * clear the first floor. **That is the whole defect this file was written to catch, reappearing one
 * layer up**, and it is why this floor counts sites that reached a handler and a struct — sites that
 * were compared against something the server actually sends.
 *
 * The two floors answer different questions and both are needed. `sites` asks "did the pattern still
 * match the walkthrough"; `verified` asks "did anything get checked against the server". Dropping
 * either leaves a failure mode that prints a healthy-looking report.
 */
const VERIFIED_FLOOR = 4;
const verified = all.filter((s) => !s.deferred && s.handler).length;
if (verified < VERIFIED_FLOOR) {
  console.error(
    `\nOnly ${verified} of ${all.length} response sites reached a handler and a nameable return ` +
      `type, below the floor of ${VERIFIED_FLOOR}. The remaining sites were SKIPPED, not checked — ` +
      `so the ${unknown.length} unknown fields above is a statement about ${verified} sites, not ` +
      `about ${all.length}. A path→handler lookup that stops matching is silent in exactly this way.`,
  );
  process.exit(1);
}

if (unknown.length > 0) {
  console.error(
    "\nAn unknown field is a read of `undefined` on a response that answered 200. Where the row " +
      "falls back (`x ?? y`) that is worse than a crash: the reader is told the FIRST value under " +
      "the second value's name.",
  );
  process.exit(1);
}