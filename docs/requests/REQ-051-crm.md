# REQ-051 — CRM

> **Status:** in-progress — the **error state** of the empty/loading/error box is in (`63512c5` the request id, `c0e51fe` one `ErrorState` for the six screens), and the tenant half was in `b899f7f`. A screen whose read failed now shows a sentence, the machine code, the **request id the API named** and a retry that recovers — and nothing invents an id for a failure that never reached the server. What is left of the box is the empty state on the two places that still draw a bare paragraph instead of `EmptyState`: the board's per-column body and the activities filter bar. Then the 390×844 pass and the keyboard sheet. · `cargo test -p omnion-api --lib request_id` **10/10** · `cargo test -p omnion-module-crm --lib` **172/172** · `pnpm turbo run typecheck --force` **2/2** · **Captured:** 2026-09-26 · **Layer:** module (`modules/crm`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Relationship layer of the platform: people and companies, the deals they are part of, and the activity trail around them.

- **Contacts** (people) and **Companies** with custom fields, tags, owners, and merge/dedupe.
- **Deals** with stages, amount, probability, expected close date, owner, and a **pipeline board** (drag between stages, per-stage totals).
- **Activities** (call, meeting, note, task) attached to contacts/deals, with a timeline view.
- **Lists & filters**: saved views ("my open deals", "stale this month"), column chooser, inline edit.
- **Import/export** of contacts (CSV) with mapping preview.
- **Permissions**: per-role visibility (own / team / all), field-level hiding for sensitive fields.
- **Automation hooks**: events (`crm.contact.created`, `crm.deal.stage_changed`) feeding the workflow engine.
- **AI copilot**: summarize a deal, draft a follow-up mail, suggest next action (uses AI Hub).

## Implementation spec

> **Module:** `modules/crm` (crate `omnion-module-crm`, workspace member) · **Migration:** `database/migrations/0011_crm.sql` (next free slot at build time) · **Admin routes:** `/crm/*` · **Permission family:** `crm.*` · **Depends on:** core crates (`identity`, `permissions`, `audit`, `events`, `workflows`, `ai-hub`, `media`).

### Scope (in / out)

**In**

- Companies and contacts with owner, tags, lifecycle status (`lead`, `customer`, `partner`), notes, custom field values.
- Deals on one pipeline per organization (editable stages), amount + currency, probability, expected close date, source, lost reason.
- Pipeline board (kanban) with drag between stages, per-stage count/total, weighted forecast for the open column set.
- Activities (call, meeting, note, task) attached to a company/contact/deal, shown as one merged timeline per record.
- Saved views per user (filters + columns + sort), column chooser, inline edit of single cells, CSV import (map → validate → preview → commit) and CSV export.
- Visibility levels per permission binding: `own` / `team` / `all`; field-level hiding of flagged fields (e.g. contract value note) for roles without `crm.fields.sensitive.read`.
- Events for every meaningful transition, feeding automations (REQ-003) and webhook subscribers (REQ-016).
- CRM copilot (REQ-042): summarize a deal, draft a follow-up, suggest the next action — read-only suggestions, nothing is written without confirmation.

**Out (tracked elsewhere)**

- Quotes, orders, invoices → REQ-052, REQ-054. Tickets on a contact → REQ-009 (deep link only). Marketing segments → REQ-060.
- Custom-field *definitions* → REQ-026; CRM reads field defs when the dynamic model is present and otherwise stores flat `custom jsonb` values.
- Search infrastructure → REQ-002 (CRM only registers index + palette sources). PDF/document output → REQ-029.

### Screens (UI)

Module nav sits under the app shell as a second-level tab bar: **Overview · Contacts · Companies · Deals · Activities · Settings**.

| Route | Screen |
|---|---|
| `/crm` | Overview: my open deals, deals closing this month, stale deals, recent activities, funnel by stage |
| `/crm/contacts` | Contact list (table) |
| `/crm/contacts/new`, `/crm/contacts/{id}` | Contact create form / detail with timeline and inline edit |
| `/crm/companies`, `/crm/companies/new`, `/crm/companies/{id}` | Company list / create / detail (contacts, deals, activities tabs) |
| `/crm/deals` | Pipeline board + board/list toggle |
| `/crm/deals/new`, `/crm/deals/{id}` | Deal create / detail with stage stepper and timeline |
| `/crm/activities` | Activity feed (all records the caller may see) |
| `/crm/settings/pipelines` | Pipeline + stage editor with drag reorder |
| `/crm/settings/fields` | Field visibility: which roles see flagged fields (thin view over REQ-026 defs) |

