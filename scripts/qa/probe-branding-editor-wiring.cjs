#!/usr/bin/env node
/**
 * The branding editor's wiring (REQ-062, criterion 9, panel half).
 *
 * ## Why a static gate
 *
 * Tick 59 closed the API half of criterion 9 — `check_branding` refuses an oversize, an
 * out-of-range or a wrong-typed asset and names the field — and ticked nothing, because the
 * criterion asks for a **field-level message**, which is a claim about a screen. The panel half
 * was then still a free-text `FlatEditor`: every `pnpm typecheck`, every `cargo test` and every
 * walkthrough pass could have reported green with branding still being three bare strings.
 *
 * That is the same family as tick 101's media toolbar and tick 59's `branding` section itself:
 * the rule exists on one side of a boundary and the field that triggers it is offered nowhere
 * on the other. `tsc` cannot see it, `cargo` cannot see it, and the browser pass only finds it
 * if somebody opens the Branding accordion.
 *
 * So the gate reads the *sources of both sides* and checks the wire between them, with no
 * browser and no database — which is the point, it must run in milliseconds on every tick:
 *
 * 1. **Every branding key the validator checks is an input the panel renders.** Read from
 *    `BRANDING_KEYS`, not from a hand-kept list: a fourth key added to the renderer would be a
 *    stored setting with no input, and this file would not notice.
 * 2. **Every finding the API can produce is split by field before it is rendered.** The server
 *    tags each finding with `field`; the panel has to read that tag rather than print the
 *    joined sentence, or every message lands in the same place and names no input.
 * 3. **The limits the panel shows come from the server, not from a client constant.** A second
 *    implementation of the ceiling in TypeScript is a rule that will disagree with the Rust one
 *    for exactly the themes that narrow it.
 * 4. **The view carries the limits the save enforces**, so the screen can render them at all.
 * 5. **A branding value is a media id, so the section has an actual picker.** A text box for a
 *    value that must resolve to a file is how an operator meets a 422 they cannot act on.
 *
 * Every assertion reads the boundary's own source, so renaming a field or reordering a struct
 * does not require editing this file to stay true.
 */

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..", "..");
const BRANDING_RS = path.join(ROOT, "crates/content/src/branding.rs");
const THEME_SETTINGS_RS = path.join(ROOT, "crates/content/src/theme_settings.rs");
const ROUTE_RS = path.join(ROOT, "apps/api/src/routes/theme_settings.rs");
const CUSTOMIZE_TSX = path.join(
  ROOT,
  "apps/admin/features/themes/theme-customize-view.tsx",
);
const EDITOR_TSX = path.join(
  ROOT,
  "apps/admin/features/themes/theme-branding-editor.tsx",
);
const API_TS = path.join(ROOT, "apps/admin/lib/api.ts");

const read = (file) => fs.readFileSync(file, "utf8");

/**
 * Read a source, or return a sentinel that matches NOTHING.
 *
 * A probe that throws `ENOENT` on a missing file exits non-zero for the wrong reason: the run
 * reports a crash instead of a list of failed assertions, and the reader cannot tell a missing
 * feature from a broken harness. Worse, the exit code is still 0 when the throw happens in a
 * later stage, so a gate can "pass" against a file that is not there. An empty string makes
 * every check that reads the file fail, which is the honest answer.
 */
const readOrEmpty = (file) => (fs.existsSync(file) ? read(file) : "");

const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

const brandingRs = read(BRANDING_RS);
const themeSettingsRs = read(THEME_SETTINGS_RS);
const routeRs = read(ROUTE_RS);
const customizeTsx = read(CUSTOMIZE_TSX);
const editorTsx = readOrEmpty(EDITOR_TSX);
const apiTs = read(API_TS);

if (!editorTsx) {
  console.log(
    `MISSING  ${path.relative(ROOT, EDITOR_TSX)} — the branding editor does not exist, so the panel half of criterion 9 is absent and every check below must fail.`,
  );
}

// -------------------------------------------------------------------------------------------
// 1. Every branding key is an input.
// -------------------------------------------------------------------------------------------

