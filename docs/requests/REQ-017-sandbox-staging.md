# REQ-017 — Sandbox / Staging

> **Status:** in-progress (slices 3–4; **tick 69 closed large-site batching, and the code
 it closed did not exist** — the Risks section had promised "the runner batches by area with a
 configurable page size" since the request was written and `runner.rs` held one unbatched
 `insert … select` per area. Writing it surfaced **three** defects that every existing walk
 passed, all of the same family: a value that was self-consistent and matched nothing on disk
 (the per-batch `delete` wiping the previous batch; the first window's `id > NULL` lower bound
 copying nothing; and `limit 1 offset n-1` skipping the final partial batch, so a 7-page clone
 at a batch of 2 reported `done` with 6 pages and an `items_done` that agreed with itself). The
 walk `a_clone_that_crosses_a_batch_boundary_copies_every_row_exactly_once` is the first thing in
 this suite that crosses a boundary, and it is what found all three; tick 67 fixed the clone-wizard contract — three of the six
> clone areas copied nothing while being labelled and priced like the three that do, and the
> panel's guard counted ticked boxes where the runner requires an area that actually copies, so a
> staging environment could be created empty through the browser. `Area::copies()`/`Area::note()`
> now declare the truth in the crate, the API carries `copies` + `note` on every option, the
> shared areas are listed unticked and explained rather than hidden, and a unit test reads the
> runner off disk so the two halves cannot drift again (`5c10e54`). Tick 68 closed the two isolation defects the clone created in
 > the **public** read — a staging copy used to match two rows and answer `500`, and a page that
 > existed only in staging used to answer `200` and publish an unpublished draft to every visitor
 > of production; `pages::find_page_by_slug` now takes the environment as a *required* argument so
 > the unsafe form cannot be written, and the public renderer resolves the organization's
 > production environment explicitly. The same tick added the `noindex` layer, so a staging host
 > answers `X-Robots-Tag: noindex, nofollow` on every status including the `404`. **Still owed:
 > the browser gate** — the four `runEnvironmentsDepth` claims plus the five new ones for the
 > clone areas have never executed, and for the fourth tick running the box has had no room
 > (QA slot held by a live sibling; `/mnt/apopic` at 100%, load 13–16)
> · **Captured:** 2026-09-25 ·
 **Browser gate unblocked at tick 76**: three consecutive ticks closed with "the slot is held", and the cause was never the box — it was the harness, which had two authors. Merging `origin/main` (13 commits) produced four conflicts, three in the QA harness, and resolving the qa-slot holder-format conflict naively would have introduced a defect neither branch had a test for: main's `awk '{print $NF}'` reader returns the OWNER on this branch's two-field line, so the reaper would `kill` a live pass while inspecting it. Fields are now read by position, and `scripts/qa/qa-slot-parse-test.sh` (8/8, six consecutive runs; both reader mutants caught) is the test that had never existed. The pass itself has still not run this tick — the slot is free and the gate is next.
> **Layer:** platform
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

The admin clicks **Create Staging Environment**:

```text
Production
     │
     └── Clone
          ↓
       Staging
```

Changes are tried there first:

```text
Staging
 ↓
Preview
 ↓
Approve
 ↓
Deploy to Production
```

## Implementation spec

Staging is a **content-and-configuration environment inside the same installation**: a second copy of an organization's addressable content that the panel can enter, edit and later
promote back.
It is deliberately not infrastructure duplication — one database, one deployment, two environments — and the spec says so everywhere the UI could imply otherwise.

### Scope (in / out)

**In**

- Environment records per organization: one `production` (created with the organization) and zero or more `staging` environments, each with a key, a name, a status and a staging host.
- **Clone**: copy pages, revisions, translations, menus/settings records and media *references* from production into a staging environment as a tracked job with progress and per-area
  counts.
- **Enter staging**: an environment switcher in the panel header that scopes content screens to the chosen environment, with a permanent, non-dismissible staging banner.
- **Changes view**: the diff between a staging environment and production — added, updated, deleted items per area, with author and timestamp, conflict flags when production moved on.
- **Promotion**: request → approve → apply a *frozen* change set to production in one transaction, with optimistic concurrency per row, an audit trail and a promotion history.
- Staging hosts are excluded from search engines and marked `noindex`; public preview rendering of staging content is REQ-018's business, not this one's.

**Out**

- Separate database instances, containers or clusters per environment (REQ-035 / REQ-036 cover infrastructure-level isolation); this request never provisions infrastructure.
- Application version promotion, rollback of builds, replica/CPU views (REQ-024 Deployment Center).
- Plugin, theme and workflow sandboxes for experiments beyond promotion (REQ-034).
- Billing, per-environment quotas, or user account separation — identity and roles are shared, and staging access is a permission, not a separate login.

### Screens (UI)

- **`/environments` — list.** Table columns: Name, Type (badge: production / staging), Status (active / cloning / error / archived), Content (pages + translations counts of the last
  clone), Staging host (copyable, copy button), Last clone (relative time + actor), Promotions (pending count linking to the detail tab), actions (Open staging, Re-clone, Promote,
  Archive). Filters: type, status, text search. Bulk: none (archive is per-row and destructive). Empty state when no staging environment exists:
  one-line explanation of what staging is and a `Create staging environment` button.
- **`/environments/new` — create wizard (3 steps).** Step 1: Name (1–64 chars) and key (lowercase slug, unique per organization, auto-derived, editable); Step 2: clone options —
  checkboxes for Pages & revisions, Translations, Menus & navigation records, Site settings, Theme selection, Workflow definitions, with a live estimate ("~412 rows · ~18 MB of metadata,
  media files are referenced, not copied") and a `Exclude archived pages` toggle; Step 3: summary + `Create and clone`. The wizard blocks on: empty name, invalid key, duplicate key, zero
  areas selected. Nesting limit:
  a staging environment can never be cloned to another staging environment — the option is not offered.
- **`/environments/[id]` — detail.** Header: name, type badge, status, host, primary actions (Open staging, Re-clone, Promote, Archive). Tabs: Overview (clone metadata, per-area counts,
  storage note, host, who requested the clone), Changes (the diff table), Promotions (history + in-flight promotion), Activity (audit entries for the environment). Changes table columns:
  Item (title, links to the record), Area (Page / Translation / Menu / Setting / Theme), Change (added / updated / deleted badges), Last change by, Last change at, Conflict (badge when
  production changed since the clone). Filters: area, change type, conflicts only, text. Bulk:
  select non-conflicting rows → `Promote selection`.
- **Promotion dialog.** Shows the frozen change set summary (counts by area), a conflict list when any, the requester, and — above 25 changes — a typed confirmation of the environment
  name. Primary button `Request promotion` (creates a `pending_approval` record) or `Approve and deploy` for a caller holding the deploy permission.
  Progress replaces the dialog with a step timeline (validate → apply → audit → done) that survives a refresh.
- **Header environment indicator.** `AppShell` header gains an environment chip next to the site switcher: `Production` (neutral) or `Staging — not public` (warning colour, with `Exit to production`).
  While a staging environment is active, every content screen shows a thin warning top border and the banner text is repeated at the top of the page.
- **States.** Loading: wizard estimate and Changes tab use skeletons. Error during clone: status `error`, the Overview tab shows the failing area, the error string and a `Retry clone`
  button. Clone in progress: a progress bar with `items_done / items_total` that updates without a manual refresh; a cancel action marked as destructive.
  Empty Changes: "No changes since the clone."
- **Keyboard / mobile.** `/environments` supports `/` search, `n` new environment, `Enter` open, `Esc` close dialog.
Below `lg` the tables become cards, the wizard becomes a single scrolling form, the promotion dialog becomes a full-height sheet with the primary action pinned at the bottom, and the
environment chip stays visible in the sticky header.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/environments` | List environments of the organization | `deployment.read` |
| POST | `/api/v1/environments` | Create a staging environment and start its clone | `deployment.preview` |
| GET | `/api/v1/environments/{id}` | Environment detail, per-area counts, job state | `deployment.read` |
| POST | `/api/v1/environments/{id}/clone` | Re-clone from production (discards staging changes after confirmation) | `deployment.preview` |
| GET | `/api/v1/environments/{id}/clone-jobs` | Clone job history and current progress | `deployment.read` |
| POST | `/api/v1/environments/{id}/clone-jobs/{job_id}/cancel` | Cancel a running clone | `deployment.preview` |
| GET | `/api/v1/environments/{id}/changes` | Diff vs production; `area`, `change`, `conflicts`, `cursor` | `deployment.preview` |
| POST | `/api/v1/environments/{id}/promotions` | Request a promotion of selected changes (frozen change set) | `deployment.deploy` |
| GET | `/api/v1/environments/{id}/promotions` | Promotion history of the environment | `deployment.read` |
| GET | `/api/v1/promotions/{id}` | One promotion: change set, conflicts, step log | `deployment.read` |
| POST | `/api/v1/promotions/{id}/approve` | Approve and apply the frozen change set to production | `deployment.deploy` |
| POST | `/api/v1/promotions/{id}/cancel` | Cancel a pending promotion | `deployment.preview` |
| DELETE | `/api/v1/environments/{id}` | Archive a staging environment (content kept, host released) | `deployment.rollback` |

Errors are named: `environment_not_found`, `environment_key_taken`, `staging_nesting_refused`, `clone_already_running`, `promotion_conflict` (with the conflicting item ids),
`self_approval_refused`, `environment_not_staging`.

### Data model

Migration `0012_environments.sql` (number is a placeholder — renumber to the next free slot):

- `environments` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null references organizations(id) on delete cascade`, `key text not null`, `name text not null`,
  `type text not null check (type in ('production','staging'))`, `status text not null default 'active' check (status in ('active','cloning','error','archived'))`,
  `cloned_from_environment_id uuid references environments(id) on delete set null`, `cloned_at timestamptz`, `staging_host text`, `created_by uuid references users(id) on delete set null`,
  `created_at`, `updated_at`. Constraints: `key ~ '^[a-z0-9]([a-z0-9-]{0,53}[a-z0-9])?$'`, `length(btrim(name)) between 1 and 64`, `unique (organization_id, key)`, and a partial unique
  index guaranteeing exactly one production environment per organization: `create unique index environments_single_production_key on environments (organization_id) where type = 'production'`.
  `staging_host` gets a unique index where not null.
  Existing organizations are backfilled with their production environment inside the same migration.
- `environment_clone_jobs` — `id uuid pk`, `environment_id uuid not null references environments(id) on delete cascade`, `status text not null default 'pending' check (status in ('pending','running','done','failed','cancelled'))`,
  `areas text[] not null`, `items_total integer not null default 0`, `items_done integer not null default 0`, `error text`, `started_at timestamptz`, `finished_at timestamptz`,
  `created_by uuid`, `created_at`.
  Index `(environment_id, created_at desc)`.
- `promotions` — `id uuid pk`, `environment_id uuid not null references environments(id) on delete cascade` (source staging), `target_environment_id uuid not null references environments(id) on delete cascade`,
  `status text not null default 'pending_approval' check (status in ('pending_approval','approved','running','done','failed','cancelled'))`, `changes jsonb not null` (the frozen change
  set: item id, area, operation, base snapshot hash), `conflicts jsonb not null default '[]'::jsonb`, `requested_by uuid`, `approved_by uuid`, `approved_at timestamptz`, `step_log jsonb not null default '[]'::jsonb`,
  `error text`, `created_at`, `updated_at`, `finished_at`.
  Index `(environment_id, status, created_at desc)`, partial index on `(status) where status in ('pending_approval','running')`.
- Environment ownership on content: `alter table pages add column environment_id uuid references environments(id) on delete cascade`, backfilled to the organization's production
  environment and made `not null` after the backfill; same column and treatment for `menus`, `site_settings` and `translations` (translations are resolved through the row's environment
  when a row is written by a staging context).
  New composite indexes `(environment_id, site_id)`, `(environment_id, updated_at desc)`.

### Events

- **Emitted:** `environment.created`, `environment.clone.started`, `environment.clone.completed` (with per-area counts), `environment.clone.failed` (with the failing area),
  `environment.archived`, `promotion.requested`, `promotion.approved`, `promotion.completed`, `promotion.failed`, `promotion.conflict` (with conflicting item ids).
- **Consumed:** `page.published`/`page.updated` inside a staging environment advance the Changes diff cache; nothing else is consumed.
- **Webhook relevance:** promotion lifecycle events are the CI/CD signal — an endpoint subscribed to `promotion.*` can trigger a build, a cache purge or a smoke test after a deploy, and
  `environment.clone.completed` tells a test runner that fresh staging data is ready. Production promotion deliberately does **not** re-emit `page.published` for every copied row (that
  would flood subscribers);
  one `promotion.completed` carries the affected ids.

### Acceptance criteria

- [x] A new organization gets exactly one `production` environment; a second production insert fails at the database (partial unique index proven in a test).
- [x] `POST /api/v1/environments` creates a staging environment with status `cloning` and returns immediately; the clone job reaches `done` and per-area counts match the production
  counts.
- [x] Clone copies pages, revisions, translations, menus, site settings, theme selection and workflow definitions, and copies **no** media blobs (verified by storage object count before
  and after).
  *(tick 67 is the reason this reads oddly: three of the six areas — menus, site settings and the
  theme — were being **labelled and priced** in the wizard while copying **nothing**, and
  `Area::copies()` now declares the truth in the crate. **Tick 74 ran the walk for the first
  time and it was wrong twice before it was right** (`9c9749ab`), which is the honest shape of
  this box: `a_clone_copies_content_and_leaves_every_media_byte_where_it_was` had been written
  and committed without ever executing, and a walk that has never run is a *guess about a
  schema*, not evidence. It decoded `environment_clone_jobs.areas` as `jsonb` when it is
  `text[]` (the per-area counts are `area_counts jsonb`), and once that was fixed it asserted
  the theme area was priced and copied — which is the exact defect tick 67 closed, asserted
  back into existence by a test nothing contradicted. It now asserts the true claim: the job
  must **not** list `theme` among its counts, the three areas that really copy must each be
  present, and `items_done` must equal the sum of `area_counts` (a self-consistent runner that
  reported 3 of 3 having copied nothing is the shape the batching bugs took). The object count
  itself comes from `omnion_backup::pending_objects` — the reader an archive uses — so the
  before and after numbers are comparable by construction rather than by agreeing on a
  definition; it also compares the **key set**, because a clone that moved bytes would keep the
  count and break production's copy, and finally reads the object back to prove it is still
  there. PASS, 8.3s.)*
- [x] Clone is idempotent: re-cloning an unchanged environment produces the same counts and no duplicate rows (natural keys are unique per environment).
- [x] Staging nesting is refused with `staging_nesting_refused` for a staging source.
- [x] Editing a page in staging leaves the production row byte-identical (asserted by comparing `updated_at` and revision hashes).
- [x] `GET /api/v1/environments/{id}/changes` lists the edited page as `updated`, a new page as `added`, a deleted page as `deleted`, each with author and timestamp — `the_change_set_names_what_staging_holds_that_production_does_not` asserts all three kinds in one walk, the untouched page's absence, and that production still holds every row it held before.
- [x] A production edit made after the clone marks the item `Conflict`, and promoting a change set that contains conflicts is refused with `promotion_conflict` listing item ids.
  *(walk `a_production_edit_after_the_request_is_refused_with_the_item_id`: the conflict re-check runs at **approve**, not only at request, and the refusal carries the offending `page_id` in `error.details.items`; the row ends `failed` with the refreshed list on it and its step log stopping at `validate`.)*
- [x] Promotion of a clean change set applies every item in one transaction: production pages match staging content afterwards, and `promotion.completed` carries the same item count.
  *(walk `promoting_a_clean_change_set_applies_every_item_and_says_how_many`: all three kinds at once — the added page reaches production, the deleted one is gone, the edited one carries staging's title — and the event carries the 3 affected ids with `written: 2` / `removed: 1`.)*
- [x] A failure injected mid-apply leaves production unchanged (transaction rolled back) and the promotion status `failed` with a readable error.
  *(walk `a_failure_midway_through_the_apply_leaves_production_unchanged`, deliberately a **store** walk: through the route the failure is unreachable by design, because the conflict re-check refuses anything that would collide. The walk builds the frozen set by hand with two items sharing a slug; production holds exactly its original rows afterwards and the row is `failed` with an error.)*
- [x] Self-approval is refused for a requester without the deploy permission, and the same person holding the deploy permission can approve (both paths covered by tests).
  *(walk `self_approval_is_refused_without_the_deploy_key_and_allowed_with_it`. **A decision worth recording:** the refusal fires only when the requester *lacks* `deployment.deploy`. A holder may approve their own, because the deploy key *is* the authority that says "I may decide" — refusing would leave a one-person team unable to ship at all. Two-person approval belongs to REQ-069's policy engine, and a half-built version of it does not belong in the core.)*
- [x] Promotion keeps a history row with requester, approver, timestamps and the frozen change set, visible in the Promotions tab.
  *(walk `promotion_history_detail_and_the_gates`: `GET /environments/{id}/promotions` returns the history newest-first with the set's own counts, `GET /promotions/{id}` returns the frozen items, and 403/404 are each proven separately. **The tab is built** (`61c8d1c`): open promotions sort above finished ones, each row carries the frozen counts and a conflict count in the attention tone, and the expand reads the record's own step log. It has not been through the browser pass yet — see the slice note below.)*
- [x] `promotion.*` events arrive at an endpoint subscribed to `promotion.*` within the delivery window.
  *(proven over a **real socket**, not over the fan-out row: `a_promotion_reaches_a_subscribed_endpoint_over_a_signed_delivery` binds an ephemeral loopback listener and makes it a stranger — the route emits and enqueues in one process, the runner signs and posts in another, and a fake client would have proved only the first half. The body is checked for the signature header, the event name and the payload; a second walk proves an endpoint subscribed to `promotion.*` receives a promotion and an endpoint subscribed to something else receives none, so the subscription filter is measured rather than assumed.)*
- [ ] The environment chip appears in the panel header while staging is active, the staging banner cannot be dismissed, and staging hosts answer with `X-Robots-Tag: noindex`.
  *(the `noindex` half is done and walked: `a_staging_host_answers_noindex_and_a_production_one_does_not` reads the header on a staging host's `200` **and** its `404`, proves the same host carries no header before the environment exists and none after it is archived, and proves production is never marked. The mark is a **middleware layer**, not a line in the handler, because "is this address staging" is a fact about the host rather than the status — a handler marks its own `200` and forgets the `404`.*
  *(**tick 74: the chip and the banner did not exist.** Three ticks of notes said they "owe the browser pass", which reads as *built and unmeasured*; `grep` for the components found nothing, and the honest reading is that **nobody had built them**. That is a different kind of gap from a red gate, and it is worth naming because a checkbox's unticked state and a REQ's prose describe the same situation in words that invite the wrong conclusion. The word doing the damage in this line is **active**: everything else in this request is about a *named* environment, so a chip could have been ten minutes of reading the URL — and would have been a chip right on one screen and wrong on the other forty. So the selection is explicit and owned (`lib/active-environment.tsx`): the chip is a control that sets it, production is the default *and* the reset, the selection is stored per tenant and a stored id no longer in the list falls back to production, and an archived staging environment is deliberately **not** "in staging" because its content is read-only. The banner carries no dismiss control at all — no ✕, no hide preference, no `localStorage` flag — because a dismissable staging banner is dismissed once on a screen where the answer is not what you are working on and is then gone for the rest of the session, which is precisely the mistake it exists to prevent. `runEnvironmentsDepth` measures all of it: the list opening, the flip to staging, the banner naming the host, **the absence of a dismiss control**, the banner surviving a navigation to an unrelated screen, the selection surviving that navigation, the link reaching the changes tab, and the banner going away only when production is chosen again. It runs *before* the archive step, because an archived environment is defined to report production and measuring after it would prove the opposite. The pass had not yet run when this tick ended: the box again, and recorded as such.)
- [x] All new routes answer `403` without their permission and `404` for another organization's environment — `the_change_set_is_404_for_another_organization_and_403_without_the_key` proves both on the new route; the existing walks cover the other six.
- [x] Archive releases the staging host and leaves the content readable in the archived state.
  *(integration walk `archiving_releases_the_host_and_keeps_the_content`; the browser half is
  still owed by the depth pass below)*
- [ ] The QA walkthrough visits `/environments`, `/environments/new` and `/environments/[id]` with zero high findings.
  _**Still owed, and the reason is now precisely stated.** The pass is scoped `environments`
  and its route is in the walkthrough inventory, so it *will* run when the slot frees; what was
  missing was any way for it to fail. Tick 93 gated the twenty-three ungated claims, so the pass
  can now return a finding the roll-up counts — before which an empty wizard or a banner in
  production would have produced a report identical to a healthy one. The gates are written and
  `node --check` is the only authority that has touched them, which is the same unproven
  position tick 89's assertions were in. **The first environments-scoped pass to reach the box
  must read a red here as a real product defect until proven otherwise.** Not proven, so not
  closed._
  *(`?tab=changes` and `?tab=promotions` are visited and clicked by `runEnvironmentsDepth` as well — the tab strip is a real navigation and an untested tab is an untested screen. **This box stays open because the pass still has not been executed.** Tick 73 established that the five prior ticks' "a sibling held the slot / the disk was at 100%" story was wrong on every count once the harness was actually run: `runEnvironmentsDepth` had never executed a line of its own body. Tick 74 fixed the chip and the banner, extended the pass to measure them, and still could not run it — so the deferral is now the *only* remaining explanation and is recorded as one fact rather than three.)*
  *(**tick 85: the spec names a route the product does not have.** This line has said
  `/environments/new` for twenty-odd ticks and the panel has no such route — the wizard is a
  modal the list opens with `data-env-new`, and `/environments/[id]` is only reachable with a
  real id that a route-list entry cannot carry. So two of the three screens in this criterion
  have never been walkable *by path*, and the wizard's phone layout ("below `lg` the wizard
  becomes a single scrolling form") has never been measured at all: the depth pass drives it,
  at 1280px. The wizard's open state now lives in the URL beside the filters (`?wizard=1`,
  `9213e49d`), which is what makes it linkable, Back-button-correct and measurable at 390px,
  and the mobile route entry carries `expect: "[data-env-wizard]"` so a deep link the view
  ignores is a finding rather than a second screenshot of the list. The detail route is still
  opened by the depth pass with a real id, which is right and unchanged. The box stays open:
  the pass has still not run, and the next scoped run is the environments one.)*

### QA plan

The walkthrough creates a staging environment from a seeded production site, watches the clone progress to `done`, opens staging, edits one page and adds another, then visits
`/environments/[id]` → Changes and confirms both items with the right change types. It continues through `Request promotion` → approve → progress → `done`, then switches back to
production and confirms the edited and added pages are live. It then re-clones, confirms the staging edits are gone after the confirmation dialog, and finally archives the environment.
Controls exercised: wizard steps 1–3 with validation errors, `Exclude archived pages`, estimate display, Changes filters, conflicts-only toggle, typed confirmation, cancel action,
Promotions tab, mobile banner at 390 px. The visual check must see:
a staging banner that visibly differs from production, a progress bar that actually moves, a Changes table with real titles and author names, a conflict badge on a deliberately
conflicted item, and no dead buttons or placeholder text.

### Slices

1. **Environment model + clone.** Migration, environments CRUD (list/create/get/archive), clone job + runner, `/environments` list screen with status and counts.
*Done line:* an operator creates a staging environment, the clone finishes, and the list shows real per-area counts with an honest progress state while it runs.
2. **Staging context + changes.** Environment chip and banner, content screens scoped by environment, `changes` diff endpoint, `/environments/[id]` with Overview and Changes tabs.
*Done line:* editing a page in staging appears in Changes as `updated` with the editor's name, and production is untouched.
3. **Promotion.** Promotion records, approve endpoint, frozen-change-set apply with conflict detection, Promotions tab, `promotion.*` events. *Done line:* a requested promotion is
approved, applied atomically, visible in history, and emitted to a subscribed endpoint.
   **Every screen this slice names is now written (`07464d7`…`5c8870d`):** the Changes tab's
   checkbox column and its selection-scoped `Promote selection`, the promotion dialog with the
   frozen summary, the conflict list, the typed confirmation and the step timeline, and the
   Promotions tab reading the record back. **What keeps the slice open is the gate, and it is
   two things:**
   - the **browser pass** — `runEnvironmentsDepth` grew four claims for this slice (the selection
     names its own scope, the dialog leads with a count, requesting writes a `promotions` row, the
     tab is re-read after a navigation) and **none of them has run yet**: a sibling writer held
     the QA slot for the whole tick. A screen nobody has opened is not a screen that works.

     **Tick 93: those four claims are now gated, which is the step that comes *before* running
     them.** Auditing the pass as source found **23 claims assigned and read by nothing** —
     including every step of the create wizard. A `steps.x = false` differs from `true` in one
     word of `summary.json` and fails nothing, and the `ok` this function returns is only printed
     and stored. So the pass could not have failed whatever the product did, on precisely the
     claims that matter: an empty name must not advance the wizard, a URL where a host belongs
     must be refused on screen, and a clone that copies nothing must not be submittable — the
     mistake `require_areas` exists to prevent, with the browser as the last line of defence.
     Twelve are gated now, plus the one in the chip-and-banner block that had no gate at all
     (`bannerBeforeSelectingStaging`), which is the sharpest of them: a staging banner shown in
     a production session inverts the one thing it exists to say, and every other measurement in
     that block still read true. The pass reads **97% gated**; what remains are tallies, which
     a reader needs in the report and which are facts rather than assertions.

     `scripts/qa/audit-depth-claims.mjs` now measures the whole file (146 of 358 claims gated
     across 27 depth passes, up from 62) so this is a command rather than a habit. It needed two
     corrections before it could be believed — the second was `const steps = {}` crediting every
     claim in a pass because the guard reads `steps`. An audit that reports passes nobody took
     is worse than none, because it is believed.
   - `promotion.*` reaching a **subscribed webhook endpoint** end to end. **Closed in tick 93 —
     and it was already closed, which four ticks of this file did not know.**
     `apps/api/tests/environments.rs:3354`, `a_promotion_reaches_a_subscribed_endpoint_over_a_signed_delivery`,
     is exactly this sentence: a real HTTP server on an ephemeral loopback port, an endpoint
     subscribed to the group `promotion.*` through the store, the promotion requested and approved
     through the real routes, and the delivery driven by the runner's own `run_due`. It asserts
     the receiver saw **exactly** `["promotion.requested", "promotion.completed"]` in that order
     and nothing from the environment lifecycle that passed through on the way; that the
     `completed` event carries the affected ids (`written: 1`, one item) rather than re-emitting
     `page.published` per copied row; that every delivery's HMAC **verifies over the bytes the
     receiver received** against the endpoint's own secret; and that a second endpoint subscribed
     to `environment.*` was **not** called — so the claim "delivered to the subscriber" cannot be
     satisfied by a runner that posts to everyone. This note was stale: the writer read the
     request's wording rather than the suite, and recorded an owed proof that had been running for
     ticks. A spec file is not evidence of what the code does; grep the test name.
4. **Hardening.** Clone cancel/retry, `noindex` on staging hosts, large-site batching, conflict refresh, error states and the archive path. *Done line:* a cancelled clone leaves no
   partial environment marked active, and a conflicted promotion is refused with item-level detail.
   **Two of this slice's four items closed in tick 68** — the `noindex` layer (walk above) and
   the archive path — `archiving_releases_the_host_and_keeps_the_content` for the release, and the
   last leg of the `noindex` walk for the consequence: a released host stops being staging by
   *ceasing* to answer as one, not by being told to. Clone cancel was already covered by
   `cancelling_a_clone_leaves_the_environment_out_of_active`; **large-site batching remains**
   **closed in tick 69** — `a_clone_that_crosses_a_batch_boundary_copies_every_row_exactly_once`
   runs seven pages at a batch of two, which is four windows, and asserts the total **on disk**,
   each page's own revision count, and that production kept its rows. Reading the row count off
   the job row instead is what every earlier walk did, and it is the reason all three defects
   survived: the job's `items_done` is the sum of what each batch reported inserting, so it agrees
   with the runner even when the rows are not there. The slice stays open on its one remaining
   item — the **browser gate** — and with it the REQ: a REQ closes on its last slice, not on the
   one that is easiest to prove.

### Risks / notes

- Honesty in the UI matters more than features here: staging is a *content* environment inside one installation. The wizard and banner say so; claiming infrastructure isolation would be
  a lie.
- Clone cost grows with site size: the runner batches by area with a configurable page size
  (`OMNION_CLONE_BATCH_ROWS`, default 5 000), writes progress after each batch, and refuses to
  clone beyond a configured row ceiling (with a clear message) instead of locking the database.
  *(tick 69: this line described code that did not exist. It does now, and the walk that proves
  it found three defects in its first version — see the slice-4 note.)*
- Media is referenced, not duplicated — staging shows the same files. Editing a media *record* in staging is allowed; replacing the underlying file is not part of this request.
- Promotion conflicts are expected, not exotic; the UI must lead with them rather than hiding them behind a failure toast.
- Concurrent promotions on one environment are serialized: a second request while one is `running` is refused with a named error.
- The frozen change set is the same-artifact principle from `docs/05-VERSIONING.md` §17 applied to content: what the approver saw is exactly what runs.
