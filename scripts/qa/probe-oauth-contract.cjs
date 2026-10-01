#!/usr/bin/env node
/**
 * The panel's OAuth types against the API's OAuth response shapes (REQ-033, slice 3).
 *
 * ## The defect class
 *
 * `apps/admin/lib/types.ts` is hand-written. It is not generated from the Rust structs and
 * nothing checks it against them, so a field renamed in `developer_oauth.rs` leaves the
 * TypeScript type describing a shape the API no longer returns — and **`tsc --noEmit` stays
 * green**, because a stale type is still a well-formed type. The screen then reads
 * `app.redirect_uri_count`, gets `undefined`, and renders "None" for every row: a live screen
 * confidently reporting a count it never received.
 *
 * That is the same failure `probe-helper-contract.cjs` exists for on the other side of the wire,
 * and it has the same shape: the wrong answer reads as a plausible one, so nothing reports an
 * error.
 *
 * ## What this gate checks
 *
 * The Rust field names are read straight out of the two files that declare them, and every one
 * is required to appear in the matching TypeScript type. Rust → TS must be **complete**; a
 * field the API serialises and the type omits is a value the screen cannot read at all.
 * TS → Rust may carry extras, because the panel types deliberately document fields the API omits
 * on *some* shapes (`client_secret` is create/rotate-only), and demanding equality would demand
 * the two sides agree about which response is which — stated in prose on both sides, enforced
 * nowhere.
 *
 * It is a static read of three source files: milliseconds, no browser, no database, no slot, no
 * cargo. The drift is caught in the same second as the edit that causes it, rather than on a
 * 25-minute browser run.
 */

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..", "..");
const RUST_SOURCES = {
  // The response wrappers — `AppSummary`, `AppDetailResponse`, `AppsResponse`,
  // `MintedAppResponse`, and the request bodies.
  route: path.join(ROOT, "apps/api/src/routes/developer_oauth.rs"),
  // The domain types. `OAuthApp` and `AppStatus` are crate types, and a gate that only read the
  // route file would report MISSING for the single most important shape — an absent check that
  // reads exactly like a passing one, which is what the first version of this gate did.
  crate: path.join(ROOT, "crates/developer/src/model_oauth.rs"),
};
const TS_TYPES = path.join(ROOT, "apps/admin/lib/types.ts");
const TS_API = path.join(ROOT, "apps/admin/lib/api.ts");

const results = [];
const check = (name, pass, detail) => results.push({ name, pass, detail });

const rustSources = Object.fromEntries(
  Object.entries(RUST_SOURCES).map(([key, file]) => [key, fs.readFileSync(file, "utf8")]),
);
const tsTypes = fs.readFileSync(TS_TYPES, "utf8");
const tsApi = fs.readFileSync(TS_API, "utf8");

// ---------------------------------------------------------------- Rust side

/** The declared `pub` fields of a Rust struct, wherever it lives. */
function rustFields(structName) {
  for (const [where, text] of Object.entries(rustSources)) {
    const start = text.indexOf(`pub struct ${structName} {`);
    if (start === -1) continue;
    const end = text.indexOf("\n}\n", start);
    const body = text.slice(start, end === -1 ? undefined : end);
    const fields = [];
    for (const line of body.split("\n")) {
      // Attributes and doc comments are skipped by construction: the match is anchored on `pub`.
      const match = line.match(/^\s*pub\s+([a-z_][a-z0-9_]*)\s*:/);
      if (match) fields.push(match[1]);
    }
    return { fields, where };
  }
  return null;
}

// ---------------------------------------------------------------- TypeScript side

/**
 * The declared fields of a TypeScript object type.
 *
 * Both spellings are handled, because both are in this file: a plain `{ … }` body, and an
 * intersection such as `OAuthApp & { client_secret: string }`.
 *
 * The important part is that a **failed lookup is a failure, never a neighbour**. The first
 * version of this gate did `indexOf("export type X = {")`, and on an intersection that returned
 * -1 — so `slice(-1, …)` handed back the *previous* type's tail. It then reported `apps` as
 * missing from `OAuthAppsResponse` while quoting `OAuthAppDetailResponse`'s fields, and printed a
 * confident FAIL that had nothing to do with the file it named. A gate that can attribute its own
 * failure to an unrelated declaration is worse than no gate, because it teaches the reader to
 * trust the label instead of the evidence.
 */
