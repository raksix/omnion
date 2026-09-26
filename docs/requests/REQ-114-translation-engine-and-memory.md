# REQ-114 — Translation Engine & Translation Memory

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/translation`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Multilingual content without re-translating the world.

- Provider abstraction for machine translation (OpenAI, DeepL, Google, custom) with per-organization configuration.
- Manual translation engine as a first-class provider (human edits count as translations).
- Translation memory: reuse of previously approved strings with confidence scores.
- "Existing translation" notice when a source string changed after its translation.
- Bulk translate a page/collection into N languages; per-field target; progress and error reporting.
- Glossary per site (do-not-translate terms, preferred terms).

## Implementation spec

> **Where:** new crate `crates/translation` (core) + admin screens · **Migrations:** `0116_translation.sql`, `0117_translation_memory.sql` (reserved band 0116–0129 for the content-and-commerce wave; the ledger is append-only — take the next free number if taken) · **Extends:** REQ-020 (`translations` rows, coverage matrix, locale registry, publish path) · **Depends on:** `crates/ai-hub` + REQ-097 for provider credentials and health, `crates/secrets`/REQ-037 for credential references, `crates/events`, `crates/audit`, `crates/content` (REQ-111 diff helpers for the source-changed notice).

### Scope (in / out)

**In**

- **Provider abstraction.** One trait (`key()`, `capabilities()`, `translate_batch()`, `codes_needed()`) with adapters: OpenAI-compatible chat endpoint (reuses the organization's AI provider row when REQ-097 is installed — one credential, not two), a DeepL-style text API, a Google-style translate API and a generic `custom` HTTP adapter (method, URL, headers by reference, request and response JSON paths). Every provider is configured per organization: name, adapter, mode (test/live), base URL, model, credential by reference (never stored or echoed), default target locales, monthly character budget, retry policy, enabled toggle and a `Test connection` action that returns one sample string.
- **Manual provider.** Human editing is a first-class provider named `Manual`: any field a person saves through the REQ-020 editor is recorded as a translation with `origin = 'human'` and confidence `1.000`, and it feeds memory. It is always listed, never toggleable and never billed.
- **Translation memory.** Segment store keyed by (organization, optional site, source locale, target locale, normalized source hash) holding source and target text, origin, state (`pending`, `approved`, `rejected`), confidence, use counters and last-used timestamps. Lookup is exact-hash first, then trigram fuzzy candidates ranked by similarity (≤ 5 shown), each with a confidence score: `1.000` for an approved human segment on an identical source, `similarity` for fuzzy hits, and the adapter's own score for machine output when it reports one (otherwise a documented constant below the human tier).
- **Reuse and confidence.** Translating with `Reuse from memory` on fills a field from memory without a provider call when a candidate clears the configured confidence threshold; every reuse increments `used_count` and records which segment was used. Memory is searchable, editable, CSV importable/exportable, and rebuildable from already published translations (count preview before running).
- **Existing-translation notice.** `translations` rows gain `source_hash`, `source_revision_id`, `origin`, `confidence` and `provider_id`. When the default-locale revision changes, every locale whose stored hash differs is marked; the editor shows a notice with the changed segments and a word-level diff (REQ-111), plus `Re-translate changed`, `Keep as is` and `Mark for review`. The coverage matrix's `outdated` cell state is this same signal.
- **Bulk translation.** Pick resources (pages, posts when the blog module is installed, or a whole content type), target locales, fields (title, slug, summary, body, SEO title/description), provider and options (memory reuse + threshold, glossary enforcement, overwrite mode: `fill_missing`, `refresh_machine`, `replace_all`). The job runs as one row per resource-locale with per-item status, progress counters, character counts, errors, `Re-run failed`, cancel, and an optional auto-publish that is off by default. Results land as `draft` rows through the REQ-020 write path, so publishing keeps `content.pages.publish`.
- **Glossaries per site** (plus an optional organization-wide one): terms with a preferred translation per target locale, `do_not_translate` and `case_sensitive` flags and severity `warn` / `enforce`. Enforcement is a post-translation check that flags segments where a do-not-translate term was altered or a preferred term was ignored; every job item carries its glossary flag count and the report is readable per item.

**Out**

- Locale registry, coverage matrix states, site formats, public locale routing and `hreflang` → REQ-020, whose table and editor this request extends rather than forks.
- Review steps, assignees, approval gates and the editorial calendar → REQ-110; scheduled publishing → REQ-064/REQ-110.
- AI cost accounting and provider health dashboards → REQ-104 and REQ-097; agent memory and knowledge retrieval → REQ-102 (a translation memory is a different store and is never mixed with it).
- Translating panel UI strings or theme copy → localization bundles (REQ-020 panel language), not this engine.
- Post-editing quality review beyond glossary checks, tone and brand style rules, and human translator management.

### Screens (UI)

| Route | Screen |
|---|---|
| `/settings/translation` | Providers and defaults (adapter config, character budget, connection test) |
| `/settings/translation/memory` | Translation memory browser (search, filters, promote/reject, import/export, rebuild) |
| `/settings/translation/glossaries` · `/glossaries/{id}` | Glossary list · term editor |
| `/content/translations/bulk` | Bulk translation wizard and job monitor |
| `/content/translations` (extended) | Coverage matrix gains `Translate` and `Translate with…` bulk actions and the source-changed marker |
| `/pages/{id}/translations/{locale}` (extended) | Editor gains translate actions, memory suggestions, glossary panel and the source-changed banner |

- **Providers screen.** Table columns: Name, Adapter, Mode, Default target locales, Characters this month (against the budget), Status (`enabled` / `disabled` / `error`), Last used, Actions (`Test`, `Edit`, `Disable`). Drawer form: adapter picker, credential field showing a stored value only as `••••` with `Replace`, base URL, model, timeout seconds, retry count, target-locale defaults, monthly character budget, glossary policy (`warn` / `enforce` / `ignore`), enabled toggle. Validation: name unique per organization, base URL must parse, model required for the OpenAI-compatible adapter, budget a positive integer; `Test connection` shows a real translated sample or the provider's error verbatim.
- **Memory screen.** Search box (source or target text, debounced), filters (source locale, target locale, site, origin, state, minimum confidence, resource type), columns: Source (truncated, tooltip full), Target, Source → Target locales, Origin, State, Confidence, Used, Last used, Actions. Row drawer: both texts editable, the resource links it was used from, `Approve` / `Reject` / `Delete`. Toolbar: Import CSV (mapping + dry-run preview), Export CSV (current filter), `Rebuild from published translations` (shows the candidate count first, then progress), `Delete rejected`.
- **Glossary editor.** List: Name, Scope (`organization` / site), Terms, Target locales, Updated. Term table: Term, Target locale, Translation, Do not translate, Case sensitive, Severity, Notes. Validation: term ≤ 120 characters, unique `(term, target locale)` inside a glossary, a translation required unless `do_not_translate` is set, target locale must be installed (REQ-020). CSV import/export with a mapping preview.
- **Bulk wizard.** Step 1 resources (source picker: pages / posts, filters type, status, site, multi-select with a selected count). Step 2 target locales: one checkbox per enabled locale, each expanding to field checkboxes (Title, Slug, Summary, Body, SEO title, SEO description) so a request can translate titles only. Step 3 options: provider, memory reuse toggle with a confidence slider, glossary mode, overwrite mode, auto-publish (off by default), per-run character cap. Step 4 review: a resource × locale matrix with counts and an estimated character/cost line (rates from REQ-104 when present), then `Run`. Job monitor: overall progress bar, item table `Resource`, `Locale`, `Status` (`queued`, `running`, `done`, `failed`, `skipped`), `Memory hits`, `Glossary flags`, `Error`, with `Re-run failed`, `Cancel` and links into the editor for done items.
- **Editor additions.** A `Translate with…` menu (provider list + `Manual`) applies to the focused field or all empty fields; a memory panel lists up to five candidates with confidence badges and `Use` per suggestion; a glossary panel lists terms found in the source with their preferred translations and a violation badge; the source-changed banner names the changed revision and date, with `Show diff`, `Re-translate changed` and `Keep`.
- **States and mobile.** Real empty states (no provider configured → `Add a provider`; empty memory → an explainer that memory fills as translations are approved, linking to the wizard), skeletons on every table, error banners with a request id and retry. On mobile (<768 px) tables become cards, the wizard renders one step per screen with a sticky `Next`/`Run` bar, and the editor stacks source above target behind a toggle.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/translation/providers` | List providers with status and usage | `translation.read` |
| POST | `/api/v1/translation/providers` | Create a provider (credential by reference) | `translation.providers.manage` |
| PATCH · DELETE | `/api/v1/translation/providers/{id}` | Update, enable/disable, delete | `translation.providers.manage` |
| POST | `/api/v1/translation/providers/{id}/test` | Connection test with one sample string | `translation.providers.manage` |
| GET | `/api/v1/translation/memory` | Search segments (cursor, filters) | `translation.memory.read` |
| POST · PATCH · DELETE | `/api/v1/translation/memory` (+ `/{id}`) | Create, edit, promote/reject, delete a segment | `translation.memory.manage` |
| POST · GET | `/api/v1/translation/memory/import` · `/export` | CSV import (dry-run + commit) · CSV export | `translation.memory.manage` |
| POST | `/api/v1/translation/memory/rebuild` | Rebuild from published translations (`dry_run` first) | `translation.memory.manage` |
| GET · POST | `/api/v1/translation/glossaries` · `/{id}` | Glossary list / create, terms, CSV | `translation.glossary.manage` |
| POST | `/api/v1/translation/jobs` | Start a bulk job (resources, locales, fields, options) | `translation.translate` |
| GET | `/api/v1/translation/jobs` · `/{id}` | Job list · job detail with items and progress | `translation.read` |
| POST | `/api/v1/translation/jobs/{id}/cancel` · `/retry-failed` | Cancel a running job · requeue failed items | `translation.translate` |
| POST | `/api/v1/translation/suggest` | Memory suggestions for one source string + pair | `translation.read` |
| POST | `/api/v1/pages/{id}/translations/{locale}/translate` | Translate selected fields of one resource | `translation.translate` |

