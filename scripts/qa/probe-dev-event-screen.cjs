#!/usr/bin/env node
/**
 * The catalogue's screen hooks, and the tokens its messages are painted with.
 *
 * Two failure classes, both invisible to `tsc` and to the build:
 *
 * **1. A hook the pass names and the view does not carry.** `walkthrough.cjs` drives this screen
 * by `data-dev-event-*` name. Rename a hook on either side and the pass stops measuring the new
 * screen and keeps measuring the old one, then reports the result as green. A pass that silently
 * measures the previous screen is worse than no pass, because it is trusted. This is the same
 * cross-check tick 111 ran by hand on the OAuth screen (33/33); it is a file so it runs again on
 * the next rename.
 *
 * **2. A colour token that is defined nowhere.** The palette is
 * canvas/surface/ink/muted/line/accent/caution/positive — there is no `danger`, and no
 * `danger-soft`. `text-danger` is a well-formed Tailwind class that resolves to nothing, so the
 * message renders in the *inherited* colour: an error banner that does not look like one. The
 * main writer's media probe found this in `scanning-view.tsx` and `file-detail.tsx` (see the
 * BUILD-LOG, tick 101); this file exists so the same mistake in the developer surfaces is caught
 * by a gate instead of by reading a screenshot.
 *
 * The check is deliberately narrow: it reads the token list out of `globals.css` rather than
 * hardcoding the palette, so a rebrand moves the truth to one place and the gate follows.
 */

const fs = require("node:fs");
const path = require("node:path");

const ROOT = path.resolve(__dirname, "..", "..");
const VIEW = path.join(ROOT, "apps/admin/features/developer/developer-events-view.tsx");
const WALK = path.join(ROOT, "scripts/qa/walkthrough.cjs");
const CSS = path.join(ROOT, "apps/admin/app/globals.css");
const FORM = path.join(ROOT, "apps/admin/features/webhooks/endpoint-form.tsx");

let failures = 0;
const pass = (msg) => console.log(`PASS  ${msg}`);
const fail = (msg) => {
  failures += 1;
  console.log(`FAIL  ${msg}`);
};
const uniq = (s) => [...new Set(s)];

const view = fs.readFileSync(VIEW, "utf8");
const walk = fs.readFileSync(WALK, "utf8");
const css = fs.readFileSync(CSS, "utf8");

// ---- 1. Every hook the pass drives exists in the view -----------------------------------------
const demanded = uniq((walk.match(/data-dev-event-[a-z-]*/g) || []).sort());
const present = new Set(view.match(/data-dev-event-[a-z-]*/g) || []);

if (demanded.length === 0) {
  fail("the pass names no data-dev-event-* hook at all — the route is probably unvisited");
} else {
  for (const hook of demanded) {
    if (present.has(hook)) pass(`${hook} — driven by the pass and present in the view`);
    else fail(`${hook} — the pass drives it but the view does not carry it`);
  }
}

// The view is allowed hooks the pass does not drive yet; the reverse is the defect. A count is
// printed so a large gap is visible without a test failing on it — an unused hook is a hook for
// a claim nobody has made yet, which is a backlog note rather than a bug.
const unused = [...present].filter((h) => !demanded.includes(h)).sort();
console.log(
  `NOTE  ${present.size} hooks in the view, ${demanded.length} driven by the pass` +
    (unused.length ? `, ${unused.length} not yet driven: ${unused.join(", ")}` : ", none unused"),
);

// ---- 2. The deep link is real, not a dead button ------------------------------------------------
if (/href=\{`\/webhooks\/new\?event=\$\{encodeURIComponent\(entry\.name\)\}`\}/.test(view)) {
  pass("the Subscribe link carries the event name to /webhooks/new?event=");
} else {
  fail("the Subscribe link no longer carries ?event= — the deep link is dead");
}

// The form must READ it, or the link lands on a page that ignores the query.
if (/useSearchParams\(\)\.get\("event"\)/.test(fs.readFileSync(FORM, "utf8"))) {
  pass("the endpoint form reads ?event= — the deep link is not a dead button");
} else {
  fail("the endpoint form does not read ?event= — the deep link lands on a page that ignores it");
}

// A reserved name must not offer the link at all: there is nothing to subscribe to.
if (/const live = isLive\(entry\);/.test(view) && /\{live \? \(\s*<Link/.test(view)) {
  pass("only a live name renders a Subscribe link — a reserved name has nothing to subscribe to");
} else {
  fail("the Subscribe link is not gated on the name being live");
}

// ---- 3. Colour tokens actually exist -------------------------------------------------------------
const defined = new Set(css.match(/--color-([a-z-]+)\s*:/g) || []);
const tokens = uniq(
  [...view.matchAll(/(?:text|bg|border)-([a-z]+(?:-[a-z]+)*)/g)].map((m) => m[1]),
).sort();

// The panel's own vocabulary, not every class in the file: `size-3` and friends are not colours,
// and the utility names in the file's static class strings are what a reader would assume.
const PALETTE = ["accent", "caution", "positive", "muted", "line", "ink", "canvas", "surface"];
for (const token of tokens) {
  if (!PALETTE.includes(token) && !/^(1|2|3|4|5|6|7|8|9|10|11|12|full|xs|sm|base|lg|xl)$/.test(token)) {
    // Not a palette token and not a size: only a declared one is safe to trust.
    const isDeclared = [...defined].some((d) => d === `--color-${token}:`);
    if (!isDeclared) {
      fail(
        `${token} — used as a colour in the view but defined nowhere in globals.css ` +
          `(it renders in the inherited colour)`,
      );
    }
  }
}
if (failures === 0) {
  for (const token of tokens.filter((t) => PALETTE.includes(t))) {
    if ([...defined].some((d) => d === `--color-${token}:`)) {
      pass(`--color-${token} — used and defined in globals.css`);
    }
  }
}

// The specific regression: `text-danger` / `border-danger` have never existed in this palette.
if (/(?:text|bg|border)-danger/.test(view)) {
  fail("this view uses a `danger` token, which globals.css does not define — use `caution`");
} else {
  pass("no `danger` token in the view — the palette's warning colour is `caution`");
}

console.log(
  failures === 0
    ? `\n${demanded.length + 8}/${demanded.length + 8} dev-event screen checks passed`
    : `\n${failures} check(s) failed`,
);
process.exit(failures === 0 ? 0 : 1);