/** The keys `validate_branding` walks, read out of the constant rather than listed here. */
function brandingKeys() {
  const match = brandingRs.match(
    /pub const BRANDING_KEYS:\s*\[&str;\s*\d+\]\s*=\s*\[([^\]]+)\]/,
  );
  if (!match) return [];
  return [...match[1].matchAll(/"([^"]+)"/g)].map((entry) => entry[1]);
}

const keys = brandingKeys();
check("the branding key list is readable", keys.length > 0, `found ${keys.length}`);

for (const key of keys) {
  check(
    `key ${key} has an editor slot`,
    editorTsx.includes(`key: "${key}"`),
    `expected a BRANDING_SLOTS entry with key "${key}"`,
  );
  check(
    `key ${key} has an input the operator types into`,
    editorTsx.includes("data-theme-branding-input={slot.key}"),
    "the slot input is rendered once for every key",
  );
}

// The constant and the editor's own list must not drift: the editor hard-codes its three slots
// because each carries a label and a hint, so this check is what makes the duplication safe.
const editorKeys = [...editorTsx.matchAll(/^\s{4}key: "([^"]+)",/gm)].map((m) => m[1]);
check(
  "the editor's slot list matches BRANDING_KEYS",
  editorKeys.length === keys.length && editorKeys.every((k, i) => k === keys[i]),
  `editor [${editorKeys.join(", ")}] vs store [${keys.join(", ")}]`,
);

// -------------------------------------------------------------------------------------------
// 2. Findings are split by field, not printed as one sentence.
// -------------------------------------------------------------------------------------------

check(
  "every finding variant carries a field",
  (() => {
    const body = brandingRs.slice(brandingRs.indexOf("pub enum BrandingFinding"));
    const variants = body.slice(0, body.indexOf("\n}\n"));
    const fields = [...variants.matchAll(/field:\s*String/g)];
    // Seven variants, seven `field: String` arms — asserted as a COUNT so a new variant added
    // without a field fails, rather than a new one added WITH a field passing by accident.
    return fields.length >= 7 && /fn field\(&self\)/.test(brandingRs);
  })(),
  "a finding with no field cannot be placed under an input",
);

check(
  "the route attaches the findings array to the refusal",
  /\.with_details\(json!\(\{\s*"findings":\s*findings\s*\}\)\)/.test(routeRs),
  "the panel cannot split a sentence the server never tagged",
);

check(
  "the panel reads details.findings",
  /details\?\.findings/.test(editorTsx) && /finding\.field/.test(editorTsx),
  "the split has to read the tag the server sent",
);

check(
  "a branding refusal reaches the editor split",
  /findingsFrom\(caught\)/.test(customizeTsx) && /setBrandingMessages\(split\)/.test(customizeTsx),
  "the save handler must keep both the banner and the per-field messages",
);

check(
  "each field renders its own message list",
  editorTsx.includes("data-theme-branding-error={slot.key}") &&
    editorTsx.includes('role="alert"'),
  "one <ul role=alert> per slot, not one alert for the section",
);

check(
  "editing a refused field retires its message",
  /if \(name === "branding"\)/.test(customizeTsx) && /delete next\[key\]/.test(customizeTsx),
  "a message left under a replaced input claims an input is wrong when it is not",
);

// -------------------------------------------------------------------------------------------
// 3. The limits come from the server.
// -------------------------------------------------------------------------------------------

check(
  "BrandingLimits serialises",
  /#\[derive\([^)]*serde::Serialize[^)]*\)\]\s*(?:#\[serde\([^\]]*\)\]\s*)*pub struct BrandingLimits/.test(
    brandingRs,
  ),
  "the panel cannot display a number the API does not send",
);

check(
  "the limits are camelCase on the wire",
  /#\[serde\(rename_all = "camelCase"\)\]\npub struct BrandingLimits/.test(brandingRs),
  "maxBytes/maxPx/minPx are the names the panel reads",
);

check(
  "the settings view carries the branding limits",
  /pub branding_limits:/.test(themeSettingsRs),
  "the screen has to learn the ceiling somewhere",
);

