# REQ-050 — First-run Onboarding / Setup Wizard

> **Status:** delivered (P10) · **Captured:** 2026-09-25 · **Layer:** platform (`apps/admin` + CLI)
> **Source:** owner directive — "kurulum için onboarding ekranı falan, request olarak ekle"

## Request

A first-run onboarding + installation experience for both humans and servers.

Web wizard (first start on an empty database):

```text
Welcome
  ↓
Create owner account
  ↓
Create organization
  ↓
Create first site (domain)
  ↓
Choose theme
  ↓
(optional) Connect AI provider
  ↓
Choose starter modules
  ↓
Done
```

- Step progress indicator; each step skippable where safe (sane defaults).
- After completion: a getting-started checklist card on the dashboard
  (invite users, publish the first page, set up SMTP, connect a domain, ...).

CLI equivalent (for headless servers — tools/cli per docs/04):

```text
omnion setup      # interactive first-run
omnion doctor     # environment check: database / redis / storage / migrations / config / plugin runtime
omnion migrate    # run migrations
omnion seed       # optional demo data
```

Plus installation documentation: `docker compose up -d` quickstart (docs/02), `.env.example`
template, and a "requirements" section (Postgres, Redis, S3/MinIO).

## Acceptance criteria

- Fresh install → working admin panel in under 5 minutes, zero manual SQL.
- Wizard state is resumable after a page refresh; no dead ends.
- Owner account creation stores hashed credentials and upholds the invariants from
  docs/07-IAM.md §20 (at least one Owner must exist).
- `omnion doctor` gives a per-check pass/fail with actionable hints.
- All screens use Lucide icons — no emoji/symbol glyphs in the UI (project-wide UI rule).

## Notes

- Build phase: **P10** in [`docs/BUILD-BACKLOG.md`](../BUILD-BACKLOG.md).
- Related: REQ-044 (Package Installer — starter module selection), REQ-043 (white-label
  branding during setup), docs/02-ARCHITECTURE.md (Docker), docs/07-IAM.md (owner role).