function tsFields(typeName) {
  const marker = `export type ${typeName} =`;
  const start = tsTypes.indexOf(marker);
  if (start === -1) return null;

  const brace = tsTypes.indexOf("{", start);
  if (brace === -1) return null;

  // Only walk the braces, so a type whose header names another type (`= OAuthApp & {`) cannot
  // borrow the base type's fields, and a doc comment above cannot either.
  let depth = 0;
  let end = brace;
  for (let i = brace; i < tsTypes.length; i += 1) {
    const char = tsTypes[i];
    if (char === "{") depth += 1;
    else if (char === "}") {
      depth -= 1;
      if (depth === 0) {
        end = i;
        break;
      }
    }
  }

  const body = tsTypes.slice(brace, end);
  return { fields: fieldsIn(body), body, header: tsTypes.slice(start, brace) };
}

/**
 * The field names inside one object-type body.
 *
 * Two spellings occur in this file and both have to count, because reading only one of them is a
 * check that silently covers half the surface: a multi-line body indented by two spaces
 * (`\n  field: type`), and the single-line shorthand (`{ field: Type }`). The first version of
 * this function was line-anchored, so `export type OAuthAppsResponse = { apps: OAuthAppSummary[] };`
 * read as **zero** fields and the gate reported the list wrapper as omitting `apps` — a FAIL
 * naming a field that was plainly there. A gate that reports a confident failure about the wrong
 * thing trains its reader to ignore it.
 */