**Contacts list** — columns: `Name` (link, avatar initials), `Company`, `E-mail`, `Phone`, `Owner`, `Status` (badge), `Tags` (chips), `Last activity`, `Updated`. Filters: search (name/e-mail/company, debounced 250 ms), owner (me / team / all / pick user), status, tag, created/updated range, "no activity for N days". Sort by any column, saved as a view. Row actions: open, edit (drawer), log activity, merge, archive. Bulk actions: assign owner, add tag, remove tag, archive, export selected, merge two selected. Column chooser persists per view. Inline edit: `Owner` (select), `Status` (select), `Tags` (chip input) with optimistic save and rollback + toast on 422/403. Shortcuts: `j`/`k` row move, `enter` open, `e` edit, `x` select, `/` focus search, `g c` contacts, `g d` deals, `?` shortcut sheet. States: skeleton rows (8) on load, empty state with "Create contact" + "Import CSV" (and a "Create from scratch" hint when the list is filtered to zero), error state with retry button and request id.

**Contact form** — fields: First name (required, ≤80), Last name (≤80), E-mail (format regex, unique per organization case-insensitive, optional but warns when both phone and e-mail are empty), Phone (E.164-lite `^\+?[0-9 ()-]{7,20}$`), Company (combobox, create-inline), Job title, Owner (default: caller), Status, Tags (≤10, each ≤32), Notes (≤4000), Custom fields (from REQ-026 defs). Validation messages render under the field; the form blocks submit on error and focuses the first invalid input. Detail tabs: Overview · Timeline · Deals · Files (media picker, REQ-010) · Notes.

**Companies** — list columns: `Name`, `Domain`, `Industry`, `Owner`, `Contacts` (count), `Open deals`, `Pipeline value`, `Updated`. Company detail aggregates its contacts, deals and activities (read-only rollups). Same filter/bulk/shortcut model as contacts.

**Deals board** — column per stage with header (name, count, sum, weighted sum), card shows title, company, owner avatar, amount + currency, expected close date, days-in-stage (amber > 30d). Drag a card between stages (keyboard alternative: focus card, `ctrl/cmd + ←/→`); stage kinds `open`/`won`/`lost`; moving to `lost` opens a required lost-reason dialog; moving to `won` opens a confirm with the close date. Board reads stages + deals in one request; drag writes optimistically with rollback on failure. List toggle reuses the table pattern with the same columns contract. Mobile (<768 px): board becomes one horizontally scrollable column strip with sticky stage headers (no clipped totals), list mode defaults.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/crm/contacts` | List contacts (cursor paging, filters, sort, view id) | `crm.contacts.read` |
| POST | `/api/v1/crm/contacts` | Create contact | `crm.contacts.create` |
| GET | `/api/v1/crm/contacts/{id}` | Contact detail + timeline page | `crm.contacts.read` |
| PATCH | `/api/v1/crm/contacts/{id}` | Update (inline edit included) | `crm.contacts.update` |
| DELETE | `/api/v1/crm/contacts/{id}` | Archive contact (soft, `archived_at`) | `crm.contacts.delete` |
| POST | `/api/v1/crm/contacts/merge` | Merge two contacts into one (body: survivor, loser) | `crm.contacts.merge` |
| POST | `/api/v1/crm/contacts/import` | CSV import: `mode=dry_run\|commit`, returns mapping + row errors | `crm.contacts.import` |
| GET | `/api/v1/crm/contacts/export` | CSV export of the current filter set | `crm.contacts.export` |
| GET/POST | `/api/v1/crm/companies` | List / create companies | `crm.companies.read` / `.create` |
| GET/PATCH/DELETE | `/api/v1/crm/companies/{id}` | Company detail / update / archive | `crm.companies.read` / `.update` / `.delete` |
| GET | `/api/v1/crm/deals` | Deals list + board payload (`?pipeline=&view=board\|list`) | `crm.deals.read` |
| POST | `/api/v1/crm/deals` | Create deal | `crm.deals.create` |
| GET/PATCH/DELETE | `/api/v1/crm/deals/{id}` | Deal detail / update / archive | `crm.deals.read` / `.update` / `.delete` |
| POST | `/api/v1/crm/deals/{id}/stage` | Move deal to a stage (drag) | `crm.deals.update` |
| GET/POST | `/api/v1/crm/activities` | Timeline feed / log an activity | `crm.activities.read` / `.create` |
| GET/POST | `/api/v1/crm/pipelines` | Pipelines + stages read / replace stages | `crm.deals.read` / `crm.pipelines.manage` |
| GET/POST/DELETE | `/api/v1/crm/views` | Saved views of the caller | `crm.views.manage` |
| POST | `/api/v1/crm/copilot/summarize` | Deal summary + suggested next action | `crm.copilot.use` |
| POST | `/api/v1/crm/copilot/follow-up` | Draft follow-up text (returns draft, never sends) | `crm.copilot.use` |

List responses use a shared envelope: `{ items, next_cursor, total_estimate }`; every mutation returns the fresh row so the UI can replace optimistic state.

### Data model

Tables (all carry `organization_id uuid not null references organizations(id) on delete cascade`, `created_at`/`updated_at timestamptz not null default now()`; every list index is tenant-leading):

```text
crm_companies(id uuid pk, name text not null, domain text, industry text, owner_user_id uuid,
  status text not null default 'lead', tags text[] not null default '{}', custom jsonb not null default '{}',
  notes text not null default '', archived_at timestamptz)