- Delivered with P10: `crates/onboarding` (the flow), `tools/cli` (`omnion setup`, `doctor`,
  `migrate`), `/api/v1/onboarding` (the wizard's surface), `/setup` in the admin panel and the
  dashboard checklist; migration `0007` carries `sites.theme` and the `onboarding_state`
  singleton. The installation documentation (quickstart, `.env.example`, requirements) is part
  of P14, and the AI provider step records a skip until the AI Hub (P11) can connect one.

## Implementation spec

### Scope (in / out)

**In** — the shipped P10 flow extended with the depth this REQ still owes: starter modules, sample
content, a checklist that keeps ticking after setup, dismissal, and the installation documentation.
One flow, two front doors (the wizard and `omnion setup` both drive `crates/onboarding`), and the
starter-modules step **hands off** to the Package Installer (REQ-044) instead of installing itself.
New state goes into a new migration; `0007` is released and is never edited.

**Out** — update/upgrade tooling (docs/05-VERSIONING.md §12); white-label branding during setup
(REQ-043); the SMTP checklist item until notifications exist (REQ-021); no seeding of non-empty data.

### Screens (UI)

Admin panel; the wizard lives **outside** `RequireAuth` (a fresh installation has no session — its
first step creates one), the checklist card inside it. Routes: `/setup`, `/setup?step=done`, `/`.

- **Steps**: welcome → owner → organization → site (optional domain) → theme → AI provider
  (skippable) → starter modules → sample content → done. The indicator marks done / current / todo;
  a completed step is revisitable read-only, and a repeat POST answers `409`, shown as “already
  done”, not an error.
- **Fields**: owner (name, email, password with a strength hint — never echoed back, never logged —
  plus confirmation); organization (name → live slug, editable, rules inline); site (name, key,
  host-validated optional domain); theme (grid of bundled themes from `GET /onboarding`, previewing
  via the renderer); AI provider (“Connect now” or “Skip”); starter modules (REQ-044 catalogue
  checkboxes, default none); sample content (none / minimal / demo).
- **Resumability**: state is derived from the server on every load, so a refresh, a back button or a
  second tab resumes at the first open step, and a “Resume setup” banner shows while it is unfinished.
- **Checklist card**: label, one-line description, state icon, deep link and Dismiss, with a **Setup
  checklist** entry in the panel that restores it; all done shows a single “You're all set”. Per-step
  validation shows the server's message verbatim, failed steps keep their input, and the mobile
  layout is one column with a sticky primary button (`Enter` advances, `Esc` goes back).
- **CLI**: `omnion setup` prompts mirror the step names and print the same summary;
  `--non-interactive` takes the same values as flags/env for automation.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/onboarding` | Wizard state: steps, checklist, themes, summary | none — open by design |
| POST | `/api/v1/onboarding/owner` | Create the first owner and sign it in | none while no account exists |
| POST | `/api/v1/onboarding/organization` | Create the first organization | first-run session |
| POST | `/api/v1/onboarding/site` | Create the first site (+ optional domain) | first-run session |
| POST | `/api/v1/onboarding/theme` | Choose the first site's theme | first-run session |
| POST | `/api/v1/onboarding/ai-provider` | Record the AI decision (`skipped` / `connect`) | first-run session |
| POST | `/api/v1/onboarding/starter-modules` | Hand the selection to the installer (REQ-044) | first-run session |
| POST | `/api/v1/onboarding/sample-content` | Seed the chosen sample content | first-run session |
| POST | `/api/v1/onboarding/checklist/dismiss` | Hide the dashboard checklist card | first-run session |

The surface keeps its shape: no permission guard, because there is nothing to check against yet —
`owner` refuses once any account exists, later steps require the first-run session and refuse after
completion, and every mutation answers with the fresh status body (that is the resumability).

### Data model

New migration `database/migrations/00NN_onboarding_depth.sql` (number = next free at land time):

- `alter table onboarding_state` — add `starter_modules jsonb not null default '[]'` (array,
  `jsonb_typeof = 'array'`), `sample_content text not null default 'none'` check in
  (`none`,`minimal`,`demo`), `sample_content_at timestamptz`, `checklist_dismissed_at timestamptz`;
  drop and re-create `onboarding_state_ai_step_check` widened to (`pending`,`skipped`,`connected`).
  No new tables: the singleton stays a singleton, and everything else the flow records already
  exists (`organizations`, `sites.theme`, `domains`, `users`).
- `crates/onboarding/src/checklist.rs` gains two derivable items beside the existing six (`account`,
  `organization`, `site`, `first_page`, `first_publish`, `domain`): `team_invited` (more than one
  active user) and `ai_provider` (an enabled provider exists) — computed from the world on every
  read, never stored, so the checklist cannot lie.

### Events

`onboarding.completed` (payload: `organization_id`, `site_id`, `theme`, `sample_content`) fires when
the last step lands, and `onboarding.sample_content_seeded` (`site_id`, `pack`, `page_count`) when
sample pages are created. Both are organization-scoped and nothing is consumed — the checklist is
derived on read, so the first run stays independent of the event bus being healthy.

### Acceptance criteria

- [ ] On an empty database the wizard reaches a working panel in under five minutes with zero
      manual SQL; the timed run is recorded in `docs/BUILD-LOG.md`.
- [ ] Refreshing at any step resumes at the same step, and a second tab shows the same state.
- [ ] The owner's password is stored hashed and never logged or echoed, and the account holds the
      Owner role, satisfying the “at least one Owner” invariant (docs/07-IAM.md §20).
- [ ] `POST /onboarding/owner` answers `409` once any account exists, and every later step refuses
      after completion — both covered by tests that also run post-setup.
- [ ] Skipping the AI provider records `skipped`; the checklist keeps an `ai_provider` item until a
      provider is enabled, then ticks itself off on the next load.
- [ ] Sample content: `none` creates nothing; `minimal`/`demo` create pages that render on the
      public site; a second run creates no duplicates; a non-empty page set is left untouched.
- [ ] The checklist ticks on real change — publish the first page and `first_publish` is done on
      the next dashboard load — and dismissal persists across sessions, with the Setup checklist
      entry restoring the card.
- [ ] `omnion setup --non-interactive` produces the same final state as the web wizard;
      `omnion seed` supports `--dry-run`, refuses a non-empty database without `--force` and is
      idempotent with `--force`.
- [ ] `omnion doctor` prints one line per check with pass/fail and an actionable hint, exits
      non-zero when a required check fails, and never prints a secret value.
- [ ] `.env.example` lists every environment variable the API reads — enforced by a workspace test.
- [ ] `docs/INSTALL.md` documents requirements, the compose quickstart and the upgrade path with the
      commands the CLI actually accepts.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are
      green, with the wizard, checklist card and done screen in the walkthrough inventory.

### QA plan

- The harness resets the database before every pass, so the walkthrough already drives the first
  run; it is extended to finish **all** steps — owner, organization (invalid slug first, to capture
  the inline error), site with a domain, theme, AI provider skipped, starter modules skipped, sample
  content `minimal` — then the done screen and its three next actions.
- Checklist pass: screenshot the card, click an item through to its screen, dismiss and restore it;
  the review must see a distinguishable progress indicator, a legible inline error and no clipped
  fields at 390 px.
- `scripts/qa/probe-onboarding.cjs` (new, exits non-zero) asserts what a screenshot cannot: a
  mid-wizard refresh resumes; `POST /onboarding/owner` after completion returns `409`; sample pages
  render on the public site; dismissal survives a new session; a second sample run creates no
  duplicates, and the mobile pass completes the first three steps at 390 px.

### Slices

1. **Checklist depth + dismissal + sample content** — migration, checklist items, sample packs, API
   routes, panel UI. *Done when:* a fresh install seeds `minimal` content and dismissal survives.
2. **Starter modules + AI provider step** — selection handed to REQ-044 with progress and failure
   states, `connected` recording. *Done when:* the wizard hands a selection to the installer, shows
   its result and completes regardless of the outcome.
3. **CLI depth** — `omnion seed` (`--dry-run`, non-empty guard, `--force`), `doctor` additions
   (storage write probe, pending-migration count, redis reachability, config completeness) and
   `--non-interactive` setup. *Done when:* doctor exits non-zero on a broken storage config.
4. **Installation documentation** — `docs/INSTALL.md`, `.env.example`, the env-coverage test and a
   recorded clean-room run. *Done when:* the coverage test is green and the documented quickstart
   commands are the ones the CLI accepts.

### Risks / notes

- The wizard is the only unguarded HTTP surface in the product; its protection is entirely “no
  account exists yet” plus the first-run session rule. Any future route added here must inherit that
  reasoning rather than inventing its own.
- Documentation drifts; the `.env.example` coverage test is the automated guard, and the documented
  clean-room run is the other, since compose cannot run inside the QA harness.
- Sample content must be obviously sample: documented naming, ids recorded in `onboarding_state` so
  it can be removed later. The SMTP checklist item stays off the list until notifications exist —
  a placeholder that can never complete would train operators to ignore the card.