Every route is organization-scoped; a provider, segment or job of another organization answers `404`. `translation.translate` and `translation.providers.manage` are denied by default to Editor and granted to Administrator and Owner; `translation.read` is granted to Editor.

### Data model

Migrations `0116_translation.sql` (providers, jobs, glossary) and `0117_translation_memory.sql` (memory, `translations` extension) — additive, commented in the `0009` style.

```sql
translation_providers (id uuid pk, organization_id uuid not null, name text not null,
  adapter text not null check (adapter in ('openai_compatible','deepl_style','google_style','custom','manual')),
  mode text not null default 'test' check (mode in ('test','live')), enabled bool not null default false,
  base_url text, model text, secret_ref text not null default '', config jsonb not null default '{}',
  default_target_locales text[] not null default '{}', monthly_character_budget bigint not null default 0,
  characters_used_monthly bigint not null default 0, glossary_policy text not null default 'warn'
    check (glossary_policy in ('warn','enforce','ignore')), retry_count smallint not null default 2,
  timeout_seconds smallint not null default 30, last_used_at timestamptz, last_error text,
  created_at/updated_at)  unique (organization_id, name)
translation_jobs (id uuid pk, organization_id uuid not null, site_id uuid null, created_by uuid -> users,
  provider_id uuid null -> translation_providers, source_locale text not null, options jsonb not null default '{}',
  status text not null default 'queued' check (status in ('queued','running','done','failed','cancelled')),
  total_items int not null default 0, done_items int not null default 0, failed_items int not null default 0,
  characters_sent bigint not null default 0, started_at/finished_at timestamptz, created_at/updated_at)
translation_job_items (id uuid pk, job_id uuid not null -> translation_jobs on delete cascade,
  resource_type text not null, resource_id uuid not null, target_locale text not null,
  status text not null default 'queued' check (status in ('queued','running','done','failed','skipped')),
  fields text[] not null default '{}', translations_written int not null default 0,
  memory_hits int not null default 0, glossary_flags int not null default 0, error text,
  started_at/finished_at timestamptz)  unique (job_id, resource_type, resource_id, target_locale)
translation_glossaries (id uuid pk, organization_id uuid not null, site_id uuid null, name text not null,
  target_locales text[] not null default '{}', created_at/updated_at)  unique (organization_id, site_id, name)
translation_glossary_terms (id uuid pk, glossary_id uuid not null -> translation_glossaries on delete cascade,
  term text not null, target_locale text not null, translation text not null default '',
  do_not_translate bool not null default false, case_sensitive bool not null default false,
  severity text not null default 'warn' check (severity in ('warn','enforce')), notes text,
  constraint term_requires_translation check (do_not_translate or translation <> ''))
  unique (glossary_id, term, target_locale)
-- 0117: memory + the REQ-020 table extension
translation_memory (id uuid pk, organization_id uuid not null, site_id uuid null,
  source_locale text not null, target_locale text not null, source_text text not null, target_text text not null,
  source_hash text not null check (source_hash ~ '^[0-9a-f]{64}$'),
  origin text not null check (origin in ('human','machine','memory','import')),
  state text not null default 'pending' check (state in ('pending','approved','rejected')),
  confidence numeric(4,3) not null default 0 check (confidence between 0 and 1),
  glossaries text[] not null default '{}', used_count int not null default 0, last_used_at timestamptz,
  created_by uuid -> users, created_at/updated_at)
  unique (organization_id, coalesce(site_id,'00000000-…'::uuid), source_locale, target_locale, source_hash)
translations += source_hash text, source_revision_id uuid, origin text not null default 'human'
  check (origin in ('human','machine','memory','import')), confidence numeric(4,3) not null default 1,
  provider_id uuid null -> translation_providers, updated_by uuid -> users
```

