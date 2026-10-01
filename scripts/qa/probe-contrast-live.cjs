/**
 * The contrast panel is a MEASUREMENT of a palette, so it has to measure the palette the
 * operator is looking at. It does not, and this probe is why the gate knows.
 *
 * The screen keeps two things: `form` (what the operator is EDITING) and `view` (the last
 * SERVER response). The panel renders `view.contrast` beside a live preview of `form`, and the
 * all-clear it prints — "Every text/background pair in this draft meets WCAG AA" — is read by
 * an operator as a statement about the draft on screen. It is a statement about the last save.
 *
 * The consequence is a screen that contradicts itself and then blames the operator: edit the
 * accent into a failing pair, and the panel still says every pair passes, while the preview
 * right beside it shows the failing colours. Press *Publish* and the server refuses with
 * `theme_settings_contrast_required`, whose whole instruction is "Read the contrast panel, then
 * publish again" — sending the operator to a panel that just told them everything was fine.
 *
 * The same bug on the acknowledgement side is worse than a stale label. `acknowledge` is sent
 * from `view.contrast.length === 0 || contrastSeen`, so the browser decides whether the
 * server's guard runs. A measurement the browser takes is not the guard, it is a client-side
 * substitute for it.
 *
 * ## What this probe checks
 *
 * 1. There is a server dry-run route for the palette being edited, and the panel calls it.
 * 2. The findings the panel renders come from the LIVE measurement, not from `view.contrast`.
 * 3. The all-clear cannot be printed while the panel holds an unmeasured draft.
 * 4. `acknowledge` is only ever sent with the server's own findings in hand.
 * 5. The acknowledgement is retired the moment the palette changes (an acknowledgement of a
 *    finding set is not an acknowledgement of a different one).
 *
 * Run: `node scripts/qa/probe-contrast-live.cjs` (reads the real tree; no server needed).
 */
const fs = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..", "..");
const VIEW = path.join(root, "apps/admin/features/themes/theme-customize-view.tsx");
const ROUTE = path.join(root, "apps/api/src/routes/theme_settings.rs");
const API = path.join(root, "apps/admin/lib/api.ts");
const STORE = path.join(root, "crates/content/src/theme_settings.rs");

/** A missing file reads as an empty string, so an absent implementation fails every check. */
const read = (file) => (fs.existsSync(file) ? fs.readFileSync(file, "utf8") : "");
const view = read(VIEW);
const route = read(ROUTE);
const api = read(API);
const store = read(STORE);

const checks = [];
const check = (name, ok) => checks.push({ name, ok: Boolean(ok) });

// ------------------------------------------------------------------ 1. the server measures the draft
// A dry run that computes nothing is worse than none: it would answer `[]` and the panel would
// print its all-clear with a measurement behind it. So the body is checked for the actual
// computation, not merely for the word "contrast".
const PREVIEW_HANDLER =
  /pub async fn\s+(preview_settings|check_contrast|preview_contrast)[\s\S]{0,2200}?contrast_report\s*\(/;
check("server has a dry-run that computes contrast_report", PREVIEW_HANDLER.test(route));

// The dry run must apply the theme DEFAULTS under the submitted overrides, exactly as publish
// does. Measuring the overrides alone is a different measurement: a token the draft does not
// set is still rendered from the theme, so measuring without the merge reports a palette the
// site will never paint.
check(
  "the dry run merges the theme defaults under the submitted overrides",
  /merge_over\s*\(/.test(
    (route.match(/pub async fn\s+(?:preview_settings|check_contrast|preview_contrast)[\s\S]{0,2600}/) || [
      "",
    ])[0] || "",
  ),
);

// It writes nothing: a route whose purpose is to be called on every keystroke cannot be a save.
const previewBody =
  (route.match(/pub async fn\s+(?:preview_settings|check_contrast|preview_contrast)[\s\S]{0,2600}/) || [
    "",
  ])[0] || "";
check(
  "the dry run writes no revision",
  !/theme_settings::(save|insert_revision|publish)\s*\(/.test(previewBody),
);

// The permission is the one the read uses: reading a measurement of your own draft is reading.
// The guard is a `Layer` in `routes/mod.rs`, not in the handler file — a check that looked for
// it here would fail forever against a correctly wired route, so it reads the wiring.
const mod = read(path.join(root, "apps/api/src/routes/mod.rs"));
check(
  "the dry run is guarded like the read it mirrors",
  /theme_settings_contrast_check\s*=\s*post\([\s\S]{0,200}?themes\.read/.test(mod),
);

// ------------------------------------------------------------------ 2. the client calls it
check(
  "the client has a typed contrast-preview call",
  /themeSettingsContrastPreview|previewThemeSettingsContrast|checkThemeSettingsContrast/.test(api),
);
check(
  "the client call posts the draft's tokens",
  /(?:themeSettingsContrastPreview|previewThemeSettingsContrast|checkThemeSettingsContrast)[\s\S]{0,400}?tokens/.test(
    api,
  ),
);

// ------------------------------------------------------------------ 3. the panel renders the live answer
// The defect itself: `view.contrast` is the last server read, and the form holds the edits.
check(
  "the rendered findings are NOT the last server read",
  !/const findings = view\.contrast;/.test(view),
);
check(
  "the findings come from the live measurement",
  /const findings = liveContrast/.test(view) || /const findings = contrastFindings/.test(view),
);

// ------------------------------------------------------------------ 4. no unmeasured all-clear
// The all-clear must not be printable while a measurement is in flight or failed, because the
// whole claim is "every pair meets AA" and an unmeasured draft has not been shown to.
check(
  "the all-clear is withheld while a measurement is pending",
  /measuringContrast|contrastPending/.test(view),
);

// ------------------------------------------------------------------ 5. the acknowledgement rides the server's findings
// The source form is optional-chained — `(liveContrast?.length ?? 1) === 0` — because an
// UNMEASURED palette must not be able to read as "nothing to acknowledge". Matching the
// `view.contrast` spelling here would let a later refactor quietly restore the stale read and
// the gate would still be green, so the check names the live state explicitly.
check(
  "publish sends the acknowledgement only with the server's findings in hand",
  /const acknowledge = \(liveContrast\?\.length \?\? 1\) === 0 \|\| contrastSeen;/.test(view),
);

// An acknowledgement must die when the palette changes: acknowledging findings A and then
// publishing findings B is the operator being asked to confirm something they never read.
const retires = (view.match(/setContrastSeen\(false\)/g) || []).length;
check(
  "the acknowledgement is retired on every token edit",
  retires >= 4,
);

const failed = checks.filter((c) => !c.ok);
for (const c of checks) {
  console.log(`${c.ok ? "PASS" : "FAIL"}  ${c.name}`);
}
console.log(`\n${checks.length - failed.length}/${checks.length} passed`);
process.exit(failed.length === 0 ? 0 : 1);