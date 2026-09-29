> **Status:** in-progress (slice 1: `a546d01` the directory configuration language and its step
> ladder, `f94f5d8` the panel, `e0ac90a` the enable gate and the HTTP surface, `245951e` the
> live-database gate. Slice 2 part 1: `80ed5c6` the attribute map — `0052`, the configuration
> language, the atomic replace, the preview that runs the sign-in function, the API and the
> editor. Part 2: `b2e3672` the map decides who a sign-in becomes, and `12c12f6` two walks that
> predated the enable gate. Part 3: `645bead` the map on the sign-in path. Part 4: `4b889ee` the
> protocol kinds get their own step ladder, `ff7dbc6` a discovery document that is not the
> configured issuer is refused at the point every path reads it, `6b8a2b1` the test answers with
> a ladder for every kind, `8c2884c` two SSO walks that raced on a shared host and a blanket
> cleanup, `f017948` the walkthrough reads the ladders it renders. Part 5: `dd3e08a` the
> local-password half of the sign-in invariant — and it was broken, not merely unproven. Part 6: `d9638e3` the identity migrations moved out of a band four writers took, `a7966d1` ordered role rules with a dry run that runs the sign-in's own evaluator, `3caad0c` the editor that shows the whole walk, `a5f58cd` the walk that proves the stored set survives a refused write,
> `f0b0fe3` the rules decide on the sign-in path and the audit names the one that did, `2512e7e`
> a walk step that had been passing on a developer's second tenant) ·
> **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`, `crates/auth`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Directory and protocol logins beyond local accounts.

- LDAP authentication and Active Directory authentication (bind + search, group sync).
- OIDC login, SAML login, OAuth2 providers, SCIM provisioning (create/update/deactivate users).
- Provider registry screen: add/edit/test a provider, map attributes, default role on first login.
- SSO → role mapping rules (claim/group → role), with a dry-run preview.
- Identity provider sync status, last sync, error surfacing; per-organization enablement.
- Plugin-declared permissions so an integration can extend the identity surface safely.

## Implementation spec

### Scope (in / out)

**In**
- Provider registry per organization with kinds `ldap`, `active_directory`, `oidc`, `oauth2`, `saml`: create, edit, duplicate, test, enable/disable, delete. Credentials live behind a secret-store reference and are never stored in or returned to the panel.
- LDAP and Active Directory: simple and search-then-bind with a service account, base DN + user filter, LDAPS/StartTLS, paged search, referral handling, nested group resolution with a depth cap, attribute discovery, and scheduled group + user sync. AD adds UPN/sAMAccountName matching and the account-disabled flag mapping.
- OIDC (authorization code + PKCE) and generic OAuth2 (configurable endpoints and user-info URL) with discovery validation, plus SAML 2.0 (metadata URL or pasted XML, signature validation, assertion consumer service). Redirect URIs are shown read-only for copy.
- Attribute mapping: external attribute → panel field (email, username, display name, phone, department, title, employment type, employee id) with transforms (trim, lowercase, prefix, static, split), required-field enforcement, and a preview against a sample claims payload or a directory entry sample.
- Role mapping rules: an ordered WHEN (claim / group / department / title) → THEN (role + scope) list, first match wins with an optional `stop`, a default role on first login, and a dry-run preview that names the matched rule.
- SCIM 2.0 provisioning: per-organization tokens (prefix + hash, secret shown once, expiry required, revocable), `Users` and `Groups` endpoints, create/update/deactivate (deactivation is the default, deletion is refused), and a sync log.
- Sync surfacing: last sync, next run, counts, per-subject failures with a retry, and a status chip that goes `degraded` when the last test or sync failed.
- Plugin-declared extension: an installed plugin may declare a provider kind and namespaced keys; the declaration is validated against the permission catalogue (REQ-068) and the kind cannot be enabled until those keys exist and are assignable.

**Out**
- Role and permission depth (REQ-067, REQ-068) and ABAC (REQ-069); directory-backed groups as first-class panel groups (REQ-071).
- MFA and device trust (REQ-066): SSO completes the first factor only and hands off to the same second-factor challenge.
- Organization lifecycle and tenant hierarchy (REQ-005); invite-by-link flows for local accounts.
- Bespoke per-vendor social integrations beyond one generic OAuth2/OIDC path.

### Screens (UI)

Nav: **Settings → IAM → Authentication** and **Settings → IAM → Provisioning**; `/login` gains provider buttons above the password form.

| Route | Screen |
|---|---|
| `/settings/iam/authentication` | Provider list with status, last sync and test result |
| `/settings/iam/authentication/new` | Kind picker → wizard: Basics · Connection · Attribute mapping · Role mapping · Enable |
| `/settings/iam/authentication/{id}` | Tabs Connection · Attribute mapping · Role mapping · Sync · SCIM · Audit |
| `/settings/iam/provisioning` | SCIM tokens and the sync log |
| `/login` (extended) | One button per enabled provider; a failed round trip names the provider and shows an error code, never the raw IdP response |

- **Provider list.** Columns Name (kind badge), Kind, Organization, Status (`enabled` / `disabled` / `degraded`), Last sync, Users synced, Last test, Actions. Filters: kind, status, organization, free-text search (250 ms debounce). Bulk: Enable, Disable, Sync now. Row actions: Test connection, Sync now, Edit, Duplicate, Delete (blocked while provisioned users exist unless reassignment is confirmed with the affected count shown). Empty state explains local sign-in still works and offers `Connect a provider`.
- **Connect wizard.** Step 1 picks the kind and shows exactly the fields that kind requires. Step 2 collects connection settings with per-field validation (host/URL, base DN, bind DN, TLS mode; issuer/discovery URL, client credentials by reference; metadata URL or XML, certificate fingerprint) and a `Test connection` button that reports each step — DNS → TCP → TLS → bind/search or discovery → claims. Step 3 edits the attribute map with a live preview table (external value → panel field → transformed value) and flags missing required fields. Step 4 builds the role rules and runs the dry run. Step 5 sets enablement, default role and JIT provisioning. A provider cannot be enabled while its last test has never passed; the wizard keeps a resumable per-user draft.
- **Role mapping and dry run.** Rules table with drag order: `#`, When (kind + key + operator + value), Then (role + scope), Default, Enabled, plus `+ Add rule`. A paste/upload panel takes a sample identity (claims JSON, LDAP entry, or SAML assertion summary) and `Preview` highlights the matched rule, shows the resulting role and scope, and calls out `no rule matched → default role`. Saving writes an audit entry with the rule diff.
- **Sync and SCIM.** Sync tab: runs table (Started, Kind, Duration, Users seen/created/updated/deactivated, Groups, Errors, Status) with a drawer per run listing failures (subject, code, message) and `Retry failed`. Provisioning: token table (Name, Prefix, Scopes, Created, Expires, Last used, Revoked) with `New token` (secret shown once with copy), revoke and rotate; sync log table (Time, Source, Operation, Subject, External id, Outcome, Message) with filters and CSV export.
- **States, keys, mobile.** Skeletons on first paint, real empty states, error strips with retry; secret fields render as `••••` plus the reference label and never echo a stored value. Keys: `/` focuses search, `j`/`k` move rows, `enter` opens, `t` tests, `g p` provisioning, `Esc` closes. Below `lg` tables become cards, the wizard goes single-column with a sticky footer, and the dry-run table becomes a stacked list with the matched rule first.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/iam/providers` | List with status counts · create a draft provider | `iam.providers.read` · `iam.providers.manage` |
| GET · PATCH · DELETE | `/api/v1/iam/providers/{id}` | Read · update (secrets by reference only) · delete | `iam.providers.read` · `iam.providers.manage` |
| POST | `/api/v1/iam/providers/{id}/test` | Step-by-step connection test | `iam.providers.manage` |
| POST | `/api/v1/iam/providers/{id}/{enable,disable}` | Enable after a passing test · disable immediately | `iam.providers.manage` |
| PUT | `/api/v1/iam/providers/{id}/attribute-mappings` | Replace the attribute map (validated, atomic) | `iam.providers.manage` |
| PUT · POST | `/api/v1/iam/providers/{id}/role-rules` · `/role-rules/preview` | Replace ordered rules · dry-run a sample identity | `iam.providers.manage` |
| POST | `/api/v1/iam/providers/{id}/sync` | Start a sync now | `iam.providers.manage` |
| GET | `/api/v1/iam/providers/{id}/sync-runs` (+`/{run_id}/errors`, `/retry`) | Run history · failures · retry failed subjects | `iam.providers.read` · `iam.providers.manage` |
| GET · POST | `/api/v1/iam/provisioning/tokens` | List tokens (prefix only) · issue (secret once) | `iam.provisioning.read` · `iam.provisioning.manage` |
| DELETE | `/api/v1/iam/provisioning/tokens/{id}` | Revoke a token | `iam.provisioning.manage` |
| GET | `/api/v1/iam/provisioning/log` | Sync log with filters and CSV export | `iam.provisioning.read` |
| GET · POST | `/api/v1/auth/sso/{slug}/start` · `/callback` | Authorization start · code/assertion exchange | public sign-in route |
| POST | `/api/v1/auth/mfa/verify` | Second factor after SSO or password (REQ-066) | public sign-in route |
| GET · POST · PATCH · DELETE | `/api/v1/scim/v2/Users` and `/Groups` (+`/{id}`) | SCIM 2.0 provisioning (subset: core schema, PATCH) | provisioning token |

SSO routes never disclose whether an account exists; callback errors are stable codes logged with ids only. Enabling a provider, changing a secret reference, or issuing a provisioning token demands step-up authentication (REQ-066). SCIM tokens are scoped to exactly one organization.

### Data model

Migrations: `0116_identity_providers.sql`, `0117_provider_provisioning.sql` (reserved band 0116–0125 for the identity & access wave; the ledger is append-only — take the next free number if taken). Base tables are shared with the IAM core migration (REQ-006): one table per concept — these files create what is missing and stay additive otherwise.

- `auth_providers` (id uuid pk, organization_id uuid → organizations on delete cascade, slug text, name text, kind text in ('ldap','active_directory','oidc','oauth2','saml'), enabled bool default false, config jsonb default '{}' — non-secret settings only, secret_ref text, scopes text[] default '{}', jit_provisioning bool default true, default_role_id uuid null → roles, group_claim text, sync_interval_minutes int default 60, last_sync_at timestamptz, last_sync_status text in ('ok','partial','failed'), last_test_at timestamptz, last_test_ok bool, plugin_key text null, created_by uuid, created_at/updated_at) — unique (organization_id, slug); index (organization_id, kind) where enabled.
- `provider_attribute_mappings` (id uuid pk, provider_id uuid → auth_providers on delete cascade, source_attr text, target_field text in ('email','username','display_name','phone','department','title','employment_type','employee_id'), transform text in ('none','trim','lowercase','prefix','static','split') default 'none', transform_arg text, required bool default false, position int) — unique (provider_id, target_field); index (provider_id, position).
- `provider_role_rules` (id uuid pk, provider_id uuid cascade, position int, when_kind text in ('claim','group','department','title','always'), when_key text, when_operator text in ('equals','contains','starts_with','regex'), when_value text, role_id uuid → roles, scope_type text in ('organization','site') default 'organization', site_id uuid null → sites, stop bool default false, enabled bool default true, created_at) — index (provider_id, position).
- `provider_group_links` (id uuid pk, provider_id uuid cascade, external_id text, external_label text, member_count int default 0, last_seen_at timestamptz, synced bool default true) — unique (provider_id, external_id).
- `directory_sync_runs` (id uuid pk, provider_id uuid cascade, kind text in ('full','delta','scim','manual'), started_at/finished_at timestamptz, status text in ('running','ok','partial','failed'), users_seen/users_created/users_updated/users_deactivated int default 0, groups_seen int default 0, error_count int default 0, message text, triggered_by uuid null) — index (provider_id, started_at desc).
- `directory_sync_errors` (id uuid pk, run_id uuid cascade, subject text, code text, message text, created_at) — index (run_id).
- `provisioning_tokens` (id uuid pk, organization_id uuid cascade, name text, prefix text, secret_hash text, scopes text[] default '{}', expires_at timestamptz, last_used_at timestamptz, revoked_at timestamptz, created_by uuid, created_at) — unique (prefix).
- `provisioning_log` (id uuid pk, organization_id uuid cascade, token_id uuid null → provisioning_tokens on delete set null, operation text in ('create','update','deactivate','group_update'), subject_type text in ('user','group'), subject_id uuid null, external_id text, outcome text in ('ok','conflict','error'), message text, created_at) — index (organization_id, created_at desc).
- `users` += `identity_source` text default 'local' check in ('local','ldap','active_directory','oidc','oauth2','saml','scim'), `provisioned_by_provider_id` uuid null → auth_providers on delete set null, `external_id` text null, `external_synced_at` timestamptz — index (provisioned_by_provider_id) where identity_source <> 'local'; unique (provisioned_by_provider_id, external_id) where external_id is not null.

`config` never holds a secret; connection secrets, SCIM tokens and client secrets are stored as references or hashes and are unreadable through the API.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `iam.provider_created` · `iam.provider_updated` · `iam.provider_disabled` | Registry changes | `provider_id`, `kind`, changed field names — never values |
| `iam.provider_test_passed` · `iam.provider_test_failed` | Connection tests | `provider_id`, `step` code, `duration_ms` |
| `iam.directory_sync_started` · `iam.directory_sync_completed` · `iam.directory_sync_failed` | Sync run lifecycle | `provider_id`, `run_id`, counts, `status` |
| `iam.sso_signin_succeeded` · `iam.sso_signin_failed` | Callback outcomes | `provider_id`, `subject_id` or `external_id`, `reason` code |
| `iam.role_rule_matched` | A sign-in was assigned a role by rule | `subject_id`, `rule_position`, `role_id`, `scope_type` |
| `iam.provisioning_token_issued` · `iam.provisioning_token_revoked` | Token lifecycle | `token_id`, `prefix`, `expires_at` |
| `iam.user_provisioned` · `iam.user_deprovisioned` | SCIM/directory create and deactivate | `subject_type`, `subject_id`, `external_id` |
| `iam.group_membership_synced` | Group links changed by a sync | `provider_id`, `group_id`, added/removed counts |

Consumed: `iam.role_permissions_changed` (mapping previews and cached rule results drop), `user.created` (default binding), `iam.security_policy_changed` (sign-in policy reload). Webhook relevance: the security centre (REQ-012) subscribes to test/sync/sign-in failures, automation (REQ-003) can trigger on `iam.user_provisioned`. Payloads carry ids, counts and codes — never claims values, tokens or e-mail addresses.

### Acceptance criteria

- [x] `0116`/`0117` apply on a fresh and on a populated database; constraints and indexes land as specified; `cargo test --workspace` is green. *(Slice 1 takes `0051` instead — the REQ's own reserved band; the released high-water mark was 0050 and the mapping/rule/sync-run tables land with their own slices. `scripts/qa/run-iam-directory.sh` → **PASS 6/6**: 32 migrations applied in filename order, both directory kinds accepted, the surviving `kind` check asserted to be the *wide* one, an unknown kind still refused, the test state round-tripping through all three values, negative and over-long sync intervals refused, six registry columns + the partial index confirmed present, and **0051 applied to a populated `auth_providers` table** — 2 pre-existing rows survived, still enabled, still `never tested`. `cargo test -p omnion-identity --lib` → **131 passed**, `-p omnion-api --lib` → **165 passed**.)*
- [ ] An OIDC provider is created against the test identity provider fixture, `Test connection` passes step by step, and the provider can only be enabled after a passing test. *(Slice 1 ships the **enable gate** — `providers::enable_gate` refuses an untested provider and a failed one alike, `POST /{id}/enable` and `/disable` are separate verbs so the safe direction is never gated, and the generic PATCH asks the same question so the edit form and the Enable button are two doors with one lock. The live OIDC round trip against the stub IdP is slice 2's job and is not claimed here.)*
- [ ] An LDAP/AD provider binds with a service account, searches the configured base and resolves nested groups to the depth cap; a wrong bind DN produces a field-level error naming the bind step.
- [x] Discovery or metadata validation refuses a wrong issuer or an invalid certificate naming the failed check, then succeeds against the fixture. *(`4b889ee` + `ff7dbc6`.)* The protocol kinds had one result on the argument that they fail in exactly one place; they fail in four, and three of those repairs are unrelated to each other. `sso::protocol_steps` walks them — **discovery** (or, for SAML, **certificate**) → **issuer** → **key set** → **claims** — and the failing step's name rides on the `iam.provider_test_*` event, so a webhook subscriber at 3am learns *which* check refused rather than that "the provider is broken". A failure stops the ladder and leaves the rest `pending`: a claim read against an issuer that was never trusted is not a claim about anything, and the tests assert that rather than trusting the shape of the code. Two boundaries are deliberate and each has a test saying so. A **trailing slash is not a mismatch** — every discovery URL is built by appending to the configured issuer, so operators type one form and providers publish the other; everything *else* is compared exactly, because a normalization that trims more would accept a different host. An **unreadable key set is not reported as an absent one**: a provider that publishes only keys this platform refuses and a provider whose `jwks_uri` is wrong are different repairs, and one empty vector would send somebody to fix a provider that was working. A provider with **no** configured issuer is told, not failed — its endpoints are entered by hand, so there is nothing to compare, and refusing would break a legal configuration. And `require_issuer` runs inside `discovery_for`, the one function every code path reads the document through, rather than in the callback: the metadata cache serves one document to every later sign-in, so a mismatch the button reports and the callback ignores is a mismatch an operator was told about and nothing acted on. `cargo test -p omnion-identity --lib` → **166 passed** (19 in this module), `cargo test -p omnion-api --lib` → **165 passed**, and the three live walks `sso` / `sso_live` / `sso_attribute_map` → **1 / 2 / 1 passed** against the isolated `omnion_w9_iso`.
- [ ] A start → callback round trip signs in a user that exists in the fixture; the failure page for an unknown user is indistinguishable from a wrong-password failure.
- [ ] JIT provisioning creates the user with only the mapped attributes; a missing required attribute refuses the login naming the field instead of creating a half account. *(Proven by slice 2. `0052` gives `provider_attribute_mappings` its own rows rather than another document inside `config.role_mappings` — a second differently-shaped blob in the same JSON is how "save the attribute map" ends up rewriting the role rules nobody was looking at. The *replace* is a transaction, so the window in which a provider has no email mapping — a window in which a real person is refused a sign-in for a reason nobody can see from the panel — cannot exist even for a moment. `AttributeMap::project()` is both the callback's function and the preview's, so the rehearsal and the sign-in cannot disagree. A refused projection **withholds its values** rather than returning a half account, and a refused *write* leaves the stored map untouched. `scripts/qa/run-iam-attribute-map.sh` → **PASS 14/14** (a *populated* `auth_providers` table, and each constraint asserted as a refusal rather than as a success). The HTTP walk `apps/api/tests/iam_attribute_map.rs` → **1 passed** against an isolated database, including the cross-tenant case answered as *absent* rather than forbidden, and the audit entry asserted for what it must **not** contain: a department and an employee number are the rows themselves, and a mapping diff in a log nobody audits is a second copy of the directory. `cargo test -p omnion-identity --lib` → **147 passed**. The map is now also on the **sign-in path**: `finish_sign_in` re-projects the verified claims through the stored map with the same `project()` the preview runs, and does it before anything reads the email — so the account key, the JIT account and the audit line all use the address the operator configured. Two boundaries are deliberate and asserted: a provider with **no** map is untouched (“not configured yet” must not mean “nobody can sign in”), and groups and the subject are left alone (the map maps fields; it must not quietly break the role rules). `apps/api/tests/sso_attribute_map.rs` → **1 passed** against the **real router and the real stub identity provider**, asserting the one thing that cannot be true unless the callback reads the map: the address written to the account is the one the *map* produced, not the one the claim carried. Pointing the map at a claim the provider does not send is then refused as `attributes_incomplete`, by name, creating **nobody**.)*
- [ ] Role rules apply first-match-wins; the dry run predicts the same role and scope that a real login assigns for the same sample identity, and the user's audit shows `role via rule #N`. *(`a7966d1` + `3caad0c` + `a5f58cd` + `f0b0fe3`. All three clauses are now proven against a *real* sign-in — see the note under the tick's entry in BUILD-LOG.md; the box stays unticked only because the browser pass has not yet **observed** the wizard and the dry run in the panel, which is the one remaining half of this criterion.) The rules are rows with an explicit `position` and a unique `(provider_id, position)` index, because the order **is** the semantics — a tie would make "which role wins" an unspecified detail. `RoleRules::resolve` is the only implementation of the walk, and the dry run calls it, which is the whole reason the preview is evidence rather than decoration. Three decisions the tests pin down:

  * **A closed vocabulary.** Five `when_kind`s and four `when_operator`s, no expression syntax. A rule language extensible at save time is a rule language with an injection surface, and the person pasting into it is a directory operator. The database refuses the same values the parser does, asserted as refusals rather than as successes (`run-iam-role-rules.sh` → **PASS 18/18**).
  * **A miss is a result.** `Resolution::Default` carries the sentence `no rule matched → default role`. A rule set that silently grants nothing is indistinguishable from a misconfigured provider; the panel and the audit say which happened. Asserted for a sample that matches nothing, and for the `default` tag itself.
  * **A pattern that will not compile matches nothing, not everything.** A broken rule falls through to the default rather than being read as a catch-all, because silently granting the broad role on a typo is the worst outcome available. A regex that is *valid but expensive* (`a{1,1000000}` is 12 characters of source and 64KB of compiled program, rebuilt on every sign-in) is refused at save time by name — and a test asserts four ordinary patterns are still accepted, because a guard that refuses legitimate rules trains the operator to disable it and then the guard is gone.

  The walk proves **atomicity as an observed fact**: a set containing one invalid rule is refused 422 and the stored set is then compared with what it was before, byte for byte. A half-written rule set silently changes which role a colleague gets, and "transaction" in a doc comment is not evidence. It also proves the two refusals the schema *cannot* express: a provider in another tenant is answered `404` rather than `403` (403 leaks that the id exists), and a role in another tenant — a valid row, since `roles.organization_id` is nullable for platform roles — is refused by `assert_role_visible`, so the application is proved to be the gate rather than the database. The audit entry is read as `metadata::text` and searched as a *string*, not decoded and searched field by field, and is asserted to carry the provider slug and `row_count` while carrying **none** of the four group/department/title values the walk saved: a rule's `when_value` is usually a group name, and a rule diff in a log nobody audits is a second copy of the directory.

  Two findings the work forced, both live rather than theoretical. A claim path reader that split on dots could never reach a URI-named claim — `https://claims.example.com/team` becomes five segments and none of them exist — so a rule on one would silently never match, which reads to an operator as "the rule is wrong" and sends them to edit a rule that was correct; exact keys are now tried before the dotted path. And the regex guard was originally justified as catastrophic backtracking, which is **false** for this crate: `regex` is a finite automaton and the textbook `(x+x+)+y` matches in 89µs, so rejecting it would be superstition dressed as a control. The comment now says what the guard is actually for. `cargo test -p omnion-identity --lib` → **191 passed** (was 166), `-p omnion-api --lib` → **194 passed** (was 165), `--test iam_role_rules` → **1 passed** against an isolated database, run twice to prove it is re-runnable after a failure, `pnpm --filter @omnion/admin typecheck` → clean.*

  **The callback half, closed by `f0b0fe3`.** Until this tick the preview was a *picture* of the
  rules: `RoleRules::resolve` existed and the dry run called it, but a real callback read the
  legacy claim mapping in `config` and granted from that. `finish_sign_in` now resolves through
  the same reader and the same evaluator the preview uses. **A rule set is authoritative when it
  exists** — a provider with rules *and* a leftover claim mapping would otherwise grant the
  union, and the dry run would be a lie about the sign-in it exists to predict. The walk is built
  so it can only pass one way: the provider carries BOTH a legacy `analytics → editor` mapping and
  a two-rule set that says something different, and the test asserts `editor` is **absent**.
  That single assertion is what proves first-match-wins means *first*.

  Three boundaries, each a decision rather than an implementation detail. A site-scoped rule
  grants a `Scope::Site` binding, and a site in another organization attaches *nothing* rather
  than falling back to organization-wide — a wider grant than the operator wrote must never
  happen quietly. The audit carries `role via rule #N` and the roles it produced, and the walk
  asserts the rule's `when_value` is absent, because that value is usually a group name and a
  rule diff in a log nobody audits is a second copy of the directory.
  `iam.role_rule_matched` fires only on `Matched`: emitting it for a default role would make the
  event's *name* false, and the security centre subscribes to it. The walk asserts that too — a
  sign-in no rule decided fires nothing, and says `no rule matched` in its audit.
  `sso_live` → **3 passed** with `--nocapture`; `sso` → **2 passed**; identity → **191**;
  api lib → **194**; `run-iam-role-rules.sh` → **PASS 18/18**.

  **Not claimed.** The dry run deliberately evaluates the **stored** rules rather than unsaved
  ones, and a **site-scoped** rule is exercised by the unit tests but is not yet driven through a
  real callback. The browser pass has not observed the wizard and the dry run end to end, so this
  box stays unticked and slice 3 stays open.
- [ ] An account disabled in the directory is refused at the next sync and its active sessions are revoked.
- [x] Local sign-in still works while an enabled SSO provider is misconfigured — a provider failure never locks local accounts out. *(The provider-shaped half: a provider with **no** attribute map signs in exactly as it did before the map existed, asserted in `sso_attribute_map.rs` step 1. A provider whose issuer does not resolve cannot be enabled at all — the gate refuses it, and the start route answers 404 without a redirect, so a broken provider never becomes the only door. The **password** half is `dd3e08a`, and it found the invariant was half-broken rather than merely unproven: a JIT row stores the literal `!jit:no-password` in `password_hash`, `sign_in` handed it to the Argon2 verifier unconditionally, and the parse failure had no arm in the error mapping — so a guess against **any** SSO account answered `500 internal_error` ("password hashing failed: password hash string missing field") where a local account answered `401`. That is not only a broken message: the status code is a **tell**, so the password form could enumerate the directory, which is the one thing the constant-time unknown-address branch exists to prevent. `is_jit_account` already existed for exactly this and was called nowhere. It is now refused *as a credential* — the same `InvalidCredentials` after the same `dummy_verify` work — and checked before the lockout while registering no failure, because a password-less account cannot be brute-forced and counting guesses against it would only let anybody lock a colleague out of the one sign-in that still works for them. The walk asserts status, code **and** message are identical to a wrong password against a local account, that the local account still signs in with the provider connected, and — asserted, not assumed — that the row under test really carries the marker. `cargo test -p omnion-api --test sso` → **2 passed** (red at `500` before the fix).)*
- [ ] A SCIM create → update → deactivate round trip writes three sync-log rows and deactivates (never deletes) the user; an invalid or revoked token answers 401 without disclosing anything.
- [ ] A SCIM group create links its members, emits `iam.group_membership_synced`, and a group rule grants the mapped role on the next request.
- [ ] Sync run history shows counts and per-subject errors; `Retry failed` reprocesses only the failed subjects and the run status reflects the outcome.
- [ ] A provisioning token secret is shown exactly once; the list afterwards shows only the prefix; a rotated token refuses the old value on the next request.
- [ ] Bulk enable/disable works; a disabled provider's button disappears from `/login` within one page load.
- [ ] Deleting a provider that provisioned users is blocked with the affected count listed; after reassignment the delete succeeds and the users fall back to local accounts.
- [ ] Every new screen renders at 390 px without horizontal scroll, all lists have real empty/loading/error states, and the walkthrough reports zero high findings.

### QA plan

The walkthrough must click through `/settings/iam/authentication` (open the wizard, create the OIDC fixture provider, run `Test connection` and read the step list, save, enable), run the mapping dry run with a pasted sample payload and read the matched rule, and visit `/settings/iam/provisioning` (issue a token, run SCIM create/update/deactivate against the API, reload the log, revoke the token). One SSO round trip is exercised in a fresh browser context. Visual check: kind badges and status chips show real states, the wizard marks the failing step instead of a generic error, the dry-run highlights the matched row, the sync-run drawer lists real per-subject messages, no secret value is ever present in the DOM, and screenshots `page-iam-authentication`, `page-iam-provider-wizard`, `page-iam-provider-mapping`, `page-iam-provisioning`, `mobile-iam-authentication` are produced.

### Slices

1. **Registry and connection test.** Migrations, provider CRUD, wizard steps 1–2, the test endpoint with its step report, the enablement gate, and the list screen with status chips. *Done when:* acceptance 1–4 pass and the provider list is in the walkthrough inventory.
2. **Interactive SSO and attribute mapping.** Start/callback routes (PKCE and SAML validation), discovery, JIT provisioning, attribute mapping editor with preview, `/login` provider buttons, and the local-sign-in invariant. *Done when:* acceptance 5–7 and 9 pass and one real round trip is exercised.
3. **Role rules and dry run.** Ordered rules with operators and scope targets, the dry-run endpoint and panel, the audit reason line, and the role-rule event. *Done when:* acceptance 8 passes and the dry run agrees with a live assignment for the sample identity.
4. **SCIM and sync surfacing.** Tokens, SCIM `Users`/`Groups`, sync runs with errors and retry, status chips, and plugin-declared kinds validated against the catalogue. *Done when:* acceptance 10–15 pass and the sync log is exercised end to end.

### Risks / notes

- **Secrets by reference only.** The wizard never round-trips a stored secret to the browser; test results and sync logs carry codes and messages, not raw provider responses.
- **Directory scale.** Nested-group resolution and paged searches can explode on large directories: depth cap, single-flight paging, and a hard per-run subject cap with a visible warning instead of a silent truncation.
- **Identity linking.** One account per e-mail: connecting a directory identity to an existing local account is an explicit, audited admin decision (or a verified invite link), never a silent match on e-mail.
- **SCIM subset.** IdP group semantics differ; the supported subset (`Users`, `Groups`, core schema, PATCH) is documented and unsupported features are refused with a clear message rather than half-applied.
- **Enumeration.** Start and callback routes must stay constant-time for unknown users and must not leak whether an account exists.
- **Plugin-declared keys** cannot escape the catalogue: an unknown or non-assignable key fails provider activation with the key named (REQ-068 is the source of truth).
- **Testing SSO honestly.** The QA stack ships a local test identity provider; a skipped or mocked flow is not evidence and must fail the gate.
- **Timestamps** in the panel use the organization timezone; sync schedules are stored and evaluated in it too, so "next run" never drifts with a server locale.
