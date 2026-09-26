# REQ-069 — Policy Engine (ABAC)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/policy-engine`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Attribute-based rules on top of roles.

- Policy store with named policies; WHEN/AND/THEN builder in the panel (no code required).
- Conditions on user attributes (department, title, employment type), resource attributes (owner, site, status, amount) and numeric comparisons.
- AND chaining, negative conditions, time windows.
- Decision pipeline: role allow → policy deny → policy allow, with a recorded reason.
- Safety: policy validation, dry-run against a user/resource pair, policy audit trail.

## Implementation spec

### Scope (in / out)

**In**
- New `crates/policy-engine`: a pure, deterministic evaluator over a condition tree (`and`, `or`, `not`; operators `equals`, `not_equals`, `in`, `not_in`, `contains`, `starts_with`, `ends_with`, `exists`, `gt`, `gte`, `lt`, `lte`, `between`, `before`, `after`), typed comparisons with no implicit coercion, and the documented rule that a missing attribute compares as `null` and a null comparison is `false` rather than an error.
- Attribute vocabulary: user (department, title, employment_type, country, organization, attributes from the profile JSON), resource (type, id, site, owner, status, amount, path), request (action, method, time, weekday) and organization (id, plan). The list is served from a registry so the builder can only offer attributes and operators that exist.
- Time windows: weekday selection plus `from`/`to` hours evaluated in the organization timezone, and an optional absolute date range — both as ordinary conditions, so they compose with everything else.
- Policy store per organization: named policy, description, effect (`allow` / `deny`), target permission keys, priority `0–1000`, enabled flag, and a version written on every save.
- Decision pipeline: the resolver runs stages in the documented order — RBAC explicit deny → policy deny → RBAC explicit/inherited allow → policy allow → default deny — and every verdict records its stage, the winning policy (when one applied) and a human-readable reason.
- Dry run: evaluate one policy (or the whole enabled set) against a sample subject and resource, highlight matched and unmatched conditions, show the resulting stage and verdict, and save the sample as a reusable test case.
- Validation: unknown attribute, unknown operator, type mismatch (string operator on a number), empty target list, duplicate policy name per organization, condition depth > 4, more than 32 conditions, priority outside `0–1000`, and a target key that is not assignable in the catalogue (REQ-068) — each refused with the offending node named.
- Safety rails: policies cannot target non-assignable keys; `iam.policies.manage` itself can only be targeted when the organization turns on an explicit self-grant setting; a dangerous key target demands a confirmation and is recorded in the audit entry.
- Audit trail: version history with diffs, audit-log entries for every policy change, and a decision log for state-changing calls that shows stage, outcome, winning policy and reason without attribute values.

**Out**
- Role and permission authoring (REQ-067, REQ-068) and scope bindings (REQ-070); the interactive simulator UI (REQ-074) — the dry run here is scoped to a policy, the simulator evaluates an arbitrary subject/action/resource.
- Approval-based grants (REQ-073) and AI tool permissions (REQ-100); both consume this engine but own their own surfaces.
- Scripting, custom functions or expressions in conditions; the tree stays declarative and closed so a stored policy can never execute code.

### Screens (UI)

Nav: **Settings → IAM → Policies**.

| Route | Screen |
|---|---|
| `/settings/iam/policies` | Policy list with effect, targets, priority and condition summary |
| `/settings/iam/policies/new` · `/settings/iam/policies/{id}` | Builder tabs: Condition · Then · Test · Versions · Audit |
| `/settings/iam/policies/decisions` | Decision log |

