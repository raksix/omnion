# REQ-033 — Internal Developer Platform

> **Status:** in-progress (tick 116 — **the merge that had been queued since tick 114 is now landed, and it was not a clean apply: main had independently built the same crate, the same `/developer` section root and the same request-log table.** `crates/developer` collided add/add for the third time in three ticks, and this one is worth writing down because *both sides were this REQ's own subject matter*. What was resolved, and why:

**(1) The crate keeps this branch's implementation and main's `overview.rs`.** `keys.rs`/`keys_store.rs` and `logs_store.rs` did not come across: they are a *second reader for one credential* against `api_keys`, and `guards.rs` already argues at length for why the twin without the IP allowlist is a defect — which of the two ran would depend on which route the caller hit. `overview.rs` came across whole, behind the `store` feature, because it is the only reader that counts keys, requests and refusals in **one** statement, and the walk that arrived with it (`the_overview_counts_the_same_keys_and_requests_the_tables_show`) is the thing that will catch it drifting from the screens. `logs.rs` came across as **two functions** in a new `log_vocab.rs` rather than as main's 415-line file: `class_of` and `path_without_query` are the log screen's *rules*, they have no dependencies, and the rest of that file needs an `hmac` dependency and a `DeveloperError::Invalid` variant this branch's per-rule `code`/`field` error enum deliberately replaced. Six unit tests came with the two functions, including the one that says a class the toolbar offers must be a class a row can carry.

**(2) `/developer` renders this branch's screen, not main's.** Both are overview screens; the difference is the claim. Main's reads one snapshot endpoint; this branch's six cards each read the list **their own destination** already lists, so a card cannot disagree with the screen it opens. Main's endpoint is kept for callers that need a consistent moment. A `layout.tsx` provider nest and both `useDeveloperAccess` and `useTenantStatus` gates were grafted together rather than one chosen — they are independent filters, and the sidebar's `NAV` now applies both.

**(3) The request-log middleware is on the request path now, and it needed three columns migration `0240` had explicitly refused to add.** `0243` adds `actor_name`, `api_key_prefix` and `permission`; `client_fingerprint` is deliberately **not** added (a nullable privacy field nothing writes is a privacy field with no policy attached — it is computed in `store::ClientIdentity` and not persisted). The middleware now calls **this branch's** `log_request` rather than main's `logs_store::record`: two writers against one table with different column sets makes "is this row complete?" a question with two answers, and only one of them is covered by a walk. Two `not null` columns the middleware does not naturally carry are answered honestly rather than skipped — an anonymous call gets the platform's own id and a fresh request id, because a row that dropped itself for lacking an organization would lose exactly the public-route refusals an operator most wants. `DeveloperError::Misconfigured` is the new variant, and it is deliberately **not** a client error: a missing `OMNION_LOG_PEPPER` is a `5xx` naming a variable, not a `400` sending the caller to fix a request that was never wrong.

**PROOF.** `cargo check -p omnion-api --tests -p omnion-developer --features store` **0 errors** (it was 11 across six files when the merge started). `pnpm typecheck` **2/2**. Migration `0243` applied to the live `omnion_qa_w5` and re-applied: `ALTER TABLE` + 3 `COMMENT` + `CREATE INDEX` first time, `NOTICE: … already exists, skipping` the second — the columns and `api_request_logs_permission_refusals_idx` are present by name. BUILD-LOG merged and verified by heading multiset (180 headings, 0 short) after grepping for markers, because the tool's own gate reports clean on a file with an orphan `<<<<<<<`.

**STILL OWED: the browser pass.** The QA slot has been held by a live w8 and then a live main pass on every attempt this tick, and the box measured load 471 with 0 free RAM at the start, so a pass launched then would have measured the box rather than this merge. Nothing in this entry counts as a pass. Slices 2, 3 and 4 remain open on it. · **Captured:** 2026-09-25 · **Layer:** core (`crates/developer`) + admin UI
>
> **The same dead-button class a third time, and the shape of it is now worth writing down.** `DeviceStart.verification_uri` is `/developer/sdks?tab=cli`; the CLI prints it and the person opens it. The tab strip was **React state only**, so that link landed on the plugin generator — a working page, so no route walk, no build and no `tsc` noticed. Two details matter more than the fix. First, the tab is now read in the **`useState` initialiser**, not only in the effect that resynchronises state with the URL: a view that corrects itself one frame later has already painted the wrong screen to the person following a terminal's own instructions. Second, the reading is a **named pure function** (`initialTab`) rather than an inline expression, because the first version of the probe asked "does the view read `?tab=`?" — which is true of the dead version too, since the resync effect reads it. A claim too general to fail is the tick-98 gate wearing a different hat.
>
> **A `useSearchParams` deep link is a BUILD-time obligation, and the build is the gate.** Three routes now render one (`/webhooks/new`, `/webhooks/[id]/edit`, `/developer/sdks`) and each needs a `Suspense` boundary or `next build` fails at deploy time rather than in the editor. `tsc` is green in all three states, which is exactly why `pnpm --filter @omnion/admin build` runs on this tick.
>
> **The download is asserted on its bytes, not its DOM.** `theDownloadServesAZip` fetches the row's link and checks the first four bytes (`PK\x03\x04`), the `Content-Disposition`, the content type **and** a minimum size — a zero-byte archive passes a signature check, which is the failure a count-only assertion cannot see.
>
> **Two probes, because a text check cannot see behaviour.** `probe-dev-sdk-screen.cjs` (**41/41**, proven to fail 5/41 across five mutations — initial state hardcoded, a hook renamed, `text-danger` reinstated, the `Suspense` removed, the server's URI moved; all reverted and all four files verified byte-identical) checks hooks, tokens and *both halves* of the deep link. Its mutation run then found what it could not catch: replacing `initialTab`'s body with `return (value as Tab) || "plugin"` leaves every text check green while returning `"wat"` for an unknown tab, and `"wat"` flows straight into `ScaffoldTab kind={tab}` and on into a POST body. So `probe-dev-sdk-tab-fallback.cjs` (**11/11**) lifts the function's own source and *asks* it; it fails 5 ways on that mutation and 6 ways on a widened allowlist. It strips the TS casts on purpose — without that the mutation fails by refusing to parse, which would report "the function is broken" instead of the real claim, that the guard is gone.
>
> **A red test in the tick that committed it, and the shape of my mistake.** `cargo test` failed `a_regenerated_archive_is_byte_identical_...`: it looked up `plugin-determinism/omnion.manifest.json`, and the archive stores entries under the template's own paths with no enclosing folder. The archive was fine. "Look it up and see if it comes back" cannot tell a missing entry from a wrong guess, so a hardcoded path in a round-trip test is a **second, invisible claim about the format**; the path now comes from the scaffold being archived. The format statement lives in one test, in both directions — no path starts with the slug, *and* a slug-prefixed guess does not resolve.
>
> Gates: `omnion-api --lib` **414** (was 412), `omnion-developer --features store --lib` **196** (was 193), `developer_scaffolds` **6/6**, `store_cli` **3/3** (proven to fail 1/3 on each of two mutations), `probe-dev-sdk-screen.cjs` **41/41** (proven to fail 5/41), `probe-dev-sdk-tab-fallback.cjs` **11/11** (proven to fail 5 and 6 ways), `probe-dev-event-screen.cjs` **22/22** (unchanged, so the sibling screen was not broken), `pnpm typecheck` **2/2**, `pnpm --filter @omnion/admin build` compiles with `/developer/sdks` present in `routes-manifest.json`, `node --check` clean.
>
> **Slice 4's remaining item is the browser pass**, and it is queued: `scripts/qa/run.sh` on `QA_STACK=w5` was launched this tick with the slot free (the `w4-target` reclaim below freed 1.3G of a tmpfs that was at 95%). **Open on that pass alone**, like slices 2 and 3.

> **The browser pass ran, and it found a live defect that nothing else could.** The focused
> `QA_STACK=w5` pass (`QA_ONLY=dev-sdks,…`) reported **4 claims that did not hold**, and three of
> them were one bug: `start` minted `SPW5-SXDH`, the screen printed it, the pass typed it back, and
> the API answered `invalid device code`. `draw_user_code` returns the **grouped display form** and
> the insert bound it verbatim, while every reader — `find_for_approval`, `approve`, the poll —
> normalises its input first. The row the failing pass left in `omnion_qa_w5` reads
> `SPW5-SXDH | len=9 | canonical=false` and the reader's lookup finds `0`; rewritten the way the
> fixed insert writes it, `1`. `stored_user_code_form()` is now the single place the canonical form
> is chosen (`792d610f`).
>
> **Why 196 unit tests and a green SQL probe missed it.** `probe-cli-store.sql`'s four fixtures were
> hand-written as `'BCDF-1111'`, `'GHJK-2222'`, `'LMNP-3333'`, `'QRST-4444'` — the grouped form, which
> the store never wrote. A hand-written fixture is a second implementation; it agreed with the wrong
> half. The unit tests exercised `normalize_user_code` rather than the `.bind(...)` that chose what to
> store. Both corrected, and **two of the three new tests had to be rewritten before they could see
> the bug**: one asserted a *function's* output instead of the statement that uses it, and one read
> its own source with `include_str!` and sliced from the insert to end-of-file — a region that
> includes the test module containing the literal it greps for, so it passed against the very defect
> it exists to catch. **A check that reads itself always agrees with itself.** The check now reads
> the file at runtime via `CARGO_MANIFEST_DIR` and cuts at `#[cfg(test)]`. Both mutations now fail 1/3
> each.
>
> **The fourth claim was mine, and the code was right.**
> `thePreviewHidesTheDotfilesItAdmitsToHiding` asserted that `.env.example` and `.gitignore` were both
> kept out of the preview. `ScaffoldFile::shown_in_preview` hides **only** `.gitignore`, on purpose:
> a preview full of dotfiles is noise, while `.env.example` is the file the generated README warns
> about, so hiding it would hide the warning's target. The claim was rewritten from the rule's own
> doc and is now stronger than the original (`c5dd43fb`). The screen's **note** was wrong too and
> now says which file is where. The general lesson: read the code that owns the rule before writing a
> claim about the rule — it was written down, in that field's own doc comment, and I wrote the claim
> from the *shape* of the thing rather than from the sentence.
>
> **The re-run is green: 27/27 claims, 0 failed passes.** The device-code flow now completes in
> the browser — request a code, look it up, read "omnion-cli wants access to this organization"
> with the plain-language scopes, approve it, and the same code cannot be approved twice. The
> download serves a real zip (`theDownloadServesAZip`, `theDownloadNamesTheFile`,
> `theDownloadIsTypedAsAZip`, `theDownloadHasContent`), the validator refuses a broken manifest and
> accepts a good one, and `previewRecordedNothing` holds.
>
> **The first re-run reported the SAME `invalid device code` after the fix — and the cause was the
> harness, not the product.** `run.sh` rebuilt the API when the binary was missing or a
> `database/migrations/*.sql` was newer, and did **not** watch the Rust sources, so a committed fix
> left a 30-minute-old binary under test (`fd4821a5`). The symptom is the argument: `cargo test`
> passed on the new code and the browser pass failed on it, and the two disagreed because they were
> not testing the same thing. The guard now watches `crates/` and `apps/api`; a migration is a
> special case of "the source is newer than the binary", not a separate concern. **A pass that can
> report a verdict about code it did not run is worse than no pass, because the verdict is
> trusted.**
>
> **One medium finding was mine and it was real: `unlabeled-input` on `/developer/sdks`.** The
> manifest textarea had a heading above it and a JSON placeholder in it; a screen reader announces
> "edit text, blank", and a placeholder disappears the moment the field has content — exactly when a
> person reads the screen to check what they typed. Now a real `<label htmlFor>` with the field's
> `id` and an `aria-describedby` pointing at the hint (`ef1b2546`). The gate for it took four tries,
> each failing differently, and the failures are the argument for the fifth version: an aggregate
> count was satisfied by `aria-label` on the *tablist*; `[^>]*` ended a JSX tag at an arrow
> function's `>`; "some label body contains an `<input`" survived unwrapping the first of two
> same-tag inputs. **A gate that cannot tell correct code from broken code is worse than no gate.**
> Proven to fail 1/3, 1/3, 1/3 and 2/3 on four mutations, all reverted, view byte-identical.
>
> **Pass counters for the record** (scoped run, so NOT a whole-repository result): 27/27 claims,
> 0 failed passes, 84 screenshots. The run's 4 high findings are all in other waves — a 422 from
> `/api/v1/pages` and two `web-page` items about the renderer sample page, both the main writer's
> CMS and renderer surfaces.
>
> Tick 111's note stands for the record: slice 3's code is complete — the panel screen tick 110 said was open (`6fa5d647`, corrected at `f26b96d8` with the pass's 33 `data-oauth-app-*` hooks cross-checked against the view's 33), and the event catalogue that was the slice's last item (`6e994015`, `318321cd`).
>
> **The deep link was the feature, and it was a dead button.** The Subscribe button links to `/webhooks/new?event=<name>`; `EndpointForm` read **no query parameter at all**. The link landed on a working page and did nothing — invisible to a route walk, because the route is fine, and invisible to every compile-time gate. The form now reads `?event=`, applies it **once**, and **only when creating**: a deep link must not silently add a subscription to an endpoint that is already live and already delivering. It applies *after* the catalogue arrives, because the subscribable set is the live names only — a reserved name in the URL is simply not found, which is the correct outcome rather than a failure to handle.
>
> **`useSearchParams` costs a Suspense boundary on every route that renders the form**, not only the one the link points at: `/webhooks/new` and `/webhooks/[id]/edit` were both outside one. `tsc` is green either way — the failure is at build time, which is why `next build` is a gate on this tick and not a nicety.
>
> **A defect in my own new code, caught before it shipped:** the error banner used `text-danger`, and this palette has **no `--color-danger`** — it is `caution`. A class defined nowhere is still a well-formed Tailwind class and renders the message in the *inherited* colour, so the banner would not have looked like a banner. The main writer's media probe found this same class in two files (BUILD-LOG, tick 101); `scripts/qa/probe-dev-event-screen.cjs` now checks a developer surface's hooks against the pass **and** its colour tokens against `globals.css`.
>
> **The pass is able to fail, which several older ones are not.** `runDepthPass` only reports failure on a *thrown* error and `run.sh` reads only `summary.json`, so the tempting shape — append booleans to a `steps` object and return it — yields fourteen numbers nothing reads. Each of the fourteen claims is `check(name, value)` and the pass throws naming the ones that did not hold. `theDeepLinkPreselectsTheEvent` follows the link and checks the ticked box carries the event's **name**, not merely that some box is ticked. Two claims are inverses on purpose: a scope filter that did nothing would satisfy the default-scope claim on its own.
>
> Gates: `omnion-developer --features store --lib` **128**, `omnion-media --lib` **248**, `probe-oauth-contract.cjs` **56/56**, `probe-dev-event-screen.cjs` **22/22** (proven to fail 2/22 both ways, mutations reverted and the view verified byte-identical), `pnpm typecheck` **2/2**, `next build` compiles with the route present, `node --check` clean.
>
> **Slice 3's code is complete; what remains for it is the browser pass**, and that pass is not owed on this tick: the global QA slot is held by a live `w3` run (holder pid 2914377, cwd `/mnt/apopic/omnion-w3`), and `/mnt/apopic` is at 100% with 501 MB free. Per this branch's rule the work is committed and queued rather than reported as a pass that never ran.
>
> Tick 109's note stands unchanged for the record: slice 3c, the sessionless half, is shipped — the authorization request, the consent screen, the token endpoint and token introspection, in `crates/developer/src/oauth_flow.rs` and `apps/api/src/routes/oauth_flow.rs`, plus migration `0232` for the access tokens. **A live XSS in my own code, found by the consent screen's escaping test**: the three hidden form fields were JSON-encoded, and JSON escapes a quote as `\\"` where HTML wants `&quot;`, so a `state` of `"><script>alert(1)</script>` closed the attribute and left a live script tag in the approver's session on the platform's own origin. The same function also rendered an absent value as the four characters `null`, which the token endpoint would have echoed back to the client as a `state` it never sent. The compiler caught a third one: `redirect_with_error` takes `&'static str`, and I had written `error.to_string()` — a client-supplied `response_type` was about to ride in a `Location` header into browser history and `Referer`. Gates: `omnion-api --lib` **408** (was 397), `omnion-developer --features store --lib` **128** (was 104), `tsc --noEmit` clean, and migration `0232` proved against live PostgreSQL by **6 named refusals across 5 constraints** plus 4 positive controls.)
> **Three things tick 109 found that the code did not know about itself.** (The previous tick's
> findings — the `[::1].attacker.example` open redirect, the `0229` migration collision, and the
> two-fields-one-JSON-key response shape — are in `docs/BUILD-LOG.md` and in the slice 3b notes
> below.)
>
> **(1) A live stored XSS in the consent screen, reachable by any developer who can register an
> app in their own tenant.** The three hidden form fields were **JSON-encoded**, and JSON escapes
> a double quote as `\"` where HTML wants `&quot;`. A `state` of `"><script>alert(1)</script>`
> therefore rendered as
>
>     <input ... name="state" value="\"><script>alert(1)</script>">
>
> with the attribute closed early and the script tag live in the **approver's** session on the
> platform's own origin. The attacker is the app's tenant; the victim is the person signing in.
> The general "nothing leaked" assertion is what found it, but it could not say *which* mistake
> had been made, so the regression test is a standalone one that demonstrates the JSON form
> **does** break out of the attribute — the difference between the two escapers is the bug.
>
> **(2) An absent `state` rendered as the four characters `null`.** A hidden input whose value is
> `null` posts the *string* `"null"`, which `usable_state` accepts, and the token endpoint then
> echoes it back to the client as a `state` it never sent. The platform was inventing a value on
> the client's behalf. Absence now renders as an empty attribute.
>
> **(3) The borrow checker caught a third, before a test could.** `redirect_with_error` takes
> `&'static str` for the error code and the description — that signature is the rule, and I had
> written `&error.to_string()`. `UnknownGrantType` carries the caller's own `response_type` in its
> message, so a client-supplied value was about to ride in a `Location` header into browser
> history, proxy logs and the next `Referer`. The fixed `access_denied` goes to the browser; the
> real reason goes to a `tracing::debug`.
>
> **Slice 2 remains code-complete and waiting only on a browser pass.**
>
> **A fourth thing from tick 108, found by the response-shape tests:** `MintedAppResponse` declared
> `previous_secret_expires_at` while the flattened `OAuthApp` already carries a field of that
> name. Two fields, one JSON key, and the flatter writes first — so on a *creation* the response
> said `null` while also claiming to have no such field, and a client reading the outer value
> could never learn a rotation's deadline. The duplicate is gone and a `debug_assert` states that
> the minted app and its row must agree about the deadline.
>
> **Gates:** `cargo test -p omnion-developer --features store --lib` **104** (was 73, +31),
> `cargo test -p omnion-api --lib` **397** (was 391), `cargo test -p omnion-permissions --lib`
> **64**, the OpenAPI drift gate **5/5** with all four new routes documented, and migration
> `0231` proven against live PostgreSQL by **8 named refusals across 7 constraints** —
> `oauth_apps_deletion_is_whole` in *both* directions, plus `oauth_apps_overlap_is_whole`,
> `oauth_apps_grant_types_are_known`, `oauth_apps_redirect_uris_known`,
> `oauth_codes_challenge_is_whole`, `oauth_codes_expiry_is_future` and
> `oauth_apps_org_name_key` — with **3 positive controls**, including the one that matters: a
> *withdrawn* app frees its name, so the partial unique index is doing its job. · **Captured:**
> 2026-09-25 · **Layer:** `apps/admin` + SDKs · **Source:** owner brief — platform periphery &
> headline features (2026-09-25)
> brief — platform periphery & headline features (2026-09-25)

## Request

Inside Omnion:

```text
Developer
├── API Explorer
├── API Keys
├── OAuth Apps
├── Webhooks
├── Events
├── Logs
├── Plugin SDK
├── Theme SDK
├── Workflow SDK
└── CLI
```

A developer can extend the system without leaving it.

## Notes

- Extends the Developer Portal (REQ-022).

## Implementation spec

### Scope (in / out)

In:

- A Developer section in the admin covering all ten surfaces: API Explorer, API Keys, OAuth Apps, Webhooks, Events, Logs, Plugin SDK, Theme SDK, Workflow SDK, CLI.
- API key lifecycle: create with scopes and environment (`live` / `sandbox`), rotate, revoke, expiry, last-used, per-key usage history and rate tier.
- OAuth apps: client registration (redirect URIs, scopes, branding), authorization-code plus PKCE for organization-internal clients, secret rotation with an overlap window, app status.
- API Explorer driven by the OpenAPI document the platform serves itself: browse operations, fill parameters and body from a schema-generated form, send against the selected environment, inspect status/latency/body, and copy the call as curl, TypeScript or Python.
- Events catalog: every published event type with description, JSON Schema payload, and a validating sample, with a deep link that prefills a webhook subscription.
- Request logs: filterable API request history with per-request detail (sizes and metadata only, no bodies) and CSV export.
- SDK scaffolds: generate a plugin, theme or workflow starter from a template, preview the file tree, validate a manifest, and download the archive.
- CLI: install instructions and a device-code login that mints a scoped CLI token.

Out:

- Marketplace publishing, review and payouts (REQ-023 / REQ-048).
- API usage metering and billing.
- Hosting third-party (non-organization) OAuth clients and public consent screens.
- A general-purpose HTTP proxy: the Explorer calls only this platform's own API, with the caller's own credentials.
- Plugin execution and sandboxing internals — this REQ ships tooling, not the runtime model.

### Screens (UI)

Routes (`apps/admin/app/developer/*`, feature dir `apps/admin/features/developer/`):

```text
/developer                     ← overview
/developer/api-explorer
/developer/keys
/developer/keys/{id}
/developer/oauth-apps
/developer/oauth-apps/{id}
/developer/webhooks            ← REQ-016 surface, framed here
/developer/webhooks/{id}
/developer/events
/developer/logs
/developer/sdks                ← tabs: Plugin | Theme | Workflow | CLI
```

- Shared layout: left sub-nav with the ten entries, an environment badge (`Live` / `Sandbox`) pinned in the header, and a quickstart card on the overview with three copy-ready snippets.
- `/developer` overview cards: Active keys, OAuth apps, Webhook endpoints (with 24h failure rate), Requests 24h (with error rate), Recent events — each linking to its surface. Quickstart tabs (curl / TypeScript / Python) use a placeholder token, never a real one.
- Keys table columns: Name, Prefix, Environment, Scopes (chips with `+N` overflow), Created, Last used, Expires, Status, and a row menu (Rotate, Revoke, View logs). Filters: environment, status, scope, name. Bulk: revoke selected (typed confirmation above five) and copy prefixes.
- Key create form: Name (required, 3–60 chars), Environment (required radio), Scopes (grouped multi-select, at least one, with a `select all read` shortcut), Expiry (never / 30 / 90 / 365 days, default 90), IP allowlist (optional CIDR list, validated), Rate tier (standard / high; `high` requires an owner or admin role). Submission opens a one-time secret dialog with a copy button, a not-shown-again warning, and a `Generate another key` action.
- Key detail: daily usage chart (requests/errors), top paths table, and the latest 20 requests with a link into Logs pre-filtered by that key.
- OAuth apps table: Name, Client ID (copyable), Redirect URIs (count), Scopes, Status, Created. Create/edit form: Name, Description, Logo (png/svg ≤256 KB), Redirect URIs (one per line; `https` required except `http://localhost`), Allowed scopes, Grant types (authorization code + PKCE, optional client credentials). Secret rotation shows the new secret once and explains the overlap window.
- Events catalog: left list of event names grouped by domain with search; right pane shows description, collapsible JSON Schema tree, a validating sample payload, and `Subscribe a webhook`, which deep-links to the webhook form with the event preselected.
- Logs table columns: Time, Method, Path, Status, Duration, Key, Actor, Request ID. Filters: key, status class, path prefix, method, date range (default 24h), duration threshold. A row opens a drawer with the request summary (sizes, timing, region, request id) and a copy-as-curl action; the filtered view exports to CSV through the REQ-031 export machinery.
- Log drawer must state plainly that bodies are not stored, so the absence of payload data is understood rather than suspected.
- SDK tab: template picker (Plugin — TypeScript, Theme — TypeScript, Workflow — DSL project), slug-validated Name, Target (Live / Sandbox), and a file-tree preview of the archive before download. A `Validate manifest` drop zone reports schema errors inline with line numbers.
- CLI tab: per-platform install snippet, `omnion login` device-code flow with a code, the approval URL and an expiry countdown, plus a plain-language list of the scopes the issued token will carry. Existing tokens are never rendered.
- Empty states: keys — "Henüz API anahtarı yok" with the create CTA; logs — "Bu aralıkta istek yok" with a widen-range action; events — suggestion chips; SDK — template cards only. Loading: skeleton tables, and a cancellable sending state in the Explorer. Errors: field-level inline messages; a failed Explorer call renders as a normal result (status, body, latency), not a page error; a `403` names the missing permission, never the caller's roles.
- Keyboard: `Ctrl+K` reaches every surface (REQ-032); `e` or `Cmd+Enter` sends in the Explorer, `Cmd+/` toggles the snippet drawer, `g k` keys, `g l` logs, `g e` events, `?` shortcut sheet.
- Mobile: sub-nav becomes a select, tables become cards, the Explorer stacks (request then response) with a sticky `Send`, copy buttons are ≥44px targets, and long snippets scroll horizontally instead of wrapping mid-token.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/dev/openapi.json` | OpenAPI document for the caller's surface | `developer.read` |
| POST | `/api/v1/dev/explorer/requests` | Run one API call as the caller (no key material involved) | `developer.explorer.run` |
| GET | `/api/v1/api-keys` | List keys (metadata only) | `developer.keys.read` |
| POST | `/api/v1/api-keys` | Create a key; returns the secret exactly once | `developer.keys.manage` |
| POST | `/api/v1/api-keys/{id}/rotate` | Rotate; returns the new secret once | `developer.keys.manage` |
| DELETE | `/api/v1/api-keys/{id}` | Revoke | `developer.keys.manage` |
| GET | `/api/v1/api-keys/{id}/usage` | Daily request/error series | `developer.keys.read` |
| GET | `/api/v1/oauth-apps` | List apps | `developer.oauth.read` |
| POST | `/api/v1/oauth-apps` | Register an app; returns the client secret once | `developer.oauth.manage` |
| PATCH | `/api/v1/oauth-apps/{id}` | Edit metadata, redirect URIs, scopes | `developer.oauth.manage` |
| POST | `/api/v1/oauth-apps/{id}/secret/rotate` | Rotate the client secret with an overlap window | `developer.oauth.manage` |
| DELETE | `/api/v1/oauth-apps/{id}` | Delete an app and revoke its tokens | `developer.oauth.manage` |
| GET | `/api/v1/events/catalog` | Event types with schema and sample | `developer.events.read` |
| GET | `/api/v1/request-logs` | Paged request log with filters | `developer.logs.read` |
| GET | `/api/v1/request-logs/{id}` | Single request metadata (no bodies) | `developer.logs.read` |
| POST | `/api/v1/dev/sdks/scaffold` | Generate a starter archive | `developer.sdks.scaffold` |
| POST | `/api/v1/dev/manifests/validate` | Validate a plugin/theme/workflow manifest | `developer.sdks.scaffold` |
| POST | `/api/v1/dev/cli/device-code` | Start the CLI login device-code flow | `developer.read` |
| POST | `/api/v1/dev/cli/device-code/approve` | Approve a device code from the browser session | `developer.keys.manage` |

Webhook endpoints, deliveries and replay stay on the REQ-016 surface (`/api/v1/webhooks`, `/api/v1/webhooks/{id}/deliveries`, `/api/v1/webhooks/deliveries/{id}/replay`) and are framed under `/developer/webhooks` rather than duplicated.

### Data model

Migration `database/migrations/0013_developer_platform.sql`.

`api_keys`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `uuid pk` | |
| `organization_id` | `uuid not null` | fk `organizations` |
| `name` | `text not null` | unique per organization |
| `prefix` | `text not null` | public identifier, unique |
| `secret_hash` | `text not null` | one-way hash; plaintext never persists |
| `scopes` | `jsonb not null` | array of permission keys |
| `environment` | `text not null` | check in (`live`,`sandbox`) |
| `rate_tier` | `text not null default 'standard'` | check in (`standard`,`high`) |
| `ip_allowlist` | `jsonb` | CIDR array; null means any |
| `expires_at` | `timestamptz` | null means no expiry |
| `last_used_at` | `timestamptz` | |
| `revoked_at` | `timestamptz` | |
| `created_by` | `uuid not null` | fk `users` |
| `created_at` | `timestamptz not null default now()` | |

Indexes: unique `(organization_id, name)`, unique `(prefix)`, `(organization_id, revoked_at)`.

`api_key_usage_daily`: `api_key_id uuid not null` fk `api_keys`, `day date`, `requests integer not null default 0`, `errors integer not null default 0`, `p95_ms integer`, primary key `(api_key_id, day)`.

`oauth_apps`: `id uuid pk`, `organization_id uuid not null`, `name text not null`, `description text`, `logo_object_key text`, `client_id text not null unique`, `client_secret_hash text not null`, `previous_secret_hash text`, `previous_secret_expires_at timestamptz`, `redirect_uris jsonb not null`, `scopes jsonb not null`, `grant_types jsonb not null`, `status text not null default 'active'` check in (`active`,`suspended`,`deleted`), `created_by uuid not null`, `created_at`, `updated_at`. Index `(organization_id, status)`.

`oauth_authorization_codes`: `code_hash text pk`, `app_id uuid not null` fk `oauth_apps`, `user_id uuid not null`, `redirect_uri text not null`, `scopes jsonb not null`, `code_challenge text`, `expires_at timestamptz not null`, `used_at timestamptz`. Index `(app_id, expires_at)` for the sweeper.

`api_request_logs`: `id bigserial pk`, `organization_id uuid not null`, `api_key_id uuid`, `actor_user_id uuid`, `method text not null`, `path text not null`, `status smallint not null`, `duration_ms integer not null`, `request_id text not null`, `bytes_in integer`, `bytes_out integer`, `error_code text`, `created_at timestamptz not null default now()`. Indexes: `(organization_id, created_at desc)`, `(api_key_id, created_at desc)`, `(organization_id, status, created_at desc)`. Retention 14 days by dropping old partitions; bodies are never stored.

`sdk_scaffolds`: `id uuid pk`, `organization_id uuid not null`, `kind text not null` check in (`plugin`,`theme`,`workflow`), `name text not null`, `target text not null`, `object_key text not null`, `byte_size bigint`, `created_by uuid not null`, `created_at` — an audit of generations, not a code store.

### Events

Emitted: `api_key.created`, `api_key.rotated`, `api_key.revoked`, `oauth_app.created`, `oauth_app.secret_rotated`, `sdk.scaffold.generated`. Payloads carry ids, names, scopes and the actor — never key material of any kind.

Consumed: `webhook.delivery.failed` (REQ-016) to surface endpoint health on the overview card; the event catalog is read from the bus registry at request time so it cannot drift.

Webhook relevance: yes — the key and app lifecycle events are exactly what a security-conscious organization subscribes to (alert on key creation or rotation), and the catalog itself documents every subscribable type for the same subscribers.

Audit: key create/rotate/revoke, OAuth app create/edit/delete and secret rotation, and scaffold generation. Explorer calls that mutate are audited on the owning endpoint; read-only Explorer calls appear only in `api_request_logs`.

### Acceptance criteria

- [ ] Creating a key returns the secret exactly once; no later request or page reload returns it again.
- [ ] `secret_hash` is one-way and no endpoint response, log line or rendered page contains plaintext key material.
- [ ] Revoked and expired keys receive `401` with a reason that does not echo the key.
- [ ] Scopes are enforced: a key holding only read scopes cannot perform a write request.
- [ ] IP allowlist entries reject requests from outside the listed CIDRs.
- [ ] Rotating a key issues a new secret once, invalidates the old secret immediately, and the UI states that behaviour.
- [ ] The API Explorer lists operations from the served OpenAPI document, and a CI check fails when the document drifts from the running router.
- [ ] Explorer sends run as the signed-in caller; a call the caller could not make from the UI returns the same `403`.
- [ ] The Explorer shows status, duration and body, and copies the request as curl, TypeScript and Python with a placeholder instead of a real secret.
- [x] OAuth apps reject non-`https` redirect URIs except `http://localhost`, and an authorization-code plus PKCE flow completes end to end. *(both halves are now built and proved. The **rejection** half was slice 3a/3b: `app_rules::validate_redirect_uris` refuses a non-`https` scheme unless the host is exactly `localhost`, `127.0.0.1` or `[::1]`, compared as whole strings and never as a prefix; the refusal carries the row's **position** and never the URL, because `Display` reaches a log and `Debug` reaches a panic message. The **flow** half is slice 3c: `GET /oauth/authorize` validates the client, the grant, the redirect, the scopes and the PKCE challenge **in that order and writes nothing**; `POST /oauth/consent` mints the code, re-running the whole check against the app *as it is now* (the screen was rendered from a GET, and an app can be withdrawn or narrowed in between); `POST /oauth/token` redeems the code single-use, re-checks that the redirect matches the one the code was issued for, verifies the verifier against the stored challenge by hashing both sides, and only then mints a token scoped to the **consented** set. The code→token exchange is proved in the database by `oauth_codes_challenge_is_whole` and `redeem_code`'s `used_at is null` predicate living *inside* the update — a read-then-update would let two simultaneous redemptions both succeed, and that is invisible in every test that redeems a code once.)*
- [ ] Client secret rotation keeps the previous secret valid until its overlap expiry, then rejects it. *(proved in code and in the database. `rotate_secret` moves the old hash into the overlap slot **by the same expression** that writes the new one — `previous_secret_hash = client_secret_hash` inside the `update` — so there is no window in which the old secret is in neither slot, and migration `0231`'s `oauth_apps_overlap_is_whole` refuses a partial write at the database level. `which_secret_matched` filters the overlap **by the clock before comparing** and returns *which* slot matched: `a_previous_secret_is_honoured_only_inside_its_overlap_and_the_slot_is_reported` asserts the second-before boundary works, the instant of expiry does not, and a window that expired years ago authenticates nobody. The slot name is what lets the audit row distinguish a deployment that has not redeployed from one that has, which is the only reason the overlap exists. The panel half shipped in `6fa5d647` and shows the deadline in the list row and the detail drawer.)*
- [ ] The Events catalog lists only event types the caller may subscribe to, and every sample validates against its own schema. *(the screen and its gate are shipped — `6e994015`, `318321cd` — and the box is **not** ticked, because the browser pass has not run. What is true in code: the subscribable set is separated from the reserved one *by default* and a reserved name renders "Not subscribable" instead of a Subscribe link, so a developer cannot build against a name the platform has claimed but does not emit; the panel shows the **server-generated** sample beside the **server-generated** schema of the same registry row, so the two cannot describe different payloads; and the pass asserts `theSampleCoversEveryRequiredField` — the sample must carry every property the schema marks required. That last one is the claim that can fail while both halves are valid JSON, which is why it is written against the schema's `required` list rather than a field count. The registry-side guarantee is REQ-016's: `every_catalogue_sample_validates_against_its_own_schema` in `omnion-events` holds each row to its own schema on every build.)*
- [ ] Request logs filter by key, status class, path prefix and date range, and history stays readable after a key is revoked.
- [ ] A log entry contains no bodies and no secret-looking values (asserted against the redaction list in tests).
- [ ] Plugin, theme and workflow scaffolds generate archives that install or load from a clean checkout. *(code-complete, **not** ticked — the browser pass has not run. What is true now: `crates/developer/src/archive.rs` is a real zip writer (stored entries, local headers, central directory, end record) and `entries_of` includes the **hidden** files, so `.env.example` and `.gitignore` are in the archive and not merely in the preview — a writer built from the preview list would drop them and the developer would find out after unzipping, the one moment they cannot ask the platform a question. The independent-reader proof exists and is not a Rust round-trip: `scripts/qa/probe-scaffold-archive.py` writes the bytes to disk through the crate's own example and opens them with Python's `zipfile` **and** `/usr/bin/unzip`, because a writer and a reader that agree on a mistake is the entire risk in a file format. The route's own claim is `a_regenerated_archive_is_byte_identical_...` — the download *rebuilds* the bytes rather than reading a stored copy, which is safe only because the zip stamp comes from the row's `created_at` and not from the clock. Deliberately **not** done: storing the bytes at generation time. A stored copy nobody revalidates is a copy of a template that has since changed, and the developer comparing two starters would be comparing them to themselves; the object is still written, on download, so it exists if and only if somebody actually received the bytes.)*
- [ ] Manifest validation reports schema errors with line numbers and rejects invalid manifests. *(code-complete and driven by the pass; **not** ticked, again only on the browser pass. The validator on the screen is the **same `validate_manifest` the runtime loader calls**, so a manifest it reports as loadable is one the platform will attempt to install — a laxer client-side "does it parse?" would tell people their extension is fine and then refuse to boot it. The pass asserts **both** directions (`anInvalidManifestIsRefused`, `aLoadableManifestIsAccepted`), because a validator that refuses everything satisfies the first, and each issue renders with its 1-based line (`line 12: …`) or as a document-level problem with no line at all.)*
- [ ] The CLI device-code flow issues a scoped token, and approving it from an account without `developer.keys.manage` is refused.
- [ ] Every mutating developer action appears in the audit log with actor, target and scopes.

### QA plan

Browser walkthrough:

1. `/developer` overview renders the cards and quickstart; snippets copy to the clipboard and contain no secret value.
2. Create a key (Live, read scopes, 90-day expiry) → the one-time dialog appears; copy the secret; reload → the secret is absent from the DOM and the table shows prefix plus expiry.
3. Call the API with the new key from a terminal → `200` on a read endpoint and `403` on a write endpoint (proves scope enforcement).
4. `/developer/api-explorer` → open the customers list operation → `Send` → real data returns; `Cmd+/` opens the snippet drawer; run the copied curl outside the browser with the placeholder replaced → same result.
5. `/developer/events` → pick `customer.created` → the sample validates; `Subscribe a webhook` prefills the subscription form.
6. Register an OAuth app with two redirect URIs (one plain `http` non-localhost, one `http://localhost`) → the invalid one is rejected inline; complete an auth-code plus PKCE flow with a test client.
7. `/developer/logs` → filter by the new key → the step 3 and step 4 calls are present with correct statuses and durations; open the detail drawer → confirm no bodies are shown and the explanation is visible.
8. `/developer/sdks` → generate a plugin scaffold → the archive downloads and the file-tree preview matches; drop a broken manifest → inline schema errors with line numbers.
9. CLI tab → start the device-code flow → approve it from a second session → the CLI receives a token and can call `/api/v1/me` within the granted scopes.
10. Sign in as a read-only developer role → management controls are absent and a direct `POST /api/v1/api-keys` returns `403`.
11. Keyboard and mobile: `g k` reaches keys; at 390×844 the sub-nav is a select, tables are cards, and the Explorer stacks.

Visual check: the one-time secret dialog is unmistakable (warning icon, explicit not-shown-again copy, copy button with feedback); status columns use icon plus label rather than colour alone; the Explorer's request/response split is legible at 1280px; code blocks scroll instead of breaking layout; the environment badge stays pinned.

### Slices

1. **Keys + logs.** Migration, key CRUD with rotate/revoke, secret hashing, request-log middleware with filters, `/developer/keys` and `/developer/logs`.
   Done: a key created in the UI authenticates a real call, is scope-enforced, and appears in the logs with the correct status and duration.
2. **API Explorer.** OpenAPI emission, operation browser, schema-driven request form, send-as-caller, snippet drawer, CI drift check.
   **Code-complete** (`be241bd2`, `de029671`, `54853888`): the document, the runtime, the routes, the screen and the depth pass all ship; the drift check is a test that runs in `cargo test --workspace`, which is what CI runs. **Open on the browser pass alone.**
3. **OAuth apps + events catalog.** App registration and editing, secret rotation with overlap, authorization-code plus PKCE, catalog from the event registry, webhook deep link.
   **Slices 3a and 3b shipped** (`a1840487`, `416a58bf`, `dd00be30`; tick 108). 3a: the client material, the redirect-URI rule, PKCE and the migration — **renumbered `0229` → `0231`**, because w8 holds `0229_crm_lead_sla_index_terminal_status.sql` and the migration namespace is shared across every worktree (two files numbered 229 make sqlx answer `VersionMismatch(29)` for the whole database). 3b: the store (`crates/developer/src/store_oauth.rs`), the authorization check (`authorize()` in `model_oauth.rs`), the six panel routes, both permission keys, and the seven documented operations.
   **A real open-redirect defect in 3a's own code was found by 3b's tests** (`816b89d5`): `http://[::1].attacker.example/cb` was accepted as loopback, because the fix that stopped `split(':')` reducing `[::1]` to `[` took a bracketed authority whole *as a literal*. The closing bracket is now honoured only when nothing but an optional decimal `:port` follows it.
   **Panel screen shipped** (`6fa5d647`, `2595999e`, `a560b0b7`; tick 110's commits, status corrected at tick 111). `/developer/oauth-apps` — list with name/status/grant filters, detail drawer, register, edit, rotate, suspend/resume, withdraw-with-confirmation, the one-time secret dialog and the empty/populated/loading/error states. The pass and the screen are cross-checked hook-for-hook (33/33) because a rename on either side leaves a pass that measures the previous screen and calls it green. The risk note on the overlap window ("show the expiry in the UI") is honoured in **two** places, the list row and the detail drawer, so an operator who never opens the drawer still sees that a previous secret is live. **Open: the browser pass** — the slot is held by a live `w3` run.
   **Slice 3c shipped** (`0469249e`, `d497b8f9`, `bfd699dd`, `de3b6620`, `ba95b808`, `4867981f`; tick 109). The sessionless half: `crates/developer/src/oauth_flow.rs` (the request parsing, the code, the access token, the scope-narrowing rule, the grant provenance) and `apps/api/src/routes/oauth_flow.rs` (the four endpoints, the consent screen, the RFC 6749 error shapes, the introspection response), plus migration `0232` for `oauth_access_tokens`.
   **Three defects, all in this tick's own code, and the tick is mostly about them.** A **live stored XSS** in the consent screen: the hidden fields were JSON-encoded, and JSON escapes a quote as `\"` where HTML wants `&quot;`, so a `state` of `"><script>…` closed the attribute and left a live script tag in the approver's session on the platform's own origin — reachable by any developer who can register an app in their own tenant, executed in the victim tenant's user's session. An absent value rendered as the four characters `null`, which the consent POST accepts and the token endpoint would echo back as a `state` the client never sent. And the **borrow checker** caught a third before a test could: `redirect_with_error` demands `&'static str`, and I had passed `error.to_string()` — which would have put a caller-supplied `response_type` into a `Location` header, and from there into browser history, proxy logs and the next `Referer`. The signature was the rule; the type error was the compiler enforcing it.
   The event catalogue half of this slice is **shipped** (`6e994015`, `318321cd`; tick 111) as `/developer/events` — and it is deliberately **not** a second copy of `/events`. REQ-016's screen answers "what happened?" for an operator: a feed with a time window, a cursor and a retention panel. This one answers "what CAN happen, and what will I receive when it does?" for someone writing a subscriber, so it keeps the deep link, the copyable schema and the server-generated sample, and drops the feed. The catalogue itself is one compiled registry read either way — `/api/v1/events/catalogue` — so the two screens cannot disagree about what the platform knows; what they differ on is the question.
   Done: a local test client completes the flow and every catalog sample validates against its schema.
4. **SDKs + CLI + polish.** Scaffold generator, manifest validator, CLI device-code, overview cards, permission-hidden controls, mobile layout.
   Done: scaffolds install or load, `omnion login` issues a scoped token, and the read-only role sees no management controls.
   **Code-complete at tick 112** (`8a7695c7`, `27679d38`, `3a330665`, `95b2d07e`, `5e2db6df`), and **open on the browser pass alone** like slices 2 and 3. What shipped: the zip writer with an independent-reader probe (`d64f1e76`); the archive route, the row that records it and the three panel routes (`8a7695c7`); the four-tool screen on `/developer/sdks` with a nav entry placed *after* the event catalogue, because a developer reads what the platform does, then what it emits, and only then builds against it (`27679d38`); the `?tab=cli` deep link (`3a330665`); the red determinism test (`95b2d07e`); and the depth pass plus the two probes (`5e2db6df`).
   **The permission split is argued in the router comment and worth repeating.** The list, the record and the download all take `developer.sdks.scaffold` — the same key the generator takes — rather than a `developer.read`-style key, because a list of what was produced is part of producing one, and a key nobody holds is a list nobody sees. The download is deliberately **not** a read key either: a read-only developer who can fetch the bytes of a scaffold is holding the source of an extension, which is the artefact and not a description of it.
   **Two things the slice deliberately does not do**, both argued rather than skipped: the bytes are **not** stored at generation (see the acceptance note above), and **no refresh token** is minted (an hour-long access token plus a panel that says so is the honest version of the same requirement; a second long-lived credential with no revocation story is a different product).
   **Both of the "not yet built" items now exist** (`f640f868`, `8cc6d777`, `09d3c410`, tick 113). The **overview cards** are `/developer` (`apps/admin/app/developer/page.tsx` + `developer-overview-view.tsx`): six cards, every figure read from the same list its own destination renders, so a card cannot disagree with the screen it links to; a failing read degrades one card to a refusal naming its permission, and **an unreadable list is never rendered as a silent zero** — "this tenant has no keys" and "this account cannot read the keys" are identical on a card and mean opposite things. The **read-only check** was the harder half, because it is a *fixture*, not a screen: `scripts/qa/seed-readonly-developer.cjs` seeds an account holding `developer.*.read` and not `developer.sdks.scaffold`, and refuses to finish holding the key it exists to deny. Three defects were found by **running** it rather than reading it — the seeded email did not exist (an empty CTE reads as a permission problem, not a missing row), the held-check compared a whitespace string to `"0"` and inverted itself, and a non-recursive CTE counts the table as it was *before* its own insert.

   **Still open: the browser pass** over `/developer` and the read-only sign-in. The three claims in `runDevOverviewDepth` are proven to fail in both directions against lifted fixtures (a card claiming 7 over a list of 2, and a card pointing at the wrong screen), and `dev-overview` is registered *after* the other developer passes because it counts what they wrote.

### Risks / notes

- Secret handling is the headline risk: hash at rest, display once, never in URLs, logs, telemetry or error text; keep a test that greps every response during the walkthrough for the secret value.
- OpenAPI drift would teach wrong calls: emit the document from the same router definition and fail the pipeline when a route lacks annotations.
- The Explorer can resemble a privileged proxy: it must run with the caller's session and permissions only, and it is rate-limited per user.
- Overlap windows briefly double the valid-secret surface: cap the window, log old-versus-new secret usage distinctly, and show the expiry in the UI.
- Request logs are attractive to attackers: store no bodies, keep the retention window explicit (14 days), and mask client identifiers according to the compliance policy.
- Manifest validation must use the same code path as the runtime loader; a laxer validator produces extensions that pass review and fail to boot.
- Device-code phishing: codes are short-lived, bound to the approving user, displayed with requesting-client metadata, and cannot be approved by a session lacking key-management permission.
- Scaffolds get copied into public repositories: ship a README warning against committing tokens and a placeholder-only example environment file.
