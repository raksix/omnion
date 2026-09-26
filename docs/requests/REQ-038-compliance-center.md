# REQ-038 — Compliance Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core + admin UI
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

```text
Compliance
├── Audit
├── Data Retention
├── Data Export
├── Data Deletion
├── Access Logs
├── Security Policies
└── Privacy
```

Example operations:

> "Find all of this user's data in the system."

> "Export the user's data."

## Notes

- Data deletion must respect audit-log immutability and retention rules; ties into
  docs/05-VERSIONING.md §15 (config versioning) and docs/07-IAM.md §20 (safety invariants).

## Implementation spec

### Scope (in / out)

**In**

- A **data map registry**: every crate contributes a `DataSubjectSource` descriptor (module, record type, table, subject column, export serializer, erasure strategy) and the Compliance Center composes them into one searchable inventory. This is what makes "find all of this user's data" a real answer instead of a promise.
- **Retention policies** per data class (audit rows, events, webhook deliveries, media, page revisions, AI logs, form submissions, session records) with an action of `delete`, `archive` or `pseudonymize`, a dry-run mode and a per-run evidence record.
- **Subject requests** — access, export and erasure — as first-class, auditable work items with verification, an impact preview, an approval step for erasure, and a blocked-items report when a record cannot be touched.
- **Exports**: a machine-readable bundle (JSON index + original files) written to storage with a checksum, an expiry and a download audit row.
- A **policy register** for security policies (owner, review date, status, document link) and a
  **privacy** screen covering consent records, processing purposes and cookie/analytics notices.
- **Access logs**: the Compliance Center links into the audit trail (REQ-039) with a compliance-oriented preset instead of duplicating a second log.

**Out**

- Certification claims. The centre helps an operator answer an auditor; it does not certify.
- A DPO case-management product (assignments are a field, not a workflow engine).
- Third-party sub-processor tracking and contract storage.
- Deleting or rewriting audit rows: erasure pseudonymizes the subject identifier and keeps the row, always.

### Screens (UI)

- `/compliance` — overview. Cards: open subject requests, overdue requests, policies due for review, next retention run, records under retention, last export. A readiness checklist (retention policies set · subject search works · erasure approval configured · privacy notice published) with links. Below: recent activity, last 20 items, each linking to its detail.
- `/compliance/retention` — table: **Data class · Scope · Retention · Action · Enabled · Last run · Next run · Affected (last run)**. Filters: data class, scope, action, enabled. Bulk actions:
  **Enable / Disable · Set retention · Run now (dry run)**. Policy editor fields: Data class (select from the registry) · Scope (organization or site) · Retention (days, 1–3650) · Action (`delete` `archive` `pseudonymize`) · Enabled · Force flag for classes marked immutability-protected (audit rows refuse `delete` with an explanatory message).
- `/compliance/requests` — table: **Reference · Type · Subject (masked) · Received · Verified · Status · Assignee · Due · Items**. Status: `new` `verifying` `collecting` `ready` `awaiting approval` `completed` `rejected`. Filters: type, status, assignee, date range, overdue toggle. Bulk actions: **Assign · Reject** (reason required).
- `/compliance/requests/{id}` — three panes. Left: identity block (masked subject, verification method and timestamp, requested-by). Middle: **discovered data** grouped by module, columns
  **Module · Record type · Records · Matched by · Action**, with per-row include/exclude for export and erasure. Right: timeline (created, verified, collected, approved, executed) with the decision buttons. Blocked rows show a reason chip ("legal hold", "audit record — will be pseudonymized", "shared with another subject") and are never silently dropped.
- New-request wizard (`/compliance/requests/new`): **Identifier type** (`email` `user id` `external key`) · **Identifier value** · **Request type** (`access` `export` `erasure`) ·
  **Scope** (organization / site) · **Reason** (required, ≤ 500) · **Verification method** (signed-in confirmation, identity check by an administrator, e-mail confirmation) · **Due date**. Validation: identifier format per type; a search that finds nothing is a valid outcome and shows "no records found for this identifier" instead of an error; erasure requires a second approver before execution; the same identifier with an open request of the same type is refused as a duplicate.