crm_contacts(id uuid pk, first_name text, last_name text, email citext, phone text, job_title text,
  company_id uuid null references crm_companies(id) on delete set null, owner_user_id uuid,
  status text not null default 'lead', tags text[] not null default '{}', custom jsonb not null default '{}',
  notes text not null default '', last_activity_at timestamptz, archived_at timestamptz)
crm_pipelines(id uuid pk, name text not null, is_default boolean not null default false)
crm_pipeline_stages(id uuid pk, pipeline_id uuid not null, name text not null, kind text not null,
  position integer not null, probability integer not null default 0)
crm_deals(id uuid pk, pipeline_id uuid not null, stage_id uuid not null, title text not null,
  company_id uuid, contact_id uuid, owner_user_id uuid, amount numeric(14,2) not null default 0,
  currency char(3) not null default 'USD', probability integer, expected_close_on date,
  source text, lost_reason text, stage_changed_at timestamptz not null default now(), archived_at timestamptz)
crm_activities(id uuid pk, kind text not null, subject text not null, body text not null default '',
  company_id uuid, contact_id uuid, deal_id uuid, occurred_at timestamptz not null default now(),
  due_at timestamptz, done_at timestamptz, owner_user_id uuid, created_by uuid)
crm_views(id uuid pk, owner_user_id uuid not null, entity text not null, name text not null,
  filters jsonb not null default '{}', columns text[] not null, sort jsonb not null default '{}',
  is_shared boolean not null default false)
