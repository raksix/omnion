# REQ-085 — Design Quality Bar & Accessibility Gate

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform + themes
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The taste floor enforced by tooling, not by opinion.

- Modern SaaS look: solid typography scale, generous whitespace, restrained colour, no gradients-by-default.
- Mobile-first responsive grid; verified at 390/768/1440 in the QA harness.
- Accessibility: contrast, focus-visible, keyboard paths, landmarks, alt text; automated checks in the walkthrough.
- Dark mode as a first-class theme capability; skeletons/loading states; tasteful motion with reduced-motion respect.
- SEO/Open Graph/structured data defaults per page type.

## Implementation spec

### Scope (in / out)

**In**

- `@omnion/tokens` — one source of truth for the visual language, authored as typed data and generated into artefacts: a CSS custom-property stylesheet (`tokens.css`, light and dark), a Tailwind 4 theme preset, and a small JS export for canvas and image generation. Contents: colour roles (surface, text, muted text, border, accent, success, warning, danger, focus ring), typography (families, modular scale ratio and steps, line heights, weights), spacing on a 4px base, radius, elevation, motion (durations and easings), z-index scale, container widths, grid definition and breakpoints (390 / 768 / 1024 / 1440). Both apps and every theme consume the generated artefacts; nothing else defines a colour.
- Taste rules as lint, not taste as taste: a shared lint configuration `packages/eslint-config` (React/TypeScript + a11y) plus repository rules that fail the build — no literal colours outside the token package, no `outline: none`/`outline: 0` without a replacement `focus-visible` ring, no positive `tabIndex`, no click handler on a non-interactive element, `jsx-a11y` recommended rules at error, images require `alt` (explicit empty string when decorative), animation and transition declarations must be reachable behind `prefers-reduced-motion`, and motion that ignores the reduced-motion wrapper is an error rather than a warning.
- Contrast gate in code: a test in `@omnion/tokens` computes WCAG contrast for every declared text/background pair (body ≥ 4.5:1, large text and UI boundaries ≥ 3:1) and fails the build on a violation; the same computation is exposed as `POST /api/v1/quality/contrast` for the theme settings screen, which shows a badge per pair, suggests the nearest passing token value and blocks publish while a serious failure stands (an override is possible, requires a written reason and is audited).
- Automated accessibility in the harness: `scripts/qa/a11y.cjs` runs axe against every route in the walkthrough inventory × viewports 390 / 768 / 1440 × light and dark, records findings with severity, rule, selector and snippet, and additionally performs a keyboard-only pass over the critical flows (sign-in, create and publish a page, activate a theme, install a module) plus a landmarks and heading-outline dump per route. Any serious or critical finding fails the gate, matching the "zero high findings" rule in the build plan.
- Visual gate: `scripts/qa/visual.cjs` captures route screenshots per viewport and mode, diffs them against baselines (including the per-theme baselines of REQ-082), applies a pixel budget per scope, uploads diff artefacts and reports a diff ratio. Baselines update only with an explicit flag and produce a reviewable summary, so a visual change is an event rather than a silent overwrite.
- Performance and layout discipline: budgets per scope (route pattern or theme key) for JavaScript weight, CSS weight, image weight, LCP, CLS and INP, stored as data so raising a budget is a reviewable change; images render with explicit dimensions or a reserved aspect ratio, lazy loading below the fold, modern formats served from the media pipeline; fonts are self-hosted, subset, preloaded where used above the fold and declared with `font-display: swap`.
- Motion rules: a documented set of approved durations and easings from the token package, entrance animations under a threshold, no layout-shifting motion, no parallax or auto-rotating carousels without a pause control, and a harness check that renders with `prefers-reduced-motion: reduce` and asserts that no transform or animation runs while the content still renders fully.
- Dark mode as a first-class capability: every token has a dark value, theme settings may choose the default mode and whether visitors can switch, the visitor's choice persists (cookie plus `prefers-color-scheme` fallback), no hard-coded white or black survives in either mode, and the harness captures both modes for every route.
- SEO and structured data defaults per page type: `apps/web/lib/metadata.ts` is extended into per-type builders (content page, post, product, documentation page, list/index, search, tag, author, 404) producing title with a site template, description, canonical URL, robots rules (preview and staging hosts are `noindex`), Open Graph and Twitter card tags with an image generated deterministically from the site's tokens and page fields, and JSON-LD (`WebSite`, `Organization`, `BreadcrumbList`, `Article`, `Product`, `FAQPage` where an FAQ section exists, `ItemList` for lists). A `sitemap.xml` and `robots.txt` route complete the set, with `hreflang` alternates when localisation is active.
- The gate is reported, not just run: run and finding records are stored (below) and surfaced on `/quality`, and `scripts/qa/run.sh` exits non-zero when a high finding appears so CI and the loop cannot ignore it.