- `/compliance/policies` — table: **Title · Category · Owner · Status · Review due · Document**. Actions: add, edit, mark reviewed (moves the review date forward), attach a document link (URL must be `https`).
- `/compliance/privacy` — consent purposes table (**Purpose · Basis · Retention · Active · Last withdrawn**), a notice editor (title + body, published/unpublished), and the cookie/analytics toggles. Withdrawals listed with a masked subject and a timestamp.
- States: skeletons while loading; empty states everywhere ("No subject requests yet" with the wizard link); a failed run shows the error text and keeps the previous evidence record; an export that has expired renders as `expired` with a re-export action.
- Keyboard: `/` search, `n` new request, `a` approve the open request when the caller may approve, `Esc` closes panes. Mobile: the three-pane detail stacks into tabs, tables become cards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/compliance/overview` | Readiness checklist and counters | `compliance.read` |
| GET | `/api/v1/compliance/data-map` | Registered data-subject sources | `compliance.read` |
| GET | `/api/v1/compliance/subjects/{hash}/data` | Find every record for one subject | `compliance.read` |
| GET | `/api/v1/compliance/subjects/{hash}/access-log` | Access history for one subject | `compliance.read` |
| GET | `/api/v1/compliance/retention/policies` | List retention policies | `compliance.read` |
| POST | `/api/v1/compliance/retention/policies` | Create a retention policy | `compliance.retention.manage` |
| PATCH | `/api/v1/compliance/retention/policies/{id}` | Edit a retention policy | `compliance.retention.manage` |
| DELETE | `/api/v1/compliance/retention/policies/{id}` | Remove a retention policy | `compliance.retention.manage` |
| POST | `/api/v1/compliance/retention/policies/{id}/run` | Run now (dry run or apply) | `compliance.retention.manage` |
| GET | `/api/v1/compliance/retention/runs` | Run history with evidence | `compliance.read` |
| GET | `/api/v1/compliance/requests` | List subject requests | `compliance.read` |
| POST | `/api/v1/compliance/requests` | Open a subject request | `compliance.requests.manage` |
| GET | `/api/v1/compliance/requests/{id}` | Request detail with items | `compliance.read` |
| POST | `/api/v1/compliance/requests/{id}/verify` | Record verification | `compliance.requests.manage` |
| POST | `/api/v1/compliance/requests/{id}/approve` | Approve an erasure or export | `compliance.requests.approve` |
| POST | `/api/v1/compliance/requests/{id}/execute` | Run the collection or erasure plan | `compliance.requests.manage` |
| POST | `/api/v1/compliance/requests/{id}/reject` | Reject with a reason | `compliance.requests.approve` |
| GET | `/api/v1/compliance/requests/{id}/export` | Download the export bundle | `compliance.read` |
| GET | `/api/v1/compliance/policies` | Security policy register | `compliance.read` |
| POST | `/api/v1/compliance/policies` | Add a policy entry | `compliance.policies.manage` |
| PATCH | `/api/v1/compliance/policies/{id}` | Edit or mark reviewed | `compliance.policies.manage` |
| GET | `/api/v1/compliance/privacy` | Purposes, notice, toggles | `compliance.read` |
| PATCH | `/api/v1/compliance/privacy` | Update notice and toggles | `compliance.policies.manage` |

Collection runs are queued and report progress through the request resource; an export URL is single-use-ish, expiring, and every download writes an audit row. Errors: `403` wrong permission, `409` duplicate open request, `422` missing verification or missing second approver, `410` expired export.

### Data model

`database/migrations/0013_compliance.sql`:

- `compliance_requests` — `id uuid pk`, `organization_id uuid not null`, `reference text not null unique`, `kind text not null check (kind in ('access','export','erasure'))`, `subject_kind text not null check (subject_kind in ('email','user','external'))`, `subject_hash text not null`, `subject_display text not null`, `scope text not null`, `reason text`, `status text not null default 'new'`, `verification_method text`, `verified_at timestamptz`, `verified_by uuid`, `assignee_user_id uuid`, `due_at timestamptz`, `approved_at timestamptz`, `approved_by uuid`, `completed_at timestamptz`, `decision_reason text`, `requested_by uuid`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`.
- `compliance_request_items` — `id bigserial pk`, `request_id uuid not null references compliance_requests(id) on delete cascade`, `module text not null`, `record_type text not null`, `record_id text not null`, `matched_by text not null`, `planned_action text not null`, `status text not null default 'pending'`, `blocked_reason text`, `executed_at timestamptz`. Unique `(request_id, module, record_type, record_id)`.
- `compliance_exports` — `id uuid pk`, `request_id uuid not null references compliance_requests(id) on delete cascade`, `storage_key text not null`, `format text not null default 'json'`, `size_bytes bigint not null default 0`, `sha256 text`, `item_count int not null default 0`, `expires_at timestamptz not null`, `downloaded_at timestamptz`, `created_by uuid`, `created_at timestamptz not null default now()`.
- `retention_policies` — `id uuid pk`, `organization_id uuid not null`, `data_class text not null`, `scope text not null default 'organization'`, `retention_days int not null check (retention_days between 1 and 3650)`, `action text not null check (action in ('delete','archive','pseudonymize'))`, `enabled boolean not null default true`, `immutable boolean not null default false`, `last_run_at timestamptz`, `next_run_at timestamptz`, `created_by uuid`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Unique `(organization_id, data_class, scope)`.
- `retention_runs` — `id bigserial pk`, `policy_id uuid not null references retention_policies(id) on delete cascade`, `dry_run boolean not null default false`, `status text not null default 'running'`, `affected_rows bigint not null default 0`, `evidence_key text`, `error text`, `started_at timestamptz not null default now()`, `finished_at timestamptz`.
- `security_policies` — `id uuid pk`, `organization_id uuid not null`, `title text not null`, `category text not null`, `owner_user_id uuid`, `status text not null default 'draft' check (status in ('draft','active','retired'))`, `document_url text`, `review_due_at timestamptz`, `last_reviewed_at timestamptz`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`.
- `consent_records` — `id bigserial pk`, `organization_id uuid not null`, `site_id uuid`, `subject_hash text not null`, `purpose text not null`, `basis text not null`, `granted boolean not null`, `source text`, `captured_at timestamptz not null default now()`, `withdrawn_at timestamptz`.

Indexes: `compliance_requests_status_idx (organization_id, status, due_at)`, `compliance_requests_subject_idx (organization_id, subject_hash)`, `compliance_request_items_request_idx (request_id)`, `retention_policies_next_run_idx (next_run_at) where enabled`, `security_policies_review_idx (review_due_at)`, `consent_records_subject_idx (subject_hash)`.

`subject_hash` is a keyed digest of the normalized identifier; the display value is masked on screen and only a caller holding `compliance.requests.manage` can see the full identifier. Erasure replaces subject identifiers in source tables with the same digest so cross-references survive pseudonymization, and audit rows are never deleted.

### Events

- `compliance.request.created` · `compliance.request.verified` · `compliance.request.approved`
- `compliance.request.completed` — `{reference, kind, items, blocked}`
- `compliance.request.rejected` · `compliance.request.failed`
- `compliance.export.ready` — `{reference, sha256, expires_at, size_bytes}`
- `compliance.retention.run.completed` — `{data_class, action, affected_rows, dry_run}`
- `compliance.retention.policy.changed`
- `compliance.consent.withdrawn` — `{purpose, subject_hash}`
- `compliance.policy.review_due`

Webhook relevance: `compliance.export.ready` and `compliance.request.completed` let an operator close the loop without polling; `compliance.consent.withdrawn` is what a marketing module subscribes to in order to stop sending. Payloads carry references and counts, never identifiers in the clear.

### Acceptance criteria

- [ ] The data map lists at least the content, media, identity, audit, events and AI modules, and `/compliance/data-map` returns the same list the search uses.
- [ ] A subject search returns per-module counts and drill-down record rows for a seeded fixture user.
- [ ] A search for an unknown identifier returns an empty result set and a friendly empty state.
- [ ] A request can be created, verified and moved through its statuses with every transition recorded in the timeline.
- [ ] Erasure refuses to execute without a second approver.
- [ ] Erasure removes or pseudonymizes the planned rows, keeps the request and its item list, and never deletes an audit row.
- [ ] Blocked items appear with a reason and cannot be silently excluded.
- [ ] The same identifier cannot have two open requests of the same type (`409`).
- [ ] An export bundle contains a JSON index plus referenced files, has a checksum, an expiry, and logs a download audit row.
- [ ] An expired export returns `410` and shows a re-export action.
- [ ] A retention policy runs in dry-run mode without changing a row and reports the count the apply run would touch.
- [ ] A retention policy marked immutable rejects `delete` and offers `pseudonymize`.
- [ ] Retention runs leave an evidence record listing the affected classes and counts.
- [ ] Security policies can be added, edited and marked reviewed, with the review date moving forward.
- [ ] Privacy toggles persist; withdrawing consent records a withdrawal and emits the event.
- [ ] `compliance.read` alone cannot create, approve, execute or delete anything (`403`).
- [ ] `cargo test --workspace` and `pnpm typecheck && pnpm build` pass; the QA walkthrough covers every new route.

### QA plan

- The browser walkthrough visits `/compliance`, `/compliance/retention`, `/compliance/requests`, `/compliance/requests/new`, a seeded request detail, `/compliance/policies` and `/compliance/privacy`; the walkthrough inventory gains those routes.
- Controls to exercise: the wizard end to end (including the "no records found" outcome), subject search, per-row include/exclude, approve and reject with reasons, execute, export download, a dry-run retention run, policy add/review, privacy toggle and consent withdrawal.
- API-level checks: erasure without an approver is `422`; the audit row count for the fixture subject is unchanged after erasure while the profile row is gone; an expired export is `410`; a permission-less caller gets `403` on every mutation.
- The visual check must see: masked subject identifiers everywhere except the authorized detail pane, blocked rows with visible reason chips, an empty state on a fresh installation, readable tables at 1280 px and card layout under 640 px.

### Slices

1. **Data map + subject search** — migration, `DataSubjectSource` registry with four real contributors, `/compliance/data-map` and `/compliance/subjects/{hash}/data`, and the overview screen's search box wired to them.
   *Done:* the fixture user's records are found across modules and rendered with real counts.
2. **Requests: access, export, erasure** — request CRUD, verification, item planning, approval, execution, export bundle, request list + detail screens.
   *Done:* the wizard runs end to end; erasure without approval is refused and audit rows survive.
3. **Retention engine** — policies, dry run, apply, run history and evidence, `/compliance/retention`.
   *Done:* a dry run reports a count and an apply run removes exactly those rows, both recorded.
4. **Policies + privacy + access-log preset** — register, consent records, privacy screen and the audit deep-link.
   *Done:* the walkthrough covers all routes with zero high findings.

### Risks / notes

- Erasure is the one irreversible operation in the platform: two-person rule, dry-run preview and a written evidence record are not optional.
- Retention must never fight audit immutability — the engine reads each class's flags and refuses an incompatible action at policy-save time rather than failing mid-run.
- Multi-tenancy: subject search and exports are organization-scoped; a cross-organization match is reported as "other organizations hold related records" without exposing them.
- Exports concentrate personal data in one file: short expiry, checksum, download audit, and the bundle lives behind the same storage permissions as media.
- A data map that drifts from the schema is worse than none — adding a table with a subject column should fail a test when it has no registered source.
- Wording matters in a public repo: describe these screens as record-keeping and privacy controls, not as legal advice.
