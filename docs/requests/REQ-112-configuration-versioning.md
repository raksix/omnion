# REQ-112 — Configuration Versioning & Restore

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Treating settings like content.

- Versioned critical settings (site settings, theme settings, feature flags, integrations, automation rules).
- Every change appends a version with author, diff and reason.
- Restore a previous configuration version; compare two versions.
- Export/import a configuration bundle (per site or whole installation).
- Audit trail and optional approval gate for high-risk settings.

## Implementation spec

### Scope (in / out)

**In**
- Versioned domains, each with its own payload shape and write path: `site_settings`, `theme_settings`, `feature_flags`, `integrations`, `automation_rules`. A domain is registered once with its scope kind (installation, organisation, or site), its schema version, and its redaction rules.
- Every accepted change appends one immutable version: scope, domain, monotonic `version_no` per scope and domain, the full payload snapshot, a computed change set (REQ-111), a mandatory reason (short free text), the author (user, or a labelled system source such as an import, a restore or an automation), and a checksum of the canonical payload.
- Domain writes route through the version recorder. There is exactly one mutation entry point per domain; a direct write that bypasses the recorder is a defect, not a variant, because it produces state with no history.
- Restore: pick a version, see a dry-run change set of what the restore would change against the current head, then restore. A restore writes through each domain's own write path (so caches invalidate, flags propagate and integrations reconnect exactly as a normal save would), and it appends a new version that references the restored source — history is linear and append-only, never rewritten.
- Compare: any two versions of the same domain and scope, side by side, in change set form, including version-to-current.
- Export bundle: one JSON document with a header (format version, created at, installation and site identity, the version ids included, a redaction statement), one section per domain with its payload and version number, and a checksum over the canonical serialisation. Scope is one site or the whole installation (all sites plus organisation and installation domains).
- Import: three phases in one call — validate (schema version, unknown domains, scope mismatch, checksum, redacted fields), preview (per-domain change set against the current head), apply (through the recorder, so imported state is versioned like everything else). Import modes: `merge` (apply the bundle's values over current state) and `replace` (only for domains present in the bundle; refused unless the operator confirms). Import is transactional per domain with a report of every domain's outcome.
- Approval gate: a domain can be marked high-risk (default for `feature_flags` and `integrations`). A change to a high-risk domain opens a request through the approval engine (REQ-059) instead of applying; on approval the stored change applies and the version records the approval reference. Restores of a high-risk domain go through the same gate.
- Audit trail: every version row is the trail; the panel's audit surface links to the version diff, and the version detail shows the reason and author. Retention is configurable (default: keep all versions for high-risk domains, 180 days for the rest) with a pruning job that never deletes the current head or any version referenced by an approval.
- Redaction: secret material is never inside a versioned payload. Integration and similar domains store secret references (an id plus the key name), and export replaces references with a placeholder plus a required re-binding note; import refuses a bundle whose placeholder is bound silently and lists the re-binding steps.

**Out**
- Content revisions — pages already own an append-only revision model; this REQ does not duplicate it and does not version page bodies.
- Secret values, key material and connection credentials (REQ-125 owns them; only references are versioned).
- Database schema migrations (REQ-129) and code deployments (REQ-024): a version records configuration, never executable changes.
- Media files: a bundle references media ids and never embeds binaries.
- The approval engine internals (REQ-059): requests are opened and awaited, chains are not reimplemented.
- Unregistered domains: a setting surface that has not been given a domain registration, payload schema and redaction rules is not versioned by magic.

### Screens (UI)

| Route | Screen |
|---|---|
| `/settings/versions` | Version list across domains with filters |
| `/settings/versions/<id>` | Version detail: payload, change set, reason, author, `Restore this version` |
| `/settings/versions/compare` | Two-version comparison (query: `domain`, `scope`, `a`, `b`, or `b=current`) |
| `/settings/versions/export` | Bundle export wizard (scope, domains, redaction statement) |
| `/settings/versions/import` | Bundle import: validate → preview → apply |
| Domain screens (History tab) | Site settings, theme settings, feature flags, integrations, automation rules |

- **Version list.** Columns: Version, Domain, Scope (site name or `Installation`), Author, Source (`panel`, `api`, `system`, `import`, `restore`, `automation`), Reason (truncated with a tooltip), Changes (count badge), Created. Filters: domain, scope, author, source, date range, and a `High-risk only` toggle. Row actions: Open, Compare with current, Restore (disabled without the restore permission, and disabled with a reason for a high-risk domain that has already been gated this cycle).
- **Version detail.** Header card: domain, scope, version number, author, source, timestamp, approval reference when gated, checksum with a copy action. Body: the change set rendered by the shared table (REQ-111), grouped per top-level key with an expander for nested values, plus a `Payload` tab showing the snapshot with secret references masked as `••• (reference)`. Footer: `Compare with previous`, `Compare with current`, `Restore this version…`.
- **Restore flow.** A confirmation dialog states the domain and scope, shows the dry-run change set (what will change relative to the current head), requires a reason, and — for a high-risk domain — explains that the change will wait for approval and shows the approver role. After a successful restore the version list shows the new version at the top with `restored from v<N>`.
- **Compare.** Two-column change set with a swap control, `vs current` shortcut, and a summary strip (`12 settings changed, 2 added, 1 removed`). Domains never mix in one comparison: the screen refuses a pair from different domains or scopes with a clear message.
- **Export wizard.** Step 1 scope (installation or one site), step 2 domain checkboxes with per-domain version numbers shown, step 3 review with the redaction statement and a `What is not included` list (secret values, media files, content), step 4 download. The download name carries the format version and date; a copy of the header goes to the audit trail.
- **Import.** Drop a bundle or paste it; validation results render as a per-domain table (Compatible, Unknown domain, Schema mismatch, Checksum failed) before anything applies. `Preview changes` shows the per-domain change set; `Apply` is enabled only when validation passed, lists every domain's outcome afterwards, and refuses a `replace` bundle without the explicit confirmation checkbox.
- **States and keys.** Empty history (a domain that has never changed) shows "No versions yet — the first save creates v1". Keys: `g v` versions, `c` compare with current, `r` restore, `j`/`k` rows, `Esc` closes dialogs. Mobile: list becomes cards, the compare view stacks with a diff toggle.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/config/versions` | List versions (domain, scope, author, source, date filters) | `config.versions.read` |
| GET | `/api/v1/config/versions/{id}` | Version detail with change set and redacted payload | `config.versions.read` |
| GET | `/api/v1/config/versions/diff` | Compare two versions or a version with current (`a`, `b`) | `config.versions.read` |
| POST | `/api/v1/config/versions/{id}/restore` | Dry-run or apply a restore (reason required) | `config.versions.restore` |
| GET | `/api/v1/config/domains` | Registered domains with scope kind, schema version and risk class | `config.versions.read` |
| POST | `/api/v1/config/export` | Build a bundle for a scope and domain set | `config.export` |
| POST | `/api/v1/config/import/validate` | Validate a bundle without applying | `config.import` |
| POST | `/api/v1/config/import` | Preview or apply a bundle (`mode`, `phase`) | `config.import` |
| GET | `/api/v1/config/imports/{id}` | Import report: per-domain outcome and errors | `config.import` |
| GET · PUT | `/api/v1/config/domains/{domain}/head` | Read the current head · write it through the recorder (the one mutation path) | `config.read` · `config.write` |
| POST | `/api/v1/config/approvals/{id}/decide` | Apply or refuse a gated change | `config.approvals.decide` |

The head endpoint is the single write path every domain screen uses; it takes the payload plus a reason and returns the new version. High-risk domains return an `approval_pending` body instead of a new version, and the change applies only after the decision arrives.

### Data model

Migration: `0118_configuration_versions.sql` (next free number; append-only ledger — shift up if taken).

```sql
config_domains (domain text pk, scope_kind text in ('installation','organization','site'), schema_version int default 1,
  risk_class text in ('normal','high') default 'normal', retention_days int default 180, registered_at timestamptz)
config_versions (id uuid pk, organization_id uuid, site_id uuid null -> sites, domain text -> config_domains,
  version_no int, payload jsonb not null, change_set jsonb not null, reason text not null, author_user_id uuid null -> users,
  source text in ('panel','api','system','import','restore','automation') default 'panel', restored_from_id uuid null -> config_versions,
  approval_request_id uuid null, checksum text not null, created_at timestamptz default now())
  unique (organization_id, coalesce(site_id, '00000000-0000-0000-0000-000000000000'::uuid), domain, version_no)
  index (domain, site_id, created_at desc)  index (author_user_id, created_at desc)
config_heads (organization_id uuid, site_id uuid null, domain text, version_id uuid -> config_versions,
  updated_at timestamptz, primary key (organization_id, coalesce(site_id, zero-uuid), domain))
config_import_jobs (id uuid pk, organization_id uuid, mode text in ('merge','replace'), scope text in ('installation','site'),
  site_id uuid null, bundle_format text, checksum text, status text in ('validated','applied','failed','partial') default 'validated',
  report jsonb default '{}', created_by uuid -> users, created_at, applied_at timestamptz null)
config_prunes (id uuid pk, domain text, cutoff timestamptz, versions_deleted int, run_at timestamptz)
```

Notes. `version_no` is allocated under the head row with a row lock so two concurrent edits cannot share a number; the loser retries and gets the next one. `checksum` is a hash over the canonical, sorted payload serialisation, and it is what import validation compares against. Payloads store references where secrets would sit (`{"secret_ref": "<id>", "key_name": "<name>"}`) and the API masks them on read; a validator refuses a payload that looks like inline credential material. Pruning skips heads, approval-referenced versions and the newest version per domain. Bundle format: `config-bundle@1` with header, `domains[]`, and per-domain `schema_version`, `version_no`, `payload`, `checksum`.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `config.version.recorded` | Any accepted change, including imports and restores | `domain`, `scope`, `version_no`, `source`, `actor_user_id` |
| `config.version.restored` | A restore applied | `domain`, `from_version`, `to_version_id`, `changed_keys` |
| `config.export.created` | A bundle built | `scope`, `domains`, `bundle_format`, `actor_user_id` |
| `config.import.validated` · `.applied` · `.failed` | Import lifecycle | `job_id`, `mode`, `domains`, `outcome_counts` |
| `config.approval.requested` · `.decided` | High-risk change gated or released | `domain`, `version_id`, `decision` |

Consumed: `approvals.request.decided` (applies or discards a pending high-risk change), `feature_flags.flag.changed` (accepted as an input when flags change outside the recorder, and used to repair history rather than to justify bypassing it). Webhook relevance: `config.version.recorded` and `config.import.applied` are the two events external tooling needs; payloads carry domain names, version numbers and key names, never values and never secret references.

### Acceptance criteria

- [ ] Changing one setting in a versioned domain creates exactly one new version with the caller's reason, the correct author and source `panel`, and the change set lists only the keys that changed.
- [ ] A write without a reason is refused with `422` and the field-level error names `reason`; a reason shorter than the documented minimum is refused too.
- [ ] Two concurrent saves on the same domain produce two versions with sequential `version_no` values, the loser never overwrites the winner, and the change set of the second is computed against the first.
- [ ] Version detail renders the change set with added/removed/changed badges, and the payload tab masks secret references as `••• (reference)` with the reference id visible only to a caller holding the domain write permission.
- [ ] Comparing v3 with v5 and v3 with current both work, and comparing versions from different domains or scopes is refused with a clear message.
- [ ] A restore dry-run lists exactly the keys that will change; applying it writes a new version at the top with `restored from v<N>` and the target state matches the dry-run.
- [ ] A restore of a feature-flag domain flips the flags in the running system within the documented propagation window and records one `config.version.restored` event with the flag keys.
- [ ] A change to a high-risk domain does not apply immediately: it returns `approval_pending`, appears in the approvals surface, and applies only after the approver decides; a refusal leaves the head unchanged and records the decision.
- [ ] A bundle exported for one site contains only that site's domains, its checksum validates on re-import into a different installation, and the header's redaction statement is present.
- [ ] An installation-scope export includes organisation and installation domains and never any site content, media files or secret values; a grep over the bundle finds no credential-shaped field.
- [ ] Import validates a well-formed bundle as compatible, and rejects — before applying anything — a bundle with a failed checksum, an unknown domain, or a newer schema version, each with a per-domain reason.
- [ ] Import preview shows a per-domain change set, and `merge` apply writes versions through the recorder: the version list of the importing installation shows `source = import` entries.
- [ ] A `replace` import without the explicit confirmation is refused; with it, only the domains present in the bundle are replaced and every other domain is untouched.
- [ ] A restored or imported payload that would contain inline credential material is refused by the validator with a field path, and nothing is written.
- [ ] Pruning deletes no head, no approval-referenced version and no newest version; a pruned version is gone from the list and a linked audit entry states the prune.
- [ ] All new screens render at 390 px without horizontal scroll and the walkthrough reports zero high findings.

### QA plan

The walkthrough visits a site's settings screen (change two settings with a reason, open the History tab), `/settings/versions` (filter by domain and source, open the new version, compare with the previous and with current), restore (dry-run, apply, confirm the new version references the source and that the site settings actually changed), feature flags on a high-risk domain (attempt a change, watch it wait for approval, approve, confirm the flags flipped), `/settings/versions/export` (build a site bundle and an installation bundle, inspect the header and the redaction statement), and `/settings/versions/import` (validate the site bundle in a scratch site, inspect the preview, apply as `merge`, read the per-domain report). Ops checks: submit a no-reason write, submit two concurrent writes, attempt a bundle with a tampered checksum, attempt a payload with an inline secret-shaped value, and run the prune job with a short retention and confirm heads survive. Visual check: the version list shows real change counts, the compare view shows two-column changes, the restore dialog shows the dry-run, and import validation renders a per-domain table.

### Slices

1. **Recorder, versions and list.** Migration `0118_configuration_versions.sql`; domain registry, head endpoint as the single write path, version allocation under lock, checksum, version list and detail with the shared change set renderer, reason enforcement, masking of secret references. *Done when:* acceptance 1–4 and 15 pass and `/settings/versions` is in the walkthrough inventory.
2. **Compare, restore and the approval gate.** Two-version and version-to-current comparison; restore with dry-run, linear history and event; high-risk domains gated through the approval engine with `approval_pending` handling; audit links. *Done when:* acceptance 5–8 pass and a real flag flip from a restore plus one gated change are recorded in the QA environment.
3. **Export, import and pruning.** `config-bundle@1` export for both scopes with redaction; validate → preview → apply import with mode handling, per-domain reports and recorder integration; schema-version compatibility checks; the prune job with its safety rules. *Done when:* acceptance 9–14, 16 pass, a bundle round-trips between two installations, and the QA report lists zero high findings for the wave.

### Risks / notes

- One write path. The whole REQ fails if any domain screen keeps writing settings directly; the reviewer's test is "open every versioned screen, change one value, confirm a version row appeared". Domains without a version row after a save are bugs.
- Secrets never travel. Versioned payloads hold references only, export masks them, import lists re-binding steps, and the validator refuses credential-shaped values. A leaked export is a credential incident, not a config incident.
- Import is the sharpest tool here. Checksum first, validation before preview, preview before apply, `replace` behind an explicit confirmation, and a per-domain report even on partial success — a half-applied bundle with no report is unacceptable.
- Restore must go through the domain's own write path so side effects (cache invalidation, flag propagation, integration reconnection) actually happen; writing the row directly would produce a version that lies about the running system.
- High-risk gating creates a window where a version is created but not applied. The head stays unchanged, the version explains its state, and the UI must never show a gated change as live.
- Payload schemas drift as the product ships. `schema_version` per domain plus a documented compatibility rule (read old, write new, migrate on write) keeps imports working across releases; an unknown newer version is refused, never guessed.
- Retention defaults favour high-risk domains. Pruning deletes whole rows only, skips heads, approval-referenced versions and the newest version per domain, and records a `config_prunes` row so an emptied history is explainable rather than mysterious.
- Version authors can be systems. `source` is displayed everywhere, and automation-authored versions carry the automation's name, so nobody reads a system change as a person's decision.
- The audit trail is the version table plus the audit store's own entry for restores, exports and imports; if only one of the two exists for an action, the trail is incomplete and the QA step fails.
