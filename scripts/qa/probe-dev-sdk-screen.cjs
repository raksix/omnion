#!/usr/bin/env node
/**
 * The SDK screen's hooks, its colour tokens, and the deep link the terminal prints.
 *
 * The sibling `probe-dev-event-screen.cjs` explains why a hook cross-check and a token check
 * are one file. This one adds a third claim that only this screen has:
 *
 * **3. `verification_uri` is a URL the platform hands out.** `DeviceStart` carries
 * `/developer/sdks?tab=cli`, the CLI prints it, and the person opens it. If the tab strip is
 * held only in React state the deep link lands on the plugin generator — a working page, so no
 * route walk, no build and no `tsc` notices. The server side of the pair lives in
 * `crates/developer/src/store_cli.rs`; this file asserts both halves name the same thing, which
 * is the check a rename would break silently. A deep link that stops working is a *silent*
 * failure: the destination is fine, so every gate reports green.
 *
 * The token check reads `--color-*` out of `globals.css` rather than a typed palette, so a
 * rebrand moves the truth to one place.
 */

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const VIEW = path.join(ROOT, "apps/admin/features/developer/developer-sdks-view.tsx");
const PAGE = path.join(ROOT, "apps/admin/app/developer/sdks/page.tsx");
const WALK = path.join(ROOT, "scripts/qa/walkthrough.cjs");
const CSS = path.join(ROOT, "apps/admin/app/globals.css");
const STORE = path.join(ROOT, "crates/developer/src/store_cli.rs");

let failures = 0;
const pass = (msg) => console.log(`PASS  ${msg}`);
const fail = (msg) => {
  failures += 1;
  console.log(`FAIL  ${msg}`);
};
const uniq = (s) => [...new Set(s)];
const view = fs.readFileSync(VIEW, "utf8");
const page = fs.readFileSync(PAGE, "utf8");
const walk = fs.readFileSync(WALK, "utf8");
const css = fs.readFileSync(CSS, "utf8");
const store = fs.readFileSync(STORE, "utf8");

// ---- 1. Every hook the pass drives exists in the view -----------------------------------------
const demanded = uniq((walk.match(/data-dev-sdk-[a-z-]*/g) || []).sort());
const present = new Set(view.match(/data-dev-sdk-[a-z-]*/g) || []);

if (demanded.length === 0) {
  fail("the pass names no data-dev-sdk-* hook at all — the route is probably unvisited");
} else {
  for (const hook of demanded) {
    if (present.has(hook)) pass(`${hook} — driven by the pass and present in the view`);
    else fail(`${hook} — the pass drives it but the view does not carry it`);
  }
}

const unused = [...present].filter((h) => !demanded.includes(h)).sort();
console.log(
  `NOTE  ${present.size} hooks in the view, ${demanded.length} driven by the pass` +
    (unused.length ? `, ${unused.length} not yet driven: ${unused.join(", ")}` : ", none unused"),
);

// ---- 2. The CLI deep link, both halves ----------------------------------------------------------
const serverUri = (store.match(/verification_uri:\s*"([^"]+)"/) || [])[1];
if (!serverUri) {
  fail("store_cli.rs no longer states a verification_uri — the deep link claim cannot be checked");
} else if (serverUri !== "/developer/sdks?tab=cli") {
  fail(`the server now points the terminal at ${serverUri}, not at /developer/sdks?tab=cli`);
} else {
  pass("the server hands the terminal /developer/sdks?tab=cli");
}

// The view must read it **in the state initialiser**. This is deliberately narrower than
// "does the view read ?tab=" — and the first version of this probe was that wider check, and it
// reported GREEN on the exact dead button it exists to catch. `search.get("tab")` also appears
// in the effect that resynchronises state with the URL, so a view whose initial state is
// hardcoded still "reads" the parameter: it paints the plugin generator, then corrects itself a
// frame later. On a deep link a person opens from a terminal that is the whole interaction, and
// a gate that cannot see it is a gate that lies.
//
// So the claim is structural: `useState<Tab>(() => initialTab(search.get("tab")))`.
if (/useState<Tab>\(\(\) => initialTab\(search\.get\("tab"\)\)\)/.test(view)) {
  pass("the tab is INITIALISED from ?tab= — the terminal's deep link paints the right tab first");
} else {
  fail(
    "the tab's initial state is not derived from ?tab= — the deep link paints the wrong tab " +
      "before the effect corrects it",
  );
}