**Out**

- Manual design review as a replacement for the gate. Humans still review look and feel, but the floor is automated; the gate does not judge aesthetics beyond the rules above.
- Cross-browser pixel baselines: Chromium is the baseline browser, and the limitation is documented rather than implied.
- Full screen-reader automation: landmark, name and heading dumps are automated, while a manual screen-reader checklist (VoiceOver/TalkBack on the key flows) remains a human responsibility and is recorded in the QA document.
- WCAG certification or an audit report from a third party; the target is WCAG 2.2 AA behaviour, verified by the gate plus the manual checklist.
- Performance laboratory testing beyond the harness (Lighthouse CI or field RUM dashboards): the walkthrough metrics are the v1 evidence, and REQ-007's analytics may later carry field data.

### Screens (UI)

- **`/quality`** (admin) — the gate dashboard: latest run per kind (accessibility, visual, performance, SEO) with status, counts by severity and the commit it ran against; a per-route table (route, viewport, mode, status, findings, diff ratio, LCP, CLS, INP, JS and CSS weight); links to artefacts (screenshots, pixel diffs, axe JSON, head dumps, budget CSV); trend sparklines over the last runs and a `Run gate now` action where the harness can be triggered.
- **`/quality/findings`** — filterable list (severity, kind, route, theme, status, first seen, last seen) with a detail drawer showing the rule, the selector, the snippet, the artefact and the reproduction steps; `Accept` requires a written justification, records the actor and marks the finding accepted so it stops failing the gate but remains visible.
- **Theme settings, accessibility panel** (REQ-062's customize screen) — one row per declared text pair with the computed ratio, a pass/fail badge, the nearest passing value as a one-click suggestion, and a disabled publish while a serious failure stands, with the reason shown next to the button.
- **`/design-system`** (REQ-081's gallery) — each component gains an accessibility tab: role, accessible name, keyboard path, focus ring behaviour and the contrast of its default colours in both modes.
- **States** — `/quality` renders running, passed, failed, error (the harness could not reach an app) and stale (a run older than the current commit is labelled stale and never shown as green); before the first run it shows an empty state naming the command to run; a finding with an accepted status is visually distinct from an open one.
- **Keyboard** — `/quality` table navigable with arrow keys, `f` focuses the filters, `Enter` opens the finding drawer, `Esc` closes it, `⌘⇧R` triggers a run; each finding names the keyboard path that failed, so the fix is a path, not a guess.
- **Mobile** — `/quality` collapses to cards with the headline numbers first; the report is read-only on phones, and the theme preview keeps its reduced-motion and mode toggles so a reviewer can check both on any device.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/quality/runs` | Recent gate runs (kind, commit, status, counts, artefact base) | `quality.read` |
| GET | `/api/v1/quality/runs/{id}` | One run with per-route results and artefact links | `quality.read` |
| POST | `/api/v1/quality/runs` | Ingest a harness run; idempotent on `(kind, commit, harness version)` | `quality.report` |
| POST | `/api/v1/quality/runs/{id}/findings` | Ingest findings for a run in batches (severity, rule, route, selector, snippet, artefact) | `quality.report` |
| GET | `/api/v1/quality/findings` | Filtered findings with first and last seen, status and acceptance | `quality.read` |
| POST | `/api/v1/quality/findings/{id}/accept` | Accept a finding with a required justification (audited) | `quality.manage` |
| GET | `/api/v1/quality/summary` | Headline numbers for the build gate: high count, trend, stale flags | `quality.read` |
| GET · PUT | `/api/v1/quality/budgets[/{scope}]` | Read · raise budgets per scope (route pattern or theme key) | `quality.read` · `quality.manage` |
| POST | `/api/v1/quality/contrast` | Compute contrast for a set of token pairs, returning ratios and nearest passing values | `themes.read` |

`quality.read`, `quality.report` and `quality.manage` are new (category `quality`) and are added to `crates/permissions/src/catalogue.rs`; `quality.report` is intended for the CI service account so the harness never carries an interactive session. Every handler is guarded with `guards::require(...)`. Findings carry no customer content: snippets are truncated, selectors are sanitised and the intake rejects unknown keys, because a QA pipeline is not a place to leak page data.

### Data model

- Migration `0020_quality_gate.sql` (next free number at merge; theme requests add migrations in merge order):
  - `quality_runs` — `id uuid primary key default gen_random_uuid()`, `kind text not null check (kind in ('a11y','visual','performance','seo'))`, `commit_sha text not null`, `harness_version text not null`, `started_at timestamptz not null default now()`, `finished_at timestamptz`, `status text not null check (status in ('running','passed','failed','error'))`, `counts jsonb not null default '{}'`, `artifact_base text`, unique `(kind, commit_sha, harness_version)`, index `(started_at desc)`.
  - `quality_results` — `id bigserial`, `run_id uuid not null references quality_runs (id) on delete cascade`, `route text not null`, `theme text not null default ''`, `viewport integer not null default 0`, `mode text not null default '' check (mode in ('', 'light', 'dark'))`, `status text not null check (status in ('pass','fail','skip'))`, `findings integer not null default 0`, `diff_ratio numeric(7,6)`, `lcp_ms integer`, `cls numeric(6,5)`, `inp_ms integer`, `js_kb integer`, `css_kb integer`, `artifact text`, index `(run_id, route)`.
  - `quality_findings` — `id uuid primary key default gen_random_uuid()`, `run_id uuid references quality_runs (id) on delete cascade`, `fingerprint text not null` (hash of source, rule, route, selector and theme), `source text not null check (source in ('axe','visual','budget','seo','manual'))`, `severity text not null check (severity in ('high','medium','low','info'))`, `rule text not null`, `route text not null`, `theme text not null default ''`, `viewport integer not null default 0`, `mode text not null default ''`, `selector text`, `snippet text` (truncated to 400 characters), `artifact text`, `status text not null default 'open' check (status in ('open','accepted','fixed'))`, `first_seen_at timestamptz not null default now()`, `last_seen_at timestamptz not null default now()`, `accepted_by uuid references users (id) on delete set null`, `acceptance_note text`, indexes on `(status, severity)`, `(fingerprint)`, `(route, first_seen_at desc)`.
  - `quality_budgets` — `scope text primary key` (route pattern such as `/themes/*` or a theme key), `max_js_kb integer`, `max_css_kb integer`, `max_image_kb integer`, `max_diff_ratio numeric(7,6)`, `max_cls numeric(6,5)`, `max_lcp_ms integer`, `max_inp_ms integer`, `updated_by uuid references users (id) on delete set null`, `updated_at timestamptz not null default now()`, with a check that every numeric column that is set is positive.
- Ingestion is additive per finding fingerprint: an existing open finding has `last_seen_at` and the run reference updated instead of a duplicate row, so the list answers "is this still failing" without counting noise.
- Retention: the last 200 runs per kind are kept along with their results; findings and their acceptance notes are kept indefinitely, because a regression that returns should be obvious. Artefacts live in the QA artefact store, pruned with their run.
- Baselines, budgets and the accepted-findings list live in the repository and the database respectively: baselines are files (reviewable in a diff), budgets are rows (restrictive by default), and each accepted finding records who accepted it and why.

### Events

- `quality.gate.passed` — a gate run finished with zero high findings (payload: kind, commit, counts, artefact base).
- `quality.gate.failed` — a gate run finished with at least one high finding (payload: kind, commit, high count, first three rules, artefact base); this is the event automation and the build loop listen for.
- `quality.finding.accepted` — a finding was accepted with a justification (payload: fingerprint, rule, route, severity, actor).
- `quality.budget.raised` — a budget row was changed upwards (payload: scope, field, previous and new value, actor). Raising a budget is an event on purpose: it is the one change that can hide a regression.
- Ingested results themselves do not emit one event per finding; that would put thousands of rows on the bus for one run.

### Acceptance criteria

- [ ] `@omnion/tokens` generates `tokens.css` (light and dark), the Tailwind preset and the JS export from one source, and both apps plus every bundled theme consume the generated artefacts.
- [ ] The contrast test computes every declared text pair and fails on a seeded violation; no declared pair is below 4.5:1 for body text or 3:1 for large text and UI boundaries.
- [ ] Lint rules are enforced at error level: a literal colour outside the token package, an `outline: none` without a replacement ring, a positive `tabIndex`, a click handler on a non-interactive element, an image without `alt`, and an animation outside the reduced-motion wrapper each fail a fixture file.
- [ ] `scripts/qa/a11y.cjs` runs over every route in the walkthrough inventory × 390 / 768 / 1440 × light and dark, and a seeded violation (a removed label, a removed focus ring, a contrast failure) is caught as a high finding.
- [ ] The keyboard-only pass completes sign-in, page create, publish and theme activation without a pointer, and any element that cannot be reached fails the gate with the path that broke.
- [ ] Landmarks and heading outline per route are dumped and checked: one `main`, a labelled navigation, no skipped heading levels, no duplicate `h1` on a page.
- [ ] Visual baselines exist for the panel routes and for all ten themes (REQ-082) at three viewports and two modes; a deliberate pixel change over budget fails the diff and produces an artefact.
- [ ] A run older than the current commit is reported as stale on `/quality`, and a stale run can never render as green.
- [ ] Budgets are read from `quality_budgets`: a seeded JavaScript or CSS regression fails the budget check, and raising a budget requires a `quality.manage` actor and emits `quality.budget.raised`.
- [ ] With `prefers-reduced-motion: reduce`, no transform, transition or animation runs on the tested routes while all content still renders.
- [ ] Dark mode is verified as a designed mode: tokens have dark values, no hard-coded white or black remains in either mode, the visitor's choice persists, and both modes are captured for every route.
- [ ] Metadata builders emit per page type: title with the site template, description, canonical, robots (with `noindex` on preview and staging hosts), Open Graph and Twitter card tags, and generated OG images that are deterministic across two runs.
- [ ] JSON-LD validates against the schema.org types used, and the emitted block per page type is snapshot-tested (`WebSite`, `Organization`, `BreadcrumbList`, `Article`, `Product`, `FAQPage`, `ItemList`).
- [ ] `sitemap.xml` and `robots.txt` list the published routes of every site and exclude drafts, archived pages and preview hosts.
- [ ] `/quality` and `/quality/findings` render the latest runs and findings with working artefact links, and `Accept` requires a justification, records the actor and is visible in the audit trail.
- [ ] `scripts/qa/run.sh` exits non-zero when a high finding exists, so the build gate cannot be bypassed by a green-looking summary.
- [ ] The QA document explains every rule, how to run the gate locally and how to update baselines, and a failing rule tells the reader what to fix.

### QA plan

- **Walkthrough must click:** run the harness, then open `/quality` and read the headline numbers; open one route row and one artefact (a pixel diff and an axe report); open `/quality/findings`, filter by severity, open a finding drawer, accept a low finding with a note and confirm it leaves the failing set; on a theme settings screen create a deliberate contrast failure and confirm publish is blocked with the reason; toggle reduced motion in the preview and confirm motion stops; park the mouse and complete sign-in, page create and publish by keyboard only.
- **Visual check should see:** consistent spacing and typographic rhythm across the panel and the public pages, visible focus rings everywhere, restrained colour with no gradient-by-default hero, dark mode that holds contrast rather than inverting blindly, skeletons that match their final layout, no visible layout shift while a page loads, and an OG image that renders correctly when unfurled in a preview tool.

### Slices

1. **Tokens and rules** — `@omnion/tokens` with the source, generation into CSS and the Tailwind preset, the JS export, the contrast test and endpoint, and the lint configuration with the token, focus, tabindex, alt and reduced-motion rules; both apps and the bundled themes migrated onto tokens.
   *Done when:* the contrast test fails a seeded violation, the lint rules fail their fixtures, and no literal colour remains in the migrated code.
2. **Harness gates** — `scripts/qa/a11y.cjs` (axe matrix, keyboard pass, landmark and heading dumps), `scripts/qa/visual.cjs` (baselines, budgets, artefacts), the reduced-motion check, the budgets table with its defaults, and the wiring that makes `scripts/qa/run.sh` fail on a high finding.
   *Done when:* a seeded accessibility violation and a seeded pixel change both fail the gate with artefacts, and a clean run passes.
3. **Ingestion and the quality screens** — the three quality tables, run and finding ingestion, summary and budget endpoints, the acceptance flow with justification and audit, stale-run detection, `/quality` and `/quality/findings`.
   *Done when:* a real harness run appears on `/quality` with per-route results and a clickable diff artefact, and an accepted finding stops failing the gate.
4. **SEO, structured data and performance budgets** — metadata builders per page type, deterministic OG image generation, JSON-LD builders with snapshot tests, sitemap and robots with host rules, hreflang alternates, and performance metrics captured per run against their budgets.
   *Done when:* every page type emits a snapshot-tested head with canonical, Open Graph and valid JSON-LD, and the budget check fails a deliberate regression.

### Risks / notes

- A gate that fails on noise gets switched off within a week. Keep thresholds in data, keep captures deterministic (fixed content, animations off, fonts loaded, real-time elements masked, one browser as baseline) and make every finding name its rule, its path and its artefact.
- Automated accessibility checks cover a fraction of real issues: the keyboard pass, the landmarks dump and the manual screen-reader checklist stay in the walkthrough, and the report says what is automated and what is not rather than implying full coverage.
- Contrast overrides are the loophole: they need a written reason, an actor and an audit entry, otherwise "accessibility override" becomes the default answer to a red badge.
- The token package must not become a second design system: themes override tokens and may override components through REQ-081's mechanism; forking either one is a review conversation, not a branch.
- Budgets must measure what a user feels: route-level, cold cache, compressed sizes, and consistent measurement conditions, or the numbers will drift into decoration.
- `/quality` reads stored runs, so the harness must upload before the report is trusted; display stale runs as stale and require a fresh run before a release decision.
- This is a public repository: the rules, the report schema and the local run instructions are part of the project's face, so the QA document must stay good enough that an outside contributor reaches the same conclusion as the maintainers.
- SEO builders drift page by page unless they are centralised: one module per page type with a snapshot test, and no component is allowed to hand-write its own `<head>` tags.