Indexes: `translation_memory_lookup_idx (organization_id, source_locale, target_locale, state, used_count desc)`; a GIN trigram index on `translation_memory (source_text gin_trgm_ops)` for fuzzy candidates; `translation_jobs (organization_id, created_at desc)`; `translation_job_items (job_id, status)`; `translation_providers (organization_id, enabled)`; `translations_hash_idx (resource_type, resource_id, language, source_hash)`. Checks: `characters_used_monthly >= 0`, `done_items + failed_items <= total_items`, hashes are lowercase SHA-256 hex. `source_hash` normalises whitespace runs, trailing spaces, HTML entity spellings and empty paragraphs; it never strips punctuation or case, because that would reuse wrong segments. Exact-hash hits are deterministic; fuzzy hits are always shown with their score and are never applied automatically below the threshold. The unique key uses a sentinel UUID for organization-wide segments so PostgreSQL can enforce it.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `translation.job.queued` · `.completed` · `.cancelled` | Job lifecycle | `job_id`, `source_locale`, `target_locales`, `total_items`, `characters_sent`, `status` |
| `translation.job.item_failed` | One resource-locale item exhausts retries | `job_id`, `resource_type`, `resource_id`, `target_locale`, `error_code` |
| `translation.memory.updated` | Segment created, promoted, rejected or deleted | `origin`, `state`, `count`, `source_locale`, `target_locale` |
| `translation.glossary.updated` | Glossary or term change | `glossary_id`, `site_id`, `terms_changed` |
| `translation.provider.tested` | A connection test ran | `provider_id`, `adapter`, `ok`, `latency_ms` |