function fieldsIn(body) {
  const inner = body.replace(/^[^{]*\{/, "").replace(/\}[^}]*$/, "");
  const multiline = [...inner.matchAll(/^\s{2}([a-z_][a-zA-Z0-9_]*)\??\s*:/gm)].map((m) => m[1]);
  if (multiline.length > 0) return multiline;
  // Single line: `field: Type, other?: Type`. A doc comment inside the braces is stripped first
  // so a comment naming a field is not mistaken for one.
  const withoutComments = inner.replace(/\/\*[\s\S]*?\*\//g, "");
  return [...withoutComments.matchAll(/(?:^|[{,])\s*([a-z_][a-zA-Z0-9_]*)\??\s*:/g)].map((m) => m[1]);
}

// ---------------------------------------------------------------- the four shapes

const PAIRS = [
  { rust: "AppSummary", ts: "OAuthAppSummary", label: "the list row" },
  { rust: "OAuthApp", ts: "OAuthApp", label: "the full row" },
  { rust: "AppDetailResponse", ts: "OAuthAppDetailResponse", label: "the detail read" },
  { rust: "AppsResponse", ts: "OAuthAppsResponse", label: "the list wrapper" },
];

for (const pair of PAIRS) {
  const r = rustFields(pair.rust);
  const s = tsFields(pair.ts);
  check(
    `${pair.label}: both shapes are readable`,
    r !== null && s !== null,
    `rust ${r ? `${r.fields.length} in ${r.where}.rs` : "MISSING"}, ts ${
      s ? s.fields.length : "MISSING"
    }`,
  );
  if (!r || !s) continue;

  for (const field of r.fields) {
    // `#[serde(flatten)]` is the one legitimate Rust→TS asymmetry: `MintedAppResponse` flattens
    // `OAuthApp`, so its own fields are the *extra* ones. The flattened base is checked by its
    // own pair above, which is why this list is compared against the base's names too.
    check(
      `${pair.label}: \`${field}\` is in the TypeScript type`,
      s.fields.includes(field),
      s.fields.includes(field)
        ? "present"
        : "the API serialises it and the panel type omits it — the screen reads `undefined` and `tsc` stays green",
    );
  }
}

// ---------------------------------------------------------------- the credential rule

/**
 * A credential must not be readable from a *read* shape.
 *
 * The Rust side holds this in `a_list_row_serialises_the_count_and_never_the_uris_or_a_secret`;
 * this is the same rule stated where a *screen* would be written.
 *
 * **`previous_secret_expires_at` is deliberately allowed**, and the exclusion is the whole
 * subtlety. It matches `/secret/i` and is not a credential: it is the deadline after which the
 * *previous* secret dies, and it is the one field on a row that can be a to-do rather than a
 * fact — an operator who has rotated and not finished redeploying is looking at exactly it. The
 * Rust test says the same thing in a comment: "Not `!rendered.contains("secret")`: the summary
 * legitimately carries `previous_secret_expires_at`, which is a deadline". A substring check on
 * `secret` would have to be weakened to let it through, and the weakened version would stop
 * noticing a real credential. So the rule is on field *names*, with that one named exception.
 */
const DEADLINE_FIELD = "previous_secret_expires_at";
const READ_SHAPES = ["OAuthAppSummary", "OAuthApp", "OAuthAppDetailResponse", "OAuthAppsResponse"];
for (const name of READ_SHAPES) {
  const found = tsFields(name);
  if (!found) continue;
  const credentials = found.fields.filter(
    (field) => /secret|hash/i.test(field) && field !== DEADLINE_FIELD,
  );
  check(
    `${name} exposes no credential field`,
    credentials.length === 0,
    credentials.length === 0
      ? `clean (${DEADLINE_FIELD} is a deadline, not a credential)`
      : `${credentials.join(", ")} — a read shape must never carry one`,
  );
}

// The deadline itself must be on the row, or the screen has no way to answer "is every
// deployment on the new secret yet?".
for (const name of ["OAuthAppSummary", "OAuthApp"]) {
  const found = tsFields(name);
  if (!found) continue;
  check(
    `${name} carries ${DEADLINE_FIELD}`,
    found.fields.includes(DEADLINE_FIELD),
    found.fields.includes(DEADLINE_FIELD) ? "present" : "the overlap is invisible on the row",
  );
}

// ---------------------------------------------------------------- the minted shape

const minted = tsFields("MintedOAuthApp");
check("MintedOAuthApp is readable", minted !== null, minted === null ? "MISSING" : `${minted.fields.length} added fields`);
if (minted) {
  check("MintedOAuthApp adds `client_secret`", minted.fields.includes("client_secret"), "the dialog's signal");
  check(
    "MintedOAuthApp extends OAuthApp rather than copying it",
    /=\s*OAuthApp\s*&/.test(minted.header),
    "a copy would drift from OAuthApp silently — which is the defect this gate exists for",
  );
}

// There is no endpoint that can return a secret again, so a read helper would always be empty.
check(
  "no api.ts function reads a stored secret",
  !/client_secret\s*[:=]\s*(await\s+)?request/.test(tsApi),
  "the API has no such endpoint; a read helper would be a dead control",
);

// ---------------------------------------------------------------- the request shapes

/**
 * The panel's create body against the API's `CreateAppInput`.
 *
 * Presence only, not type or order: a Rust `Vec<String>` and a TypeScript `string[]` are one
 * contract, and a gate comparing the two spellings would be asserting a language mapping rather
 * than a drift.
 */
const createInputStart = rustSources.route.indexOf("pub struct CreateAppInput {");
const createInputEnd = rustSources.route.indexOf("pub struct EditAppInput {");
const createBody = rustSources.route.slice(createInputStart, createInputEnd);
const panelCreate = tsApi.slice(tsApi.indexOf("export type CreateOAuthAppInput"));

for (const field of [
  "name",
  "description",
  "logo_object_key",
  "redirect_uris",
  "scopes",
  "grant_types",
]) {
  const inRust = new RegExp(`pub\\s+${field}\\s*:`).test(createBody);
  check(`CreateAppInput declares \`${field}\``, inRust, inRust ? "present" : "absent on the API body");
  check(
    `CreateOAuthAppInput offers \`${field}\``,
    new RegExp(`^\\s{2}${field}\\??\\s*:`, "m").test(panelCreate),
    inRust ? "present on the panel body" : "n/a",
  );
}

// The `PATCH` body is the panel's edit body, and its `Option<Option<String>>` fields are the
// reason "absent" and "cleared" are different — so the panel must be able to send `null`.
for (const field of ["description", "logo_object_key"]) {
  const inRust = new RegExp(`pub\\s+${field}\\s*:`).test(
    rustSources.route.slice(
      rustSources.route.indexOf("pub struct EditAppInput {"),
      rustSources.route.indexOf("pub struct AppsResponse {"),
    ),
  );
  check(`EditAppInput declares \`${field}\``, inRust, inRust ? "present" : "absent");
  check(
    `editOAuthApp's body types \`${field}\` as clearable`,
    new RegExp(`${field}\\?:\\s*string\\s*\\|\\s*null`).test(tsApi),
    inRust ? "string | null — `null` clears, absent leaves it alone" : "n/a",
  );
}

// ---------------------------------------------------------------- report

const failed = results.filter((r) => !r.pass);
for (const r of results) {
  const mark = r.pass ? "PASS" : "  FAIL";
  const detail = typeof r.detail === "string" ? r.detail : JSON.stringify(r.detail);
  console.log(`${mark}  ${r.name}${detail ? ` — ${detail}` : ""}`);
}
console.log(`\n${results.length - failed.length}/${results.length} oauth-contract checks passed`);
process.exit(failed.length === 0 ? 0 : 1);