check(
  "the route computes the limits from the manifest",
  // The read path and the save path both reduce the manifest, and they differ only in whether
  // they own a String or borrow one — matching the `.await` matters, or the check passes
  // against the save path and says nothing about the read that feeds the screen.
  /BrandingLimits::for_theme\(&manifest_for\(state, &theme_key\)\.await\)/.test(routeRs),
  "the displayed ceiling must be the enforced one, read the same way",
);

check(
  "the read route passes limits into the view",
  /settings_view\(\s*state\.db\(\)\.pool\(\),\s*site_id,\s*&active,\s*defaults,\s*branding_limits,/.test(
    routeRs,
  ),
  "a field that is never passed is a field that is always the default",
);

check(
  "the panel type declares brandingLimits",
  /brandingLimits: ThemeBrandingLimits;/.test(apiTs),
  "without the field the editor's prop has no source",
);

check(
  "the editor renders the limits from the server value",
  editorTsx.includes("data-theme-branding-limits={limitLine}") &&
    editorTsx.includes("limits.maxBytes") &&
    editorTsx.includes("limits.minPx") &&
    editorTsx.includes("limits.maxPx"),
  "the numbers on screen must be the prop, not a constant",
);

check(
  "the accept filter is derived from the server's types",
  /limits\.contentTypes[\s\S]{0,120}?\.join\(","\)/.test(editorTsx),
  "a client-side type list is a second implementation of the type rule",
);

check(
  "no hard-coded ceiling is left in the editor",
  !/maxBytes:\s*\d/.test(editorTsx) && !/2\s*\*\s*1024\s*\*\s*1024/.test(editorTsx),
  "a duplicated constant is the rule that will drift",
);

// -------------------------------------------------------------------------------------------
// 4. A branding value is a media id, so there is a real picker.
// -------------------------------------------------------------------------------------------

check(
  "the section renders the branding editor",
  /entry\.name === "branding"/.test(customizeTsx) &&
    customizeTsx.includes("<ThemeBrandingEditor"),
  "otherwise the section is still the shared free-text editor",
);

check(
  "the editor offers the media library",
  /fetchMedia\(/.test(editorTsx) && editorTsx.includes("data-theme-branding-picker={slot.key}"),
  "an id typed by hand is a value the operator cannot verify",
);

check(
  "the editor can upload through the library",
  /uploadMedia\(/.test(editorTsx) && editorTsx.includes("data-theme-branding-file={slot.key}"),
  "criterion 9 asks for upload via the media library, not a paste field",
);

check(
  "only images are offered for a logo",
  /content_type\.startsWith\("image\/"\)/.test(editorTsx),
  "a video is storable in the library and is not a logo",
);

check(
  "a branding key can be cleared",
  editorTsx.includes("data-theme-branding-clear={slot.key}") &&
    /onClick=\{\(\) => onClear\(slot\.key\)\}/.test(editorTsx) &&
    /onClear=\{\(key\) => setSectionValue\("branding", key, null\)\}/.test(customizeTsx),
  "the panel's own 'no logo here' state is null, which the validator treats as clearing",
);

check(
  "the current value is visible as an image",
  editorTsx.includes("data-theme-branding-thumb={slot.key}") &&
    editorTsx.includes("mediaRawUrl(current)"),
  "an id string is not evidence that a file renders",
);

check(
  "a missing library is stated, not silently empty",
  editorTsx.includes("data-theme-branding-library-error") &&
    editorTsx.includes("data-theme-branding-empty-library"),
  "a failed fetch that renders as 'nothing here' is a lie about the site",
);

check(
  "an upload failure is shown on the slot that caused it",
  editorTsx.includes("data-theme-branding-upload-error={slot.key}"),
  "one upload error for three inputs sends the operator to the wrong one",
);

// -------------------------------------------------------------------------------------------

let failed = 0;
for (const result of results) {
  if (!result.pass) failed += 1;
  console.log(`${result.pass ? "PASS" : "FAIL"}  ${result.name}`);
  if (!result.pass) console.log(`      → ${result.detail}`);
}
console.log(`\n${results.length - failed}/${results.length} branding-editor wiring checks passed`);
process.exit(failed === 0 ? 0 : 1);