Consumed: `content.page.published` (records source hashes and bootstraps memory from published human translations), `content.translation.published` (promotes the segments that produced it to `approved`), `content.translation.deleted` (drops the resource link, keeps the segment), `localization.language.disabled` (jobs targeting that locale are cancelled, glossary rows for it flagged). Webhook relevance: `translation.job.completed` triggers downstream review tasks and notifies the requester; `translation.memory.updated` tells headless subscribers nothing changed on the public side — payloads carry ids, locale codes, counts and cost figures, never translated text and never credentials.

### Acceptance criteria

- [ ] `cargo test -p omnion-translation` is green, covering hash normalization, exact and fuzzy lookup ordering, confidence tiers, glossary checks and adapter request/response mapping against fixtures.
- [ ] Both migrations apply on a fresh and on a populated database without touching existing rows.
- [ ] A provider can be created with a credential by reference; after saving, the credential value is never returned by any endpoint (`••••` only), and `Test connection` reports success or the provider's own error text.
- [ ] The manual provider is always listed, cannot be disabled or deleted, and a human edit writes `origin = 'human'`, `confidence = 1.000` and a usable memory segment.
- [ ] Translating one field with an exact approved memory segment makes no provider call (proven by provider invocation count) and increments `used_count`.
- [ ] A fuzzy candidate is displayed with its confidence; below the threshold it is never applied without an explicit `Use`.
- [ ] A bulk job over 3 resources × 2 locales × 2 fields produces 6 items, writes 12 drafts through the REQ-020 path, and its progress and character counters settle to the documented totals.
- [ ] An item that fails (provider timeout) retries per policy, ends `failed` with a readable error, emits `translation.job.item_failed`, and `Re-run failed` succeeds after the provider is fixed.
- [ ] Editing the default-locale revision flips affected locales to the source-changed notice with a word-level diff, and `Re-translate changed` rewrites only the changed segments.
- [ ] A glossary with `do_not_translate` and an `enforce` preferred term flags a violating item (glossary flag count > 0) and records the report; a clean item shows zero flags.
- [ ] CSV export of a filtered memory view round-trips through CSV import with the mapping preview, and the rebuild from published translations reports the same candidate count it inserted.
- [ ] Memory search, provider list and job list are all cursor-paginated and filterable, and none of them loads a full table.
- [ ] Every mutation writes an audit entry with actor, before/after and request id, and cross-organization ids answer `404`.
- [ ] All four screens plus the editor additions render real states (empty, loading, error) with zero high findings, including mobile at 390 px.
- [ ] The public site is unchanged by any of this: without a publish action in REQ-020, no translated content is visible.