- **List.** Columns: Name (link, enabled dot), Effect badge, Targets (`content.pages.publish` + `+N`), Priority, Condition summary (a readable one-line WHEN rendered from the tree), Updated by + at, Version, and a `never matched in 30 days` chip fed by the decision log. Filters: effect, enabled, target permission, attribute used, search. Bulk: Enable, Disable. Row actions: Edit, Duplicate, Disable, Delete (confirm, names the policy). Empty state explains that roles and permissions already decide, and policies only add attributes on top — with `Create the first policy`.
- **Builder — Condition.** WHEN rows: attribute picker grouped by source with type icons, operator select filtered to the attribute's type, and a value editor matching the type (text, numeric input with range validation, datetime picker, enum select, weekday + hour range). `AND` / `OR` grouping with indentation and a group header, per-condition `NOT` toggle, drag to reorder, `+ Add condition` and `+ Add group`. An inline summary line under the header reads the tree back as a sentence. Validation errors appear at the node, not in a toast.
- **Builder — Then.** Effect segmented control (Allow / Deny), target permissions multi-select grouped by module with a search box and selected chips, priority input with a hint that lower numbers evaluate after higher ones on ties? — spell it: `higher priority wins; on a tie a deny wins`. Enabled switch, description, and a confirmation step when a target carries the `dangerous` flag. Sticky footer: `Save` (disabled until dirty and valid), `Discard`, `Preview impact` (opens Test with the current draft).
- **Test tab.** Two panels: sample subject (pick a user or paste an attribute set) and sample resource (type plus id picker or pasted attributes), and `Run`. Result: a verdict card (`ALLOW` / `DENY`, stage, winning policy), the condition tree re-rendered with matched conditions green, unmatched grey and negated conditions struck through, and the resolved target permission. `Copy as test case` stores the sample on the policy. A link jumps to the simulator (REQ-074) with the same subject and resource.
- **Versions.** Timeline as in the role screen: version, actor, timestamp, changed nodes; `Compare` opens the side-by-side tree diff (added conditions green, removed red); `Restore` writes a new version.
- **Decision log.** Columns: Time, Subject, Permission, Resource, Outcome (allow / deny), Stage (`rbac_deny`, `policy_deny`, `rbac_allow`, `policy_allow`, `default_deny`), Winning policy, Reason. Filters: policy, outcome, stage, permission, date range. Rows are state-changing calls only and may be sampled at volume — the screen says so with a `sampled` badge instead of pretending completeness. `Export CSV` honours the filter set; retention is stated on the screen.
- **States, keys, mobile.** Skeletons, empty states per tab, error strip with retry; a dirty draft warns before navigation and survives a palette open. Keys: `/` search, `j`/`k` rows, `enter` open, `n` new policy, `t` test, `Esc` closes. Below `lg` the builder stacks (condition tree first, THEN block second), groups collapse, the permission multi-select becomes a searchable accordion, and the verdict card stays pinned at the bottom of the Test tab.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/iam/policies` | List with target counts and summaries · create | `iam.policies.read` · `iam.policies.manage` |
| GET · PATCH · DELETE | `/api/v1/iam/policies/{id}` | Detail · update (writes a new version) · delete | `iam.policies.read` · `iam.policies.manage` |
| POST | `/api/v1/iam/policies/validate` | Validate a draft tree without saving | `iam.policies.manage` |
| POST | `/api/v1/iam/policies/{id}/test` | Dry run against a sample subject and resource | `iam.policies.manage` |
| GET | `/api/v1/iam/policies/{id}/versions` (+`/{version}`, `/diff?from=&to=`) | Version history, snapshot and diff | `iam.policies.read` |
| POST | `/api/v1/iam/policies/{id}/restore` | Restore a version as a new version | `iam.policies.manage` |
| GET | `/api/v1/iam/policy-attributes` | The attribute and operator registry the builder renders | `iam.policies.read` |
| GET | `/api/v1/iam/policy-decisions` | Decision log with filters and CSV export | `iam.policies.read` |
| POST | `/api/v1/iam/policies/{id}/test-cases` | Save a sample as a regression case | `iam.policies.manage` |

The guard path calls the same evaluator: candidate policies are narrowed in SQL (organization, enabled, target key) and conditions are then evaluated in Rust against the loaded attributes, so the dry run and a live request can never disagree about the semantics.

### Data model

Migration: `0122_policy_engine.sql` (reserved band 0116–0125 for the identity & access wave; the ledger is append-only — take the next free number if taken). It creates the policy store; base `policies` rows it shares with the IAM core migration (REQ-006) keep the same names — one table per concept, additive only.

- `policies` (id uuid pk, organization_id uuid → organizations on delete cascade, name text not null, description text not null default '', effect text in ('allow','deny') not null, conditions jsonb not null default '{"op":"and","children":[]}', target_permissions text[] not null, priority int not null default 500, enabled bool not null default true, self_grant bool not null default false, version int not null default 1, created_by uuid null → users on delete set null, created_at/updated_at) — unique (organization_id, lower(name)); index (organization_id, enabled, priority desc); GIN on `target_permissions`; GIN on `conditions` with `jsonb_path_ops`.
- `policy_versions` (id uuid pk, policy_id uuid → policies on delete cascade, version int, snapshot jsonb not null, diff jsonb not null default '{}', change_kind text in ('created','updated','restored','enabled','disabled'), actor_user_id uuid null, created_at) — unique (policy_id, version); index (policy_id, created_at desc).
- `policy_attributes` (key text pk, source text in ('user','resource','request','organization'), value_type text in ('string','number','bool','datetime','enum','time_range'), operators text[] not null, label text not null, description text not null default '', example_value text, enum_values text[] not null default '{}', position int not null default 100) — seeded from the evaluator's own registry; the builder renders only what exists here.
- `policy_decision_log` (id uuid pk, organization_id uuid not null, policy_id uuid null → policies on delete set null, subject_type text in ('user','service_account'), subject_id uuid null, permission_key text not null, resource_type text null, resource_id text null, outcome text in ('allow','deny'), stage text in ('rbac_deny','policy_deny','rbac_allow','policy_allow','default_deny'), reason text not null, attributes_snapshot jsonb not null default '{}' — redacted, occurred_at timestamptz not null default now()) — index (organization_id, occurred_at desc), index (policy_id, occurred_at desc); rows older than the retention window (90 days by default) are pruned by the maintenance pass.
- `policy_test_cases` (id uuid pk, policy_id uuid cascade, name text, subject_attributes jsonb not null, resource_attributes jsonb not null, expected_outcome text null, created_by uuid null, created_at) — index (policy_id).

The condition tree is stored as declared (`{"op":"and","children":[{"attr":"user.department","operator":"equals","value":"marketing"}]}`); nothing in it is evaluated as code, and `attributes_snapshot` keeps keys and verdict-relevant values only — never free-text profile content.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `iam.policy_created` · `iam.policy_updated` · `iam.policy_deleted` | Lifecycle (update = new version) | `policy_id`, `effect`, `target_permissions`, changed node count |
| `iam.policy_enabled` · `iam.policy_disabled` | Toggle | `policy_id`, `actor_user_id` |
| `iam.policy_version_created` · `iam.policy_restored` | Versioning | `policy_id`, `version`, `restored_from` |
| `iam.policy_denied` | A state-changing call was refused by a policy | `policy_id`, `permission_key`, `resource_type`, `stage` |
| `iam.policy_allowed` | A policy allow granted where roles alone did not | `policy_id`, `permission_key`, `subject_id` |
| `iam.policy_test_run` | A dry run completed | `policy_id`, `outcome`, `matched_conditions` count |

Consumed: `iam.role_permissions_changed` and `user.updated` drop the effective-permission cache (REQ-068) because a resolution may now differ. Webhook relevance: the security centre (REQ-012) subscribes to `iam.policy_denied`; sample it at volume and keep payloads to ids, keys and stages — never attribute values or profiles.

### Acceptance criteria

- [ ] `0122` applies on a fresh and on a populated database; constraints and indexes land; `cargo test --workspace` is green with evaluator unit tests that need no database.
- [ ] The builder round-trips a policy: condition tree saved, reopened and re-rendered identically, including nested groups and a `NOT` branch.
- [ ] An unknown attribute, an unknown operator, a string operator on a numeric attribute, an empty target list, a duplicate name, depth > 4 and more than 32 conditions are each refused with the offending node named.
- [ ] A missing attribute compares as null and makes its condition false — never an error and never a deny by accident — and the verdict reason says so.
- [ ] A numeric comparison (`resource.amount < 10000`) is exact; comparing a numeric attribute with a non-numeric value is refused at save time, not silently false at runtime.
- [ ] A time window denies outside weekdays and hours in the organization timezone and allows inside it, including across a timezone boundary.
- [ ] A matching policy deny beats a role allow for the same key on a guard call; a matching policy allow grants where roles alone deny, and both outcomes record their stage and winning policy.
- [ ] With two matching enabled policies, the higher priority wins and a tie resolves to deny; disabling the winner changes the outcome on the next request without a restart.
- [ ] A disabled policy stops applying immediately; a restored version produces the same verdict as before its change.
- [ ] Every save writes exactly one version; the diff shows added and removed nodes; restore writes a new version.
- [ ] The dry run's verdict equals the guard's verdict for the same subject, resource and key across a scripted matrix (at least one allow, one deny, one no-match).
- [ ] A target key that is not assignable in the catalogue is refused; a dangerous key demands confirmation; `iam.policies.manage` as a target is refused unless the organization enabled self-grant.
- [ ] The decision log carries one row per state-changing refusal or policy-driven grant, with stage and reason, no attribute free text, and prunes past the retention window.
- [ ] Every policy change writes an audit entry with a field-level diff and an actor.
- [ ] All new screens render at 390 px without horizontal scroll, the builder warns before discarding a dirty draft, and the walkthrough reports zero high findings.

### QA plan

The walkthrough must visit `/settings/iam/policies` (read the list, filter by effect, open the seeded example), build a policy through the Condition and Then tabs (attribute picker, AND group, a `NOT` branch, numeric comparison, time window, target permissions), submit two invalid drafts (unknown-attribute simulation is not possible from the UI — instead assert the empty-target and depth errors) and read the field-level messages, run the Test tab against a picked subject and resource (one expected `ALLOW`, one `DENY`), save a test case, edit the policy and read the v1 → v2 tree diff, disable and re-enable it, then read `/settings/iam/policies/decisions` and confirm a row exists for a refused state-changing call. Visual check: the WHEN/AND/THEN layout reads as a sentence, matched conditions are green and unmatched grey with negations struck through, the verdict card states stage and winning policy, the tree diff uses real add/remove colouring, and screenshots `page-iam-policies`, `page-iam-policy-builder`, `page-iam-policy-test`, `page-iam-policy-versions`, `page-iam-policy-decisions`, `mobile-iam-policies` are produced.

### Slices

1. **Evaluator and pipeline.** `crates/policy-engine` with the condition tree, typed operators, null semantics, candidate prefiltering, the stage-ordered decision pipeline and unit tests that prove order without a database. *Done when:* acceptance 4–8 and 11 pass in unit tests and the guard uses the engine behind a feature flag.
2. **Store, validation and API.** Migration, policy CRUD with versions, the validation endpoint and its node-level messages, the attribute registry, the dry-run endpoint, and the enable/disable/restore paths. *Done when:* acceptance 1–3, 9–10 and 12 pass and the API answers every documented code.
3. **Builder UI.** List, Condition and Then tabs with the typed editors and grouping, inline validation, dirty-draft protection, permission multi-select, and the Test tab with the verdict card and test cases. *Done when:* acceptance 2, 11, 15 pass and the builder plus Test are in the walkthrough inventory.
4. **Decision log, audit and polish.** Decision-log screen with filters, sampling badge and export, retention pass, audit entries, cache invalidation wiring, keyboard map, mobile layout, and the security-centre event hook. *Done when:* acceptance 13–15 pass and the job's QA report shows zero high findings.

### Risks / notes

- **One decision path.** Guards, list filters and the dry run must call the same evaluator; a second implementation drifts silently and is a release blocker (shared with REQ-067/REQ-068).
- **Tie-breaks are policy.** "Higher priority wins, a tie resolves to deny" must be stated in the code, the UI and the docs identically — an undocumented tie-break becomes an incident.
- **Performance.** Prefilter candidates in SQL by organization, enabled and target key, cache compiled trees, and keep evaluation off the hot path for cached resolutions; a policy set that re-evaluates on every list row will not survive an enterprise tenant.
- **Null semantics.** Missing attributes are null and comparisons against null are false — refusing to guess is the safe reading, but the UI must say so or operators will file bugs when a condition "does nothing".
- **Time windows and timezone.** Windows evaluate in the organization timezone and DST transitions are tested; a naive local-time comparison shifts twice a year.
- **Attribute exposure.** Decision payloads and snapshots carry keys and verdict-relevant values only; profile free text, e-mail addresses and resource bodies stay out.
- **Reachability.** A policy with no matching call in 30 days is a smell, not an error — surface it as a chip and let the operator decide, never auto-disable.
- **Self-grant.** A policy that grants its own management keys is a privilege-escalation path; the opt-in setting is explicit, audited and off by default.
