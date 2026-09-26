# REQ-065 — Identity Providers & SSO

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`, `crates/auth`) + admin
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

- [ ] `0116`/`0117` apply on a fresh and on a populated database; constraints and indexes land as specified; `cargo test --workspace` is green.
- [ ] An OIDC provider is created against the test identity provider fixture, `Test connection` passes step by step, and the provider can only be enabled after a passing test.
- [ ] An LDAP/AD provider binds with a service account, searches the configured base and resolves nested groups to the depth cap; a wrong bind DN produces a field-level error naming the bind step.
- [ ] Discovery or metadata validation refuses a wrong issuer or an invalid certificate naming the failed check, then succeeds against the fixture.
- [ ] A start → callback round trip signs in a user that exists in the fixture; the failure page for an unknown user is indistinguishable from a wrong-password failure.
- [ ] JIT provisioning creates the user with only the mapped attributes; a missing required attribute refuses the login naming the field instead of creating a half account.
- [ ] Role rules apply first-match-wins; the dry run predicts the same role and scope that a real login assigns for the same sample identity, and the user's audit shows `role via rule #N`.
- [ ] An account disabled in the directory is refused at the next sync and its active sessions are revoked.
- [ ] Local sign-in still works while an enabled SSO provider is misconfigured — a provider failure never locks local accounts out.
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
