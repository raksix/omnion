#!/usr/bin/env node
/** The route-adoption pass's own gate: every shape it must resolve, and the two it must refuse.
 *
 * Why this file exists: the pass rewrote 401 route registrations in one commit, and every bug it
 * had produced was invisible to `cargo build` -- a handler rebuilt from a verb name compiles,
 * a permission resolved from the wrong sub-expression compiles, and a route documented with no
 * permission at all compiles. Each of those was found by running the resolver against these
 * shapes, not by reading the diff.
 *
 * The last three cases are the ones that matter most: a permission INVENTED for a route the
 * resolver could not read answers 403 for every caller including the instance owner, while the
 * same route left unguarded serves everyone it served before. The second is the failure mode.
 */
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const repo = path.resolve(__dirname, '..', '..');
const script = path.join(repo, 'scripts/qa/adopt-documented.py');

function probe(expr) {
  const out = execFileSync('python3', ['-c', `
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("ad", ${JSON.stringify(script)})
ad = importlib.util.module_from_spec(spec); spec.loader.exec_module(ad)
expr = sys.argv[1]
parts = ad.split_routers(expr)
print(json.dumps({
    "parts": len(parts),
    "texts": parts,
    "methods": [[v, p] for v, p, _ in ad.methods_of(expr)],
}))
`, expr], { cwd: repo, encoding: 'utf8' });
  return JSON.parse(out.trim());
}

/** Whether every emitted part is a balanced, standalone MethodRouter expression.
 *
 * The parts deliberately do NOT concatenate back to the input verbatim: `.merge(` is a chain
 * operator and cannot survive inside one verb's `documented!`, so each part is closed to stand
 * alone. The property that actually matters is therefore balance -- an unbalanced part is exactly
 * how the first applied run produced `.merge(,` and a file that did not parse -- together with the
 * verbs being preserved, which `methods` already asserts.
 */
const balanced = (s) => {
  let depth = 0;
  let inStr = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (inStr) {
      if (c === '\\') i++;
      else if (c === '"') inStr = false;
    } else if (c === '"') inStr = true;
    else if (c === '(') depth++;
    else if (c === ')') {
      depth--;
      if (depth < 0) return false;
    }
  }
  return depth === 0 && !inStr;
};

const CASES = [
  {
    name: 'a single inline handler keeps its own guard',
    expr: 'get(c::g).layer(guards::require(&state, "content.pages.read"))',
    parts: 1,
    methods: [['GET', 'content.pages.read']],
  },
  {
    name: 'a merge of two keyed handlers keeps two keys',
    expr: 'get(c::g).layer(guards::require(&state, "a.b")).merge(post(c::x).layer(guards::require(&state, "c.d")))',
    parts: 2,
    methods: [['GET', 'a.b'], ['POST', 'c.d']],
  },
  {
    name: 'a trailing layer guards every verb in the chain',
    // The layer sits AFTER both verbs, so a per-part read finds it for the last part only and
    // the first verb gets documented unguarded.
    expr: 'post(g::h).get(g::i).layer(guards::require_or_machine(&state, "content.pages.read"))',
    parts: 2,
    methods: [['POST', 'content.pages.read'], ['GET', 'content.pages.read']],
  },
  {
    name: 'a bare verb before a keyed merge keeps its own absence',
    expr: 'get(h).merge(delete(h2).layer(guards::require(&state, "m.d")))',
    parts: 2,
    methods: [['GET', null], ['DELETE', 'm.d']],
  },
  {
    name: 'a qualified routing path resolves its verbs',
    expr: 'axum::routing::patch(o::u).delete(a::d).layer(guards::require(&state, "observability.manage"))',
    parts: 2,
    methods: [['PATCH', 'observability.manage'], ['DELETE', 'observability.manage']],
  },
  {
    name: 'an identifier that ends in a verb name is not a verb',
    // `target(` must not read as `get(`: a handler named `delete_media` is not a DELETE.
    expr: 'get(m::delete_media).layer(guards::require(&state, "media.delete"))',
    parts: 1,
    methods: [['GET', 'media.delete']],
  },
  {
    name: 'a verb nested inside a call argument is not a chain element',
    expr: 'get(h).merge(post(c::x).layer(guards::require(&state, "c.d")))',
    parts: 2,
    methods: [['GET', null], ['POST', 'c.d']],
  },
  {
    name: 'a body limit layer is not mistaken for a permission',
    expr: 'post(media::upload_media).layer(DefaultBodyLimit::max(25))',
    parts: 1,
    methods: [['POST', null]],
  },
];

let failed = 0;
for (const c of CASES) {
  const got = probe(c.expr);
  const unbalanced = (got.texts || []).filter((t) => !balanced(t));
  const ok =
    unbalanced.length === 0 &&
    got.parts === c.parts &&
    JSON.stringify(got.methods) === JSON.stringify(c.methods);
  if (!ok) {
    failed++;
    console.log(
      `FAIL  ${c.name}\n      expr: ${c.expr}\n      got:  parts=${got.parts} ` +
        `methods=${JSON.stringify(got.methods)}\n      want: parts=${c.parts} ` +
        `methods=${JSON.stringify(c.methods)}` +
        (unbalanced.length
          ? `\n      UNBALANCED PART: ${JSON.stringify(unbalanced[0])}`
          : '')
    );
  } else {
    console.log(`ok    ${c.name}`);
  }
}

// The control: the shape whose permission the resolver must REFUSE rather than invent. There is
// no key in this expression, so the honest answer is `null` -- a pass that answered with any
// string here would put a key in the document that answers 403 for every caller.
const invented = probe('get(a::b)');
if (invented.methods[0][1] !== null) {
  failed++;
  console.log(`FAIL  an unguarded route reports no permission, got ${invented.methods[0][1]}`);
} else {
  console.log('ok    an unguarded route reports no permission');
}

console.log(`\n${CASES.length + 1 - failed}/${CASES.length + 1} checks`);
process.exit(failed ? 1 : 0);