// An unknown value must fall back rather than blank the strip. `?tab=wat` is what a person
// types when they edit the URL, and a screen that renders no tab at all is worse than a wrong
// default. The fallback is a named function for the same reason as above: it is the second half
// of the same claim, and a named function is something a check can point at.
if (/export function initialTab\(value: string \| null\): Tab/.test(view) && /function isTab\(/.test(view)) {
  pass("an unrecognised ?tab= falls back to a real tab instead of rendering none");
} else {
  fail("the view has no initialTab()/isTab() fallback — a hand-edited ?tab= can leave no tab selected");
}

// The tab strip must also WRITE the selection back, or a reload silently throws the person back
// to the plugin generator and the terminal's own instruction becomes a stale tip.
if (/router\.replace\(`\/developer\/sdks\?/.test(view)) {
  pass("selecting a tab rewrites ?tab= — a reload keeps the tab the person chose");
} else {
  fail("a tab switch does not rewrite the URL — reloading drops the person back to the default tab");
}

// The route must be under a Suspense boundary or `useSearchParams` fails the BUILD, not the
// editor. This is the same trap the webhook form's `?event=` walked into earlier this wave.
if (/<Suspense/.test(page) && /<DeveloperSdksView/.test(page)) {
  pass("the route wraps the view in Suspense — useSearchParams cannot fail the build here");
} else {
  fail("the route does not wrap DeveloperSdksView in Suspense — next build will fail on it");
}

// ---- 3. Colour tokens actually exist -------------------------------------------------------------
const LAYOUT_UTILS = new Set([
  "center", "left", "right", "justify", "start", "end",
  "xs", "sm", "base", "lg", "xl", "2xl", "3xl",
  "bold", "semibold", "medium", "normal", "light",
  "nowrap", "ellipsis", "uppercase", "lowercase", "capitalize", "truncate",
  "t", "r", "b", "l", "x", "y", "s", "e", "w", "2", "4", "8",
  "clip", "none", "auto", "transparent", "current", "inherit",
]);

const declared = new Set(
  [...css.matchAll(/--color-([a-z0-9-]+)\s*:/g)].map((m) => m[1]),
);

const tokens = uniq(
  [...view.matchAll(/(?:text|bg|border)-([a-z][a-z0-9-]*)/g)]
    .map((m) => m[1])
    .filter((t) => !LAYOUT_UTILS.has(t) && !/^\[/.test(t)),
).sort();

for (const token of tokens) {
  if (declared.has(token)) {
    pass(`--color-${token} — used in the view and declared in globals.css`);
  } else {
    fail(
      `${token} — used as a colour in the view but declared nowhere in globals.css ` +
        `(it renders in the inherited colour)`,
    );
  }
}

// Every field on this screen must be labelled, not merely accompanied by a placeholder. The walk
// reported `unlabeled-input` on `/developer/sdks` and it was right: the manifest textarea had a
// heading above it and a JSON placeholder in it, and a screen reader announces "edit text, blank".
// A placeholder is not a label -- it disappears as soon as the field has content, which is exactly
// when a person is reading the screen to check what they typed.
//
// Counted per field rather than in aggregate, and the mutation run is why: the first version
// compared "number of fields" with "number of label-ish tokens in the file", which
// `aria-label="Developer tooling"` on the *tablist* satisfied -- so deleting the textarea's label
// left the gate green. An aggregate count is satisfied by any one correct thing; only pairing each
// field with its own label says what the claim says.
// Every field on this screen must be labelled, not merely accompanied by a placeholder. The walk
// reported `unlabeled-input` on `/developer/sdks` and it was right: the manifest textarea had a
// heading above it and a JSON placeholder in it, and a screen reader announces "edit text, blank".
// A placeholder is not a label -- it disappears as soon as the field has content, which is exactly
// when a person is reading the screen to check what they typed.
//
// What this check can honestly prove from the source is that every field has EITHER an explicit
// association (`htmlFor`/`aria-label`/`aria-labelledby` on the field's own tag) OR is nested in a
// `<label>` element. It deliberately does not try to be a full JSX parser: the first three
// versions of this check were regex attempts to read a field's attributes out of JSX, and each one
// failed for a different reason (an arrow function's `>` ends the tag; a self-closing `/>` ends it
// earlier; a comment containing `<label>` is found as a label) — and a gate that cannot tell
// correct code from broken code is worse than no gate, because it is trusted.
//
// So: extract each field's opening tag to the first `>` that is not inside a `{…}` brace group,
// and separately collect the bodies of `<label>…</label>` elements. A field passes if its own tag
// carries an association, or if some label body contains it.
function openingTags(src, tag) {
  const out = [];
  const re = new RegExp("<" + tag + "\\b", "g");
  let m;
  while ((m = re.exec(src)) !== null) {
    let i = m.index + m[0].length;
    let depth = 0;
    while (i < src.length) {
      const c = src[i];
      if (c === "{") depth += 1;
      else if (c === "}") depth = Math.max(0, depth - 1);
      else if (c === ">" && depth === 0) break;
      i += 1;
    }
    out.push(src.slice(m.index, i + 1));
    re.lastIndex = i;
  }
  return out;
}

const FIELD_TAGS = ["input", "textarea", "select"];
const fieldTags = FIELD_TAGS.flatMap((t) => openingTags(view, t).map((tag) => [t, tag]));
const labelBodies = [...view.matchAll(/<label\b[^>]*>([\s\S]*?)<\/label>/g)].map((m) => m[1]);

const unlabelled = fieldTags
  .filter(([tag, opening]) => {
    // An explicit association, which is either on the field itself (`aria-label`, and the odd
    // `htmlFor`) or the ordinary one: the field carries an `id`, and a `<label htmlFor>` elsewhere
    // in the file names that same id. Both are real label associations; only the first is visible
    // on the field's own tag, so the `htmlFor` scan covers the second.
    const explicit =
      /aria-label(?:ledby)?=/.test(opening) ||
      /\bid=["']([^"']+)["']/.test(opening) &&
        [...view.matchAll(/<label\b[^>]*>/g)].some((m) =>
          m[0].includes('htmlFor="' + opening.match(/\bid=["']([^"']+)["']/)[1] + '"') ||
          m[0].includes("htmlFor='" + opening.match(/\bid=["']([^"']+)["']/)[1] + "'"),
        );
    // The implicit form — the field nested in a `<label>` element — has to be checked against the
    // label body that actually CONTAINS THIS occurrence, not "some label body contains a field of
    // this tag". Two same-tag inputs on one screen made that mistake survivable: unwrapping the
    // first field's label left the second label's `<input>` in scope, and the mutation passed.
    //
    // The honest pairing is positional: take each label body, and check that the field's own
    // opening tag appears inside one of them.
    const implicit = labelBodies.some((body) => body.includes(opening.slice(0, 60)));
    return !explicit && !implicit;
  })
  .map(([tag]) => tag);

if (fieldTags.length > 0 && unlabelled.length === 0) {
  pass(`all ${fieldTags.length} fields on the screen carry their own label`);
} else {
  fail(
    `${unlabelled.length} of ${fieldTags.length} field(s) have no label ` +
      `(${unlabelled.join(", ")}) — a placeholder is not a label, and it vanishes once the ` +
      `field has content`,
  );
}

if (/(?:text|bg|border)-danger/.test(view)) {
  fail("this view uses a `danger` token, which globals.css does not define — use `caution`");
} else {
  pass("no `danger` token in the view — the palette's warning colour is `caution`");
}

const total = demanded.length + tokens.length + 7;
console.log(
  failures === 0
    ? `\n${total}/${total} dev-sdk screen checks passed`
    : `\n${failures} check(s) failed`,
);
process.exit(failures === 0 ? 0 : 1);