```

Checks: `email ~* '^[^@[:space:]]+@[^@[:space:]]+\.[^@[:space:]]+$'`; `amount >= 0`; `probability between 0 and 100`; `(kind = 'lost') = (lost_reason is not null)`; stage `kind in ('open','won','lost')`; unique `(organization_id, lower(name))` on companies; unique `(organization_id, lower(email))` on contacts where `email is not null`.

Indexes: `crm_contacts_org_updated_idx (organization_id, updated_at desc)`, `crm_contacts_org_email_idx (organization_id, lower(email))`, partial `crm_contacts_open_idx ... where archived_at is null`, `crm_deals_org_stage_idx (organization_id, stage_id, stage_changed_at)`, `crm_deals_org_close_idx (organization_id, expected_close_on) where archived_at is null`, GIN on `tags` and on `custom`, `crm_activities_org_occurred_idx (organization_id, occurred_at desc)`.

Migration: `database/migrations/0011_crm.sql` — additive only; seeds one default pipeline ("Sales", stages New → Qualified → Proposal → Negotiation → Won/Lost) per existing organization plus a platform default for new ones.

### Events

Emitted (via `crates/events`, subscribable through `/api/v1/webhooks`):

- `crm.contact.created`, `crm.contact.updated`, `crm.contact.merged`, `crm.company.created`, `crm.company.updated`
- `crm.deal.created`, `crm.deal.updated`, `crm.deal.stage_changed`, `crm.deal.won`, `crm.deal.lost`
- `crm.activity.logged`

Payloads carry ids and the changed field list only — never a rendered document. `crm.deal.stage_changed` includes `deal_id`, `from_stage_id`, `to_stage_id`, `amount`, `currency`, `owner_user_id`; `crm.deal.won` adds the effective close date. Consumed: `form.submitted` (REQ-064 forms) creates a contact + deal; `sales.quote.accepted` (REQ-052) marks the sourcing deal `won`. Automations may trigger on any emitted name; notification rules fire on `crm.deal.won`/`.lost` for the deal owner and the organization owners.

### Acceptance criteria

- [x] Migration `0022_crm.sql` applies on a populated database without touching existing rows; `cargo test -p omnion-module-crm` is green (84 unit tests). (Renumbered from `0021`, which `0021_iam_sso.sql` on main took while this file was in flight; a migration number is global across branches.)
- [x] Every `/api/v1/crm/*` route answers 401 unauthenticated, 403 with the permission missing, and 200 with it granted; a contact from another organization is invisible (404) — proved by `every_crm_route_is_permission_guarded` and `a_record_of_another_organization_is_invisible`.
- [x] Creating, updating, archiving and merging a contact/company/deal writes an audit entry with actor, before/after diff and request id. (Contacts and companies proved in slice 2; the deal's `crm.deal.created` / `crm.deal.updated` / `crm.deal.archived` / `crm.deal.stage_changed` rows are asserted by the slice-3 walks, which now run: **29/29 passed** against a fresh database in `b49aa3d`.)
- [x] `crm.contact.created`, `crm.deal.stage_changed` and `crm.deal.won` appear in the event feed with the documented payload and reach a subscribed webhook endpoint. (`the_won_and_lost_flows_demand_their_own_input_and_emit_their_own_event` and `a_stage_move_persists_reloads_and_emits_the_documented_event`, both in the 29/29 run. The walk also asserts the payload carries **no** deal title, and `b49aa3d` fixed `crm.deal.won`'s `close_on`, which `json!` was writing as a `[2026, 273]` tuple.)
- [x] Contact list: search, owner, status, tag and date filters combine; sort persists in a saved view; column chooser survives reload — `the_contact_list_filters_sorts_and_totals` and `a_saved_view_is_the_query_it_stands_for`, with the screen in the walkthrough route list.
- [x] Inline edit of owner/status/tags saves optimistically and rolls back with a visible error when the API rejects it — the screen sends the patch, restores the previous value and shows the refusal under the field.
- [x] The API rejects a malformed e-mail and a duplicate e-mail (case-insensitive) with a field-level message in `error.details.field` — `the_contact_and_company_forms_refuse_what_they_name`.
- [x] CSV import runs a dry run that shows row count, mapped columns and per-row errors before commit; commit writes only the valid rows — `an_import_previews_before_it_writes_and_then_writes_what_it_accepted`, and `an_export_is_the_lists_own_answer_and_imports_back` proves the export applies the same field hiding as the list and re-imports.
- [x] Pipeline board drag moves a deal, persists the new stage, updates per-stage count/sum/weighted sum, and is reversible with `ctrl + ←/→`. The one statement that computes the count, the sum and the weighted sum is the board's own header query, and the keyboard path sends the **same** request the drag sends. *Proved by `a_new_deal_lands_on_the_first_open_stage_and_the_board_adds_up` and `moving_a_deal_to_the_stage_it_is_already_in_is_a_no_op` in the 29/29 run. The browser walk (`runCrmDealsDepth`) is registered in the route list.*
- [x] Moving a deal to `lost` requires a reason; moving to `won` records/confirms the close date and emits `crm.deal.won`. The rule is in the module (`resolve_move`) and in the schema's trigger, the dialog is required before the write, and leaving the lost column forgets the old reason. *Proved by `the_won_and_lost_flows_demand_their_own_input_and_emit_their_own_event` in the 29/29 run. `b49aa3d` is what made this reachable at all: the close date was a 422 for every caller.*
- [x] Visibility scoping works: a member with `own` sees only their records (enforced in SQL, a hidden record is a `404`), a team lead sees the group's, an unnarrowed account sees the organization's — `the_own_visibility_level_hides_a_colleagues_record`.
- [x] A role without `crm.fields.sensitive.read` sees the flagged field hidden, at every depth of the custom object, in both the list and the detail — `the_flagged_fields_are_hidden_from_a_role_without_the_key`. (Export and import preview arrive with slice 2's CSV.)
- [x] Record timeline merges activities, stage changes and audit-worthy notes in one ordered stream with correct relative times. (`record_timeline` in `modules/crm/src/activities.rs` merges three arms under **one** `order by` — an activity, a deal's stage change and a deal's archive marker — rather than three queries merged in Rust, which can disagree with themselves the moment a row moves between them. The stage arm reads the deal rows rather than the event log, so a record imported before the event bus existed still has a correct history and a replayed event cannot duplicate an entry. `a_logged_activity_appears_in_the_feed_and_on_the_records_timeline` asserts the activity *and* the stage change are both on a contact's stream and that a contact's call is **not** on its company's; `the_relative_label_reads_as_a_person_would_say_it` pins the label, which is presentation only — ordering always uses `occurred_at`.)
- [x] CRM copilot returns a summary and a suggested next action; nothing is written to a record without an explicit user action, and the call is audited. (Route in `d2b9f74`: `POST /api/v1/crm/copilot/summarize/{deal_id}` and `/follow-up/{deal_id}` behind `crm.copilot.use`, sharing one handler. The **scoped read runs before the model is resolved**, so a foreign deal is a `404` and never reaches a provider. Every call is audited — `crm.copilot.summarized` / `crm.copilot.follow_up_drafted`, and `crm.copilot.failed` on the failure path — carrying the deal, the action, the model and the size but **neither the draft nor the deal's title**. Proved by `the_copilot_is_guarded_scoped_and_audited` in the 36/36 run: 401, 403 for a manager holding the whole contact/deal family but not the copilot key, 404 across the tenant boundary **in both directions**, the audit row, and the deal unchanged afterwards. No provider is faked — this installation connects none, so the call fails and the audit is asserted *because* a failure is still a call; the sanitiser's text rules are unit-tested in `modules/crm/src/copilot.rs`. **The screen shipped in `2eb8638`** — the endpoints had been reachable only from curl, which is the same as a feature that does not exist: a **Copilot** button on every board card (on the card rather than in a row menu, because on the board the card *is* the record), a side panel that answers both actions, takes focus when it opens and closes on Escape, and `?focus=<id>&copilot=1` to land on the answer from a link. A **refusal renders as a sentence** rather than a silently missing panel, and `is_draft` drives a visible marker that turns amber and reads "written to the record" if a response ever contradicts it. The answer renders in a **text node** — the server's sanitiser already reduced it to plain text. Proved in the browser by `runCrmCopilotDepth`.)
- [x] Global search (REQ-002) finds contacts, companies and deals by name/e-mail and deep-links to the record. (**Shipped** in `5bedddb`/`241cf91`: three providers in `crates/search/src/providers.rs` — `contacts` and `companies` behind `crm.contacts.read`, `deals` behind `crm.deals.read`, each pointing at its own screen — with the three upserts, the three prune arms and the `crm.*` event plans in `crates/search/src/indexer.rs`, plus `database/migrations/0031_crm_search_providers.sql` to enable the keys. A contact's **notes** are excluded: the module flags them `crm.fields.sensitive.read` and a vector cannot answer a per-role question. The deep link is `?focus=<id>`: the two lists open that row's editor, the board marks that card. Proved by three walks in the **39/39** run — the hit carries the right `url`, `type:contacts` narrows to contact rows only, a reader holding only the contact key finds the contact and gets **no** deal rows (the same split `/api/v1/crm/deals` enforces), and archiving a company takes its document out of the index. **⌘K "New contact" / "New deal" is not done** — the command centre (REQ-032) is wave 1 and owns the palette's own rows.)
- [x] An automation rule triggered by `crm.deal.stage_changed` runs **once**. (Proved by driving the **real** matcher — `crates/automation/src/matcher.rs` — over the **real** bus in this suite's own database, so the event the rule reads is the one the board's stage endpoint emitted rather than a hand-written row. Three walks: `a_rule_on_a_deal_stage_change_runs_exactly_once`, `a_rule_whose_condition_does_not_hold_starts_nothing` and `defining_the_rule_needs_the_workflow_key_and_a_tenant_rule_stays_home`, all in the **42/42** run. They assert: a move to the stage a deal is **already in** starts nothing (the board's `ctrl + ←/→` posts on every key press); one move starts **one** run whose step carries *this* move's resolved values — the subject reads "A deal entered open" and the body the deal's id, amount and currency, so a retry would repeat the first attempt rather than re-reading a bus that has moved on; a second drain over the same bus is **idle**, which is what "once" means rather than a count; the match is audited as `automation.rule.matched` against the execution; a second deal starts a second run, because exactly-once is per *event* and collapsing two customers into one run would lose one; and a condition that does not hold is `skipped`, not `matched`. The rule's key is `workflows.manage`, so a CRM manager who may move deals all day still cannot define the rule that watches them, and a rule for another organization is refused.)
- [ ] Empty, loading and error states exist on all six screens; no dead buttons and no placeholder rows.
  *Two thirds are in, and both halves were defects rather than omissions.*

  *The **tenant resolution** (`b899f7f`): a platform account that has no organization is now a
  state with a sentence and an action rather than a 400 with a code nobody can draw from.*

  *The **error state** (`63512c5`, `c0e51fe`). The box asks for "error state with retry button and
  request id" and the request id did not exist: the refusal body carried a code, a message and
  sometimes a details object, and nothing that named the exchange. So the id was built —
  `apps/api/src/request_id.rs` stamps `x-request-id` on every response (so an id captured from a
  successful call can correlate the next failure of the same operation) and `error.request_id` in
  the body (so a refusal pasted into a ticket still carries it). Installed outside the `/api/v1`
  nest, so the liveness probes are stamped too. An inbound id is honoured only when it is safe to
  reflect, and a hostile value is **replaced**, not sanitised.
  Then the six screens: each had hand-rolled its own block, they had drifted into three shapes,
  and the settings editor and the lead inbox had lost the retry. `components/error-state.tsx` is
  the one shape — sentence, code, the id **when the server named one**, and a retry. A network
  failure never reached the server, so no id is invented for it: a fabricated correlation number
  would look more useful than it is and send an operator to a log line that does not exist.
  `lib/crm.ts` was also dropping `error.details`, which is why every field-level refusal was a
  generic banner with nothing under the input it was about.
  Proved by `runCrmStateSweep`: a real 503 with a real body at the network layer, five screens
  checked for a sentence / a retry / the id, the retry **pressed** and the recovery watched, and
  the no-id case asserted to invent nothing. 10/10 unit tests, typecheck 2/2.*

  *What is left is the **empty state** on the two screens that still fall back to a bare paragraph
  rather than `EmptyState`: the board's per-column body and the activities filter bar. Both exist;
  both need the sentence-and-action shape the other four have.*
- [ ] Mobile 390×844: lists are usable, the board scrolls horizontally with sticky stage headers, and forms are single-column.
- [ ] Keyboard: `/` focuses search, `j`/`k` move rows, `enter` opens, `e` edits, `?` shows the shortcut sheet.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with routes `/crm`, `/crm/contacts`, `/crm/companies`, `/crm/deals`, `/crm/activities`, `/crm/settings/pipelines` (desktop pass) and `/crm/contacts`, `/crm/deals` (mobile pass). The QA script must exercise: create company → create contact attached to it → create deal on that contact → drag/arrow-move the deal one stage → add a tag → save a view → run an import dry run with a two-row CSV (one invalid) → open the record timeline. It must click every visible control on each screen (the generic interactor covers buttons/links/selects; the empty state must still show at least the primary action).

What the visual check should see: a board with four stage columns, per-column count/sum headers and at least three cards, no clipped amounts, no horizontal overflow of the page; a table with a sticky header, alternating rows, tag chips and owner avatars; a contact form with labelled inputs and a visible validation message; skeleton loaders on first paint and a real empty state on a fresh pipeline. Screenshots: `page-crm-contacts`, `page-crm-deals`, `page-crm-contact-detail`, `mobile-crm-deals`. Zero high findings; contrast AA on badges and chips.

### Slices

1. **Data + API core.** Migration, companies/contacts CRUD with filters and paging, permission keys registered, audit + events wired, integration tests. Done when `cargo test -p omnion-module-crm` is green and a signed-in curl round-trip creates a contact that produces an audit row and a `crm.contact.created` event. **Shipped** as `database/migrations/0021_crm.sql` (the spec's `0011` was taken by IAM before this work started; a migration number is global, so the next free one was taken and the file is still additive), `modules/crm` (`omnion-module-crm`) and `apps/api/src/routes/crm.rs`.
2. **Contact & company screens.** List, filters, saved views, column chooser, inline edit, create/edit form with validation, archive/merge, CSV import (dry run + commit) and export. Done when the walkthrough clicks both screens end to end and the QA pass reports zero high findings. **Shipped** in `apps/admin/features/crm/`, `apps/admin/lib/crm.ts`, `modules/crm/src/{csv,views}.rs` and `apps/api/src/routes/crm_views.rs`.
3. **Deals + pipeline board.** Stages editor, board with drag + keyboard move, per-stage totals and weighted forecast, won/lost flows, list mode. Done when QA drags a card, reloads, and the stage plus `crm.deal.stage_changed` persist. **Shipped** in `9909af9` as `modules/crm/src/deals.rs`, `apps/api/src/routes/crm_deals.rs`, the five `crm.deals.*` / `crm.pipelines.manage` keys, `apps/admin/features/crm/deals-view.tsx` and the walkthrough's deals depth pass. Shipped and green: 29/29 of its walks ran in `b49aa3d` and again in `d132936` (35/35 with slice 4).
4. **Activities, timeline, copilot, search & automations.** Activity capture, merged timeline, copilot read-only actions, global-search registration, workflow triggers and the `form.submitted` consumer. Done when a logged activity appears in the record timeline and in search, and an automation rule triggered by `crm.deal.stage_changed` runs once. **All seven parts are in** (2026-09-28, the seventh in `2a4a0f0`): activity capture, the feed, the log form and the merged timeline; the copilot's module, its two endpoints, their audit and **its screen**; the global-search registration; the **workflow-trigger proof**; and the **`form.submitted` consumer**. The ⌘K rows the command centre owns (REQ-032) stay with that request.
   - **The seventh part, in its own words.** A submitted form becomes a contact and a deal. The
     producer is REQ-064's public submit endpoint, which is **not in this build** — so the
     consumer is written against the *event contract* (`form.submitted`) rather than against a
     form table, and the day the form builder lands it only has to emit the documented name.
     The design in one line each: `crm_form_leads.event_id` is the bus identity and the primary
     key, so a retried drain and a second API process both leave **one** contact; the claim is
     taken **before** the writes and there is deliberately no transaction, because
     `create_contact`/`create_deal` take a `&PgPool` and a rolled-back batch would show nobody
     anything; routing is a **row** (`crm_lead_settings`), not a constant, because a support
     address and a "quote me" address are the same event with different meanings; and a repeat is
     the **same person**, matched on the normalized address and parked in the operator's repeat
     stage rather than opening a second deal for one interest.
   - **A submission is never silently dropped.** Five outcomes, all of them listed: `created`,
     `merged`, `rejected` (nothing usable, *with the sentence*), `orphaned` (no organization) and
     `disabled` (the policy says no). A form builder that quietly loses a submission is the most
     expensive failure this feature has, and the inbox is where somebody finds out.

### The walks that were written but never run (2026-09-28, `d132936`)

Slice 4 part one shipped with six integration walks that had never executed. They were never
executed because the suite is pointed at the shared development database by default, and that
database's `_sqlx_migrations` ledger carries versions 19 and 21 from a run against files no branch
carries — so the fixture panicked before any test body and the "29 passed" that was being reported
was 29 of the *slice-1-3* walks running against a database that happened to work. The six slice-4
walks were silently not in that number.

Pointed at a throwaway database, they ran and found **four product defects**, each of which made a
documented feature unusable rather than slightly wrong:

1. **The activity feed 500'd for every caller at the `team` visibility level.** The clause was
   built as `any(` followed by a `separated(", ")` of individual binds, which is `any($1, $2)`; the
   array operator needs one array parameter. `open_tasks` repeated the clause by hand, and
   `set_activity_done` used `$3` for both the `done_at` timestamp and the id list, so closing any
   task was a 500 too. All three now share `push_activity_visibility`.
2. **Every timestamp in an activity response was `time`'s tuple** (`[2026, 263, …]`), because
   `Activity` and `TimelineEntry` carried no serde attribute on those fields.
3. **Logging a task with a due date was a 422 for every caller**, because `ActivityChanges`' three
   timestamps were bare `Option<OffsetDateTime>` and `time`'s serde support is opt-in per field —
   a bare one accepts no JSON string at all. `dates::instant` is the round trip that fixes it.
4. The three walks themselves passed an e-mail where a session token belonged, a deal's id where a
   route takes an activity's, `rp.permission` where the column is `permission_key`, and read
   `actor_user_id` from a helper that names the key `actor`.

**For the next writer.** A suite's green number is only about the tests in it. If a feature's walks
are added and the run is a re-run of a database the fixture has always used, check *which* tests the
count is made of before believing the feature is proved — and run the suite against a database
created for the purpose at least once per feature.

### Risks / notes

- **Migration number** is "next free slot" — if another module lands first the loop renumbers; the file stays additive either way.
- **Workspace wiring:** the root `Cargo.toml` gains `"modules/*"` in `members` (one-time, shared with every business module) and `apps/api/src/routes/crm.rs` mounts the router behind `guards::require`.
- Visibility (`own`/`team`/`all`) and field hiding must be enforced in SQL, not only in the UI — the API is the boundary, and exports must apply the same rules.
- Drag & drop needs a full keyboard path (QA clicks everything; a drag-only control would read as dead).
- Weighted forecast is `amount × probability/100`; keep it one SQL expression so board headers and the overview cannot drift apart.
- Merge must be transactional and must move activities/deals, then archive the loser — never delete a record other rows point at.
- Copilot output is untrusted text: render as plain text, never as HTML, and always show it as a draft.

### The migration "blocker" was a misdiagnosis (2026-09-27, corrected in `b49aa3d`)

An earlier tick recorded that `cargo test -p omnion-api --test crm` could not run because this
branch's migration set had a gap at 0019/0020, and it proposed a platform-wide renumber as the
remedy. **That diagnosis was wrong, and acting on it would have touched five other writers' branches
to fix nothing.** It is kept here because a confidently wrong blocker costs the next tick more
than the bug it described.

What the evidence actually was, and what it was read as:

* The suite reported 29 FAILED in 0.99 s. That was read as "a fixture that never set up".
* The fixture's panic was `Migration(VersionMissing(19))`, read as "this branch's set is not
  contiguous".
* The fix attempted was a merge of `main`, on the theory that it would supply 0021.

What is true. The migration *directory* is non-contiguous, and that is real. But
`Migration(VersionMissing(N))` is raised when a version is **applied in the database and absent
from the source**, which is a different condition from "the source is missing a file". The shared
development database `omnion` — the default target of `OMNION_DATABASE_URL` — has 19 and 21 in
its `_sqlx_migrations` ledger, applied by a run against files no branch carries today. Pointing
the suite at a database created a moment earlier makes all 29 walks run, and 5 of them fail on
real defects.

So the migration numbering is a shared-environment wart, not this branch's blocker, and the
correct response was to name the database the suite was talking to, not to renumber five branches.

**What the runs then found** (all fixed in `b49aa3d`, walks now 29/29):

1. `create_deal`/`patch_deal` bound a money `String` into a `numeric` column. Postgres will not
   coerce it, so every create was a 500 and the board, won and lost flows all sat on top of it.
2. `expected_close_on` was a bare `time::Date`, which `serde` reads as a tuple — so the
   `"2026-12-01"` a browser's date input sends was a 422, and the won/lost flow was unreachable
   from the panel. `modules/crm/src/dates.rs` is the `YYYY-MM-DD` round trip that fixes it, on the
   request side and in the two hand-built `json!` payloads that bypass `serde` entirely.

**For the next writer who hits `VersionMissing`.** Create a throwaway database, point
`OMNION_DATABASE_URL` at it, and run the suite there before believing anything about the
migration ledger. The distinction that mattered: a *source* that is missing a file is a gap you
can see with `ls database/migrations`; an *applied-but-absent* version is invisible in the source
and only shows up as a panic. Two conditions, one error message.
