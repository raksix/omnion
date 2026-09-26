# REQ-120 — Multi-Site Operations

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + core (`crates/sites`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Running many sites without chaos.

- Create a site from a template (theme + content type set + menus + sample content).
- Clone an existing site (configuration and optionally content).
- Transfer a site between organizations; archive and restore a site.
- Cross-site dashboards: traffic/traffic-lite, publishing queue, incidents.
- Domain management across sites with DNS verification and renewal reminders.

## Implementation spec

### Scope (in / out)

**In**

- Extends the tenancy core: `sites` and `site_domains` already exist (`0003_tenancy.sql`: key format `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`, the `active|archived` status check, the one-primary-domain invariant). This REQ adds the operations around them.
- **Blueprints** (create from template): theme + content type set + menus + settings + sample content, created through the normal content service so revisions and slug rules behave like manual creation. Shipped blueprints (blank, corporate, blog, documentation) plus organization-private blueprints saved from a live site.
- **Jobs**: create, clone, transfer, archive, restore run asynchronously with named phases, a progress counter, resumability from the failed phase and idempotent retry; progress is polled and readable over REQ-041's stream when present.
- **Clone** copies configuration always; content, revisions and media are explicit options. Media defaults to *reference* (shared storage keys); *copy* duplicates bytes and the sheet shows a size estimate before the choice.
- **Transfer** moves a site — domains, module enablement, site-scoped role bindings — between organizations. Requires `site.transfer` on **both** sides, a typed confirmation of the target slug, audit rows and events to both organizations. No automatic reversal in v1; the job journal supports a manually requested reverse run.
- **Archive/restore**: archive stops public serving (configurable response, default `410`), keeps data and admin read access; restore returns the prior domains and public serving.
- **Cross-site dashboard**: one row per site with status, primary domain, publishing-queue counts (REQ-110), open incidents (REQ-014) and last activity. Traffic-lite is a 7/30-day rollup from REQ-007 when analytics is present; a missing producer renders a real `0` marked "not instrumented" — never a placeholder number.
- **Domains**: add and validate a host, verify ownership through a TXT record, switch primary atomically, remove a host, store the operator-entered renewal date and reminder lead, emit deduplicated renewal reminders.

**Out**

- DNS provider API calls (records are shown for copying), automatic TLS issuance (REQ-011 later; TLS state is stored read-only when an edge reports it), WHOIS/registry lookups (the renewal date is operator-entered), bulk content migration between sites, cross-site content federation, per-site billing.

### Screens (UI)

Admin app, `/sites/*` plus a settings section.

| Route | Purpose |
|---|---|
| `/sites` | Cross-site dashboard: status, domains, queue counts, incidents, last activity |
| `/sites/new` | Blueprint picker → create form (key, name, locale, theme) |
| `/sites/[key]` | Overview: status, domains, module state, running jobs, danger zone |
| `/sites/[key]/domains` | Domain table: verify, primary, renewal, remove |
| `/sites/[key]/jobs/[job]` | Job phases, progress, log tail, retry |
| `/settings/blueprints` | Blueprint list, create-from-site, delete |

- **Dashboard** filters: organization, status, verification state, "has open jobs"; URL-persisted so a filtered view is shareable; rows link to the site; long keys and hosts truncate with full-value titles.
- **Create sheet**: blueprint cards (name, description, contents, estimated sample-content count) → form validating the key against the existing format check; creation runs the create job and lands on the job view with phases `Prepare → Configure → Content → Domains → Ready`.
- **Job view**: phase list with state, progress counter (`142 / 300 items`), log tail, `Retry from failed phase` on failure; the failure message names the phase — never a generic error.
- **Transfer sheet**: exactly what moves (site, domains, module state, count of site-scoped role bindings), target selector limited to organizations where the caller holds `site.transfer`, typed confirmation of the target slug, and a plain-language warning that reversal is manual.
- **Domain table**: host, primary radio, verification state with the TXT value and copy button, TLS state (read-only, "unknown" without an edge), renewal date, reminder state, actions; verification history is a collapsed list with timestamps and observed values.
- **Archive/restore dialogs** state the public response being turned on/off and that data is retained; both write audit rows visible in the site activity.
- All screens: skeleton/empty/error with retry, `Esc` closes sheets, keyboard path with visible focus, light/dark parity, one column at 390 px with the primary action pinned.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/sites` | Sites across the caller's organizations with rollups | `site.read` |
| POST | `/api/v1/sites` | Create from blueprint `{key,name,blueprint,locale,theme}` | `site.create` |
| GET | `/api/v1/sites/{key}` | Overview and job summary | `site.read` |
| POST | `/api/v1/sites/{key}/clone` | `{key,name,include_content,media_mode}` | `site.create` |
| POST | `/api/v1/sites/{key}/archive` | Stop public serving, keep data | `site.manage` |
| POST | `/api/v1/sites/{key}/restore` | Return to active with prior domains | `site.manage` |
| POST | `/api/v1/sites/{key}/transfer` | `{target_organization_id,confirm}` | `site.transfer` |
| GET | `/api/v1/sites/{key}/jobs/{job}` | Phase progress, counters, log tail | `site.read` |
| POST | `/api/v1/sites/{key}/jobs/{job}/retry` | Resume from the failed phase, idempotent | `site.manage` |
| GET | `/api/v1/sites/{key}/domains` | Domains for one site | `site.domains.read` |
| POST | `/api/v1/sites/{key}/domains` | Add a host (format-checked, lowercased) | `site.domains.manage` |
| POST | `/api/v1/sites/{key}/domains/{id}/verify` | Run the TXT check now | `site.domains.manage` |
| POST | `/api/v1/sites/{key}/domains/{id}/primary` | Atomic primary switch | `site.domains.manage` |
| PATCH | `/api/v1/sites/{key}/domains/{id}` | Renewal date, reminder lead, note | `site.domains.manage` |
| DELETE | `/api/v1/sites/{key}/domains/{id}` | Remove a host (primary switched first) | `site.domains.manage` |
| GET | `/api/v1/sites/overview` | Dashboard rows in one query (no N+1) | `site.read` |
| GET | `/api/v1/site-blueprints` | Shipped and organization blueprints | `site.blueprints.read` |
| POST | `/api/v1/site-blueprints` | Save a blueprint from a live site | `site.blueprints.manage` |
| DELETE | `/api/v1/site-blueprints/{id}` | Delete an organization blueprint | `site.blueprints.manage` |

Errors: `400` key format, `403` permission miss, `404` another organization's site, `409` key or host collision naming the owner, `422` a transfer missing the confirmation or a permission on one side. New REQ-068 keys, category `sites`: `site.read`, `site.create`, `site.manage`, `site.transfer`, `site.domains.read`, `site.domains.manage`, `site.blueprints.read`, `site.blueprints.manage`; transfer also checks the target organization scope.

### Data model

Migration `database/migrations/00NN_multi_site_ops.sql` (next free slot at land time; additive-only). `sites` and `site_domains` are extended additively, never rebuilt.

- `site_domains` additions: `verification_token text` (random, unique — the TXT value shown for copying), `verification_state text not null default 'pending'` check in (`pending`,`verified`,`failed`,`revoked`), `verified_at timestamptz null`, `renewal_at date null`, `reminder_lead_days smallint not null default 30` check 0–365, `tls_state text null`, `last_checked_at timestamptz null`; indexes on `(verification_state)` and partial `(renewal_at) where renewal_at is not null`.
- `site_blueprints` — `id uuid pk`; `organization_id uuid null` (null = shipped, read-only); `key text not null`; `name`, `description`; `kind text not null` check in (`shipped`,`organization`); `source_site_id uuid null`; `definition jsonb not null` (theme, content types, menus, settings, sample content, locale); `created_by`, timestamps; unique `(coalesce(organization_id, '00000000-0000-0000-0000-000000000000'), lower(key))`.
- `site_jobs` — `id uuid pk`; `kind text not null` check in (`create`,`clone`,`transfer`,`archive`,`restore`); `site_id uuid not null` fk; `source_organization_id`, `target_organization_id uuid null`; `state text not null default 'queued'` check in (`queued`,`running`,`succeeded`,`failed`,`cancelled`); `phase text null`; `phases jsonb not null default '[]'`; `options jsonb not null default '{}'`; `progress_done`/`progress_total int not null default 0`; `error text null`; `requested_by`; `started_at`, `finished_at`, `created_at`; indexes `(site_id, created_at desc)` and `(state, created_at)` for the `for update skip locked` lease-claim runner.
- `site_job_events` — `id bigserial pk`; `job_id` fk cascade; `at`; `level` check in (`info`,`warn`,`error`); `phase text`; `message text not null`; index `(job_id, at)`; pruned on a schedule so a large clone cannot grow the log without bound.
- `site_domain_checks` — `id uuid pk`; `domain_id` fk cascade; `checked_at`; `result text not null` check in (`verified`,`no_record`,`wrong_value`,`dns_error`); `observed_value text null`; `note text null`; index `(domain_id, checked_at desc)`; last 50 rows kept per domain.
- Renewal reminders derive from `site_domains.renewal_at` in a daily job — no duplicate scheduled-state table; each reminder goes through the notification stack and is deduplicated per domain and threshold.

### Events

| Event | When | Payload |
|---|---|---|
| `site.created` | create job succeeds | `site_id`, `key`, `blueprint` |
| `site.cloned` | clone succeeds | `source_site_id`, `new_site_id`, `include_content`, `media_mode` |
| `site.transferred` | transfer succeeds | `site_id`, `from_organization_id`, `to_organization_id` |
| `site.archived` / `site.restored` | archive or restore succeeds | `site_id`, `key` |
| `site.job.failed` | a phase fails | `job_id`, `kind`, `phase`, `message` |
| `site.domain.added` | host added | `site_id`, `host` |
| `site.domain.verified` | TXT check passes | `site_id`, `host` |
| `site.domain.verification_failed` | check fails | `site_id`, `host`, `result` |
| `site.domain.expiring` | reminder threshold crossed | `site_id`, `host`, `renewal_at`, `days_left` |

Consumed: none strictly required. Transfer events go to both organizations' subscribers; everything else is organization-scoped with ids and hosts only. The reminder job emits `site.domain.expiring` and requests delivery through the notification stack rather than a second transport.

### Acceptance criteria

- [ ] Each shipped blueprint creates a matching site; the job log's content count equals the resulting page count.
- [ ] Clone copies configuration; with `include_content`, pages and revisions appear with the chosen media mode (reference: identical storage keys; copy: new objects, proven by count and size); without it the target has zero content.
- [ ] A clone colliding with an existing key in the target organization returns `409` naming the key and leaves the source untouched (counts before/after equal).
- [ ] A job killed mid-way lists `failed` with phase and message; retry resumes from the failed phase and two consecutive retries produce identical content counts.
- [ ] Transfer moves site, domains and site-scoped role bindings: source members lose access immediately (`404`), the target sees it, both receive `site.transferred`, and no other organization's rows changed.
- [ ] Transfer refused with `422` when `site.transfer` is missing on either organization or the typed confirmation mismatches; a refusal changes nothing.
- [ ] Archive serves the configured gone response publicly while the panel keeps read access; restore returns prior domains and serving; both write audit rows.
- [ ] Domain add validates format and lowercase; duplicates return `409` naming the owner; removing the primary domain is refused until primary is switched.
- [ ] Verify: correct TXT → `verified` + `site.domain.verified`; wrong value → `wrong_value` with the observed value; absent record → `no_record`; every attempt appears in the history.
- [ ] After any sequence of primary switches exactly one domain per site is primary (partial unique index holds) and public routing follows it.
- [ ] A domain inside its reminder window produces exactly one reminder and one `site.domain.expiring` per threshold; two runs in one day do not duplicate.
- [ ] Dashboard numbers match per-site detail: queue counts equal REQ-110's queue, incidents equal REQ-014's open list, and a missing producer renders a real `0` marked not instrumented.
- [ ] Permission matrix holds: `site.read` cannot manage; `site.domains.manage` cannot transfer; cross-organization access returns `404`.
- [ ] Jobs and domain screens reach skeleton, running, failure-with-retry and success states; light/dark parity, keyboard path and 390 px layout pass.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; every new screen is in the walkthrough inventory.

### QA plan

- Walkthrough: create from the blank blueprint, clone with content, add a domain, run verification (documented manual step without a resolver override), switch primary, archive, restore, transfer the clone to a second seeded organization and confirm both sides; kill a running clone in the dev worker and retry from the failure view.
- Visual review: dashboard alignment with long names, the job phase strip reaching `Ready` without a stuck spinner, transfer sheet legibility, the domain verification column.
- `scripts/qa/probe-sites-multi.cjs` (new, exits non-zero) asserts through the API: blueprint content counts, clone idempotency across retries, `409` key collision, source access loss after transfer, single-primary invariant, reminder dedupe.
- Honest verification note: TXT checks in CI run against a resolver override or are reported as not verified for that run — the report states which path was used.

### Slices

1. **Schema, blueprints, create job, site API** — migration, blueprint evaluation, create flow, dashboard shell. *Done when:* each shipped blueprint creates a matching site and `site.created` reaches the feed.
2. **Clone, job runner, job screens** — phases, progress, idempotent retry, failure view. *Done when:* an interrupted clone retried twice yields identical content counts.
3. **Domains: add, verify, primary, renewal** — domain table, check history, daily reminder job with dedupe. *Done when:* verify states, the primary invariant and one reminder per threshold are proven.
4. **Transfer, archive/restore, cross-site dashboard** — dual-permission transfer with audit and events, rollup columns. *Done when:* the probe moves a site across two organizations and the dashboard matches per-site detail.

### Risks / notes

- Transfer crosses an organization boundary: audit row, dual permission, typed confirmation, no automatic reverse in v1 — recoverable only by a second transfer, which the UI states before committing.
- Clone with `media_mode = copy` can duplicate large assets; the sheet shows the size estimate, the default is reference, and the job is cancellable before the copy phase.
- Blueprint definitions are validated at save time (a bad blueprint is refused with its failing field, not discovered mid-job), and content always goes through the normal content service.
- DNS verification depends on resolver visibility and propagation; `dns_error` is distinct from `no_record`, and retries never demote a `verified` domain on a transient failure.
- Renewal dates are operator-entered; the UI never claims to know a registry expiry.
- Job growth is bounded: log events are pruned, progress is counters rather than per-item rows, and long runs are polled or streamed instead of holding a request open.