### QA plan

The walkthrough extends `scripts/qa/walkthrough.cjs` with `/settings/translation` (create a test provider with a fixture credential by reference, run `Test connection`, then add a manual-only provider while the machine one stays disabled), `/settings/translation/glossaries` (create a glossary with one `do_not_translate` term and one `enforce` preferred term, import a two-row CSV, verify the invalid row is reported), `/content/translations/bulk` (run a job over two pages × two locales for all fields with memory reuse on, watch progress, cancel a second job mid-run and confirm its status), then `/content/translations` (the matrix shows `draft` cells with the source-changed marker after the source is edited) and `/pages/{id}/translations/{locale}` (memory suggestions appear with confidences, `Use` fills the field, the glossary panel names the found terms, and the source-changed banner shows a real diff). The visual check must see: a provider drawer rendering `••••` and never a credential, a memory table with readable confidence badges, a job monitor with a real progress bar and per-item states, a diff with highlighted words (not a plain text blob), and no screen showing raw keys or placeholder text. Screenshots: `page-translation-providers`, `page-translation-memory`, `page-translation-job`, `page-translation-editor`.

### Slices

1. **Providers, manual engine and memory core.** `crates/translation` with the provider trait, the OpenAI-compatible/DeepL-style/Google-style/custom adapters, credential-by-reference handling, character budgets, the manual provider, both migrations, memory store with exact and fuzzy lookup, `/settings/translation` and `/settings/translation/memory` with import/export and rebuild, plus `POST /api/v1/translation/suggest`. *Done when:* acceptance 1–6 and 11–12 pass, a provider can be configured and tested, and an edit seeds memory.
2. **Bulk translation.** Jobs and items, the wizard and job monitor, the translate endpoint on one resource, per-field targets, retry/cancel, the REQ-020 draft write path, provider error surfacing and the `translation.job.*` events. *Done when:* acceptance 7–8 and 13 pass and a whole page set is translated into two locales from one wizard run.
3. **Source tracking, glossary and editor integration.** `source_hash`/`source_revision_id` on `translations`, the source-changed notice with the REQ-111 diff, glossary enforcement and reports, the memory suggestions panel, coverage-matrix bulk actions and the events wiring. *Done when:* acceptance 9–10 and 14–15 pass, and the QA walkthrough reports zero high findings for the wave.

### Risks / notes

- **Cost control is a product feature, not an afterthought.** Every job shows an estimated character count before running, every provider has a monthly budget that stops jobs at the cap with a clear message, and a job without an estimate is refused rather than started blind.
- **Memory poisoning is the failure mode to guard.** Machine output enters memory as `pending`, is never auto-applied at full confidence, and only approved human segments reach the human tier; fuzzy match is display-only until clicked.
- **Hashing discipline.** The normaliser must remove only whitespace and markup noise — stripping punctuation or case would "reuse" segments that are subtly wrong, which is worse than a cache miss. The normaliser ships with fixture tests and a version tag stored on the segment so a future change can invalidate safely.
- **Segmentation granularity.** A segment is one field, so a long body is one large segment; per-block segmentation and block-level memory wait for the block editor (REQ-063) and must land as a stored `segment_key`, not a second memory table.
- **Publishing stays with REQ-020.** This engine never publishes content by itself; the optional auto-publish flag delegates to the existing publish path and permission, so editorial gates (REQ-110) keep working.
- **Credentials never leave the reference.** The adapter reads the secret at call time, logs never contain request headers or full bodies, and error text is passed through a redaction filter before it reaches the UI or the audit trail.
- **Worker idempotency.** A job item claims its row atomically (`for update skip locked`), writes translations for that resource-locale in one transaction and marks the item done only after commit, so a restart neither double-writes nor loses a locale.
- **Unbounded memory growth.** Memory is capped per organization with an oldest-rejected-first sweep and an explicit retention setting; the rebuild action reports both candidates and the rows it would delete.
- **Locale codes are validated against the installed languages** at every entry point, so a job cannot target a locale the site cannot render.
