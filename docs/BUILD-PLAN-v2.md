# Omnion — Build Plan v2 · Feature Waves

> **Status:** active · **Created:** 2026-09-26 · **Owner:** Furkan ERMAĞ
> The platform core (P00–P14) is complete. This plan defines the wave structure, the acceptance
> gates and the execution model for building **every feature in the owner's brief**, one
> verifiable slice at a time. Backlog counterpart: [`BUILD-BACKLOG.md`](BUILD-BACKLOG.md)
> (P00–P14, closed).

## 0. Goal

**"Omnion = an open-source enterprise application platform with a powerful CMS at its core."**
WordPress's CMS side + Odoo's business modules + n8n's automation + an AI agent layer +
a low-code studio + enterprise IAM + a marketplace — all on one core.

The owner's bar is explicit: **the panel must be detailed, the features must be complete, and it
must feel substantial.** A feature is therefore not "done" until its screens genuinely work and
a human can use them end to end.

## 1. Execution model

| Role | Who | Rule |
|---|---|---|
| **Sole code writer** | `omnion-build` loop (cron `4cb22957038f`) | One REQ (or one slice of a large REQ) per tick |
| **Acceptance gate** | `scripts/qa/run.sh` (browser walkthrough + vision review) | Runs at the end of every tick; red means the slice is not finished |
| **Continuous audit** | `omnion-qa` loop (cron `7eeb8f90c9e1`) | **Paused** while features are built; re-enabled once the queue drains |
| **Curator** | main chat session | Plan/priority updates, reports, publishing |

Tick loop:

```text
pick REQ (wave order) → read spec + related docs → implement (API + data + UI + tests)
  → cargo test / pnpm typecheck && build → bash scripts/qa/run.sh (browser + vision)
  → tick the acceptance checklist → atomic English commit + push → REQ status: done(<commit>)
  → BUILD-LOG + ledger → next REQ
```

## 2. Definition of Done

A REQ closes only when **all** of the following hold:

1. **Real, complete UI:** list + detail + create/edit/delete, empty state, populated state,
   validation messages, loading/error states, keyboard use, mobile layout.
2. **API:** endpoints are permission-guarded (a permission name exists), errors are meaningful,
   input is validated.
3. **Data model:** ships as a migration; never breaks existing rows.
4. **Tests:** unit + integration; E2E for the critical flow.
5. **QA pass:** the new screens are visible in the walkthrough inventory and get clicked
   (zero high findings).
6. **Evidence:** QA screenshots + a BUILD-LOG entry + a ticked acceptance checklist in the REQ.
7. **Forbidden:** dead buttons, fake data, hidden/disabled features, "coming soon" placeholders.
8. **Language rules:** public repo — no sensitive/adult vocabulary; docs and commits in English.

## 3. Waves

### Wave 1 — Panel depth (the visible core) ← **starting point**

The direct answer to "the panel isn't detailed": navigation, search, notifications, user
management, file manager and the system centres.

| REQ | Work | Why first |
|---|---|---|
| REQ-002 | Global search engine + **⌘K command palette** | Reach everything from anywhere |
| REQ-032 | Command centre (palette + quick actions + recents) | Same backbone, daily use |
| REQ-007 | Analytics + real dashboard (charts, metrics) | Live data instead of an empty shell |
| REQ-006 | IAM: user/role/permission screens + sessions & devices | Enterprise prerequisite |
| REQ-010 | Enterprise file manager (grid, preview, folders, versions) | Deepens the media screen |
| REQ-021 | Notification centre (in-app + e-mail) | User feedback loop |
| REQ-016 | Webhook + event management screen (deliveries, redeliver) | Visibility + debugging |
| REQ-012/013/014 | Security / Backup / System health centres | Admin control |
| REQ-022 | Developer portal (API keys, embedded API explorer, usage) | Integration prerequisite |
| REQ-050* | Onboarding depth (step checklist, sample content) | First-run experience |

### Wave 2 — Content & CMS depth

| REQ | Work |
|---|---|
| REQ-063 | Block system + visual page builder (drag & drop, block settings, templates) |
| REQ-064 | CMS depth pack: menus, forms, SEO toolkit, redirects, scheduled publishing, comments, newsletter, memberships |
| REQ-062 | Ten default themes + theme settings + theme builder (docs/03) |
| REQ-019 | Headless CMS: content API, field selection, webhook triggers |
| REQ-018 | Preview system (draft preview, shared preview links, device modes) |
| REQ-020 | Globalization: multilingual content, translation workflow, locales, RTL |
| REQ-031 | Import/export centre (CSV/JSON, mapping, dry run) |
| REQ-029 | PDF/document generation (template + data → PDF, queue) |
| REQ-026 | Dynamic data model (user-defined content types + fields) |

### Wave 3 — Automation & AI

| REQ | Work |
|---|---|
| REQ-003 | Automation engine depth (trigger/condition/action library) |
| REQ-004 | Visual workflow builder (node canvas, branching, error paths) |
| REQ-046 | AI workflow builder (prompt nodes, model choice, human approval) |
| REQ-045 | AI app builder (natural language → tables/forms/panels) |
| REQ-042 | AI copilots (context-aware help in every module) |
| REQ-047 | AI admin (usage, cost, safety, audit) |
| REQ-001 | AI engine (provider pool, RAG, agents, tool calling) |
| REQ-015 | Integration hub (ready connectors: Stripe, Slack, e-mail, …) |
| REQ-041 | Real-time platform (WS/SSE streams, live counters, presence) |

### Wave 4 — Business modules (the Odoo side)

| REQ | Work |
|---|---|
| REQ-051 | CRM (contacts, companies, deals, pipeline board, activities) |
| REQ-052 | Sales & quotes (catalog, price lists, quote → order, approvals) |
| REQ-053 | Inventory & warehouse (items, movements, low-stock alerts, barcodes) |
| REQ-054 | Accounting (chart of accounts-lite, invoices, payments, expenses, reports) |
| REQ-055 | HR (employees, leave, attendance, documents, org chart) |
| REQ-056 | Projects & tasks (kanban, milestones, timesheets) |
| REQ-057 | Calendar & appointments (views, booking page, reminders) |
| REQ-058 | Documents & knowledge base (folders, versions, wiki, search) |
| REQ-059 | Approvals (multi-step chains, inbox, audit) |
| REQ-060 | Marketing (segments, campaigns, e-mail, UTM, A/B-lite) |
| REQ-061 | Manufacturing (BoM, work orders, stock consumption) |

### Wave 5 — Platform & enterprise

REQ-005 (tenant depth), REQ-011 (CDN/edge), REQ-017/034 (sandbox/staging), REQ-024 (deployment
centre), REQ-033 (internal developer platform), REQ-035 (multi-region), REQ-036 (air-gapped),
REQ-037 (secrets manager), REQ-038 (compliance centre), REQ-039 (advanced audit), REQ-040 (API
gateway), REQ-043 (white-label), REQ-044 (package installer), REQ-023/048 (marketplace),
REQ-025/027/028/030 (studio, dashboard builder, BI, e-signature), REQ-049 (Omnion Studio).

### Wave 6 — Deployment (P-DEP) + hardening

Development deployment on `omnion.fermag.com.tr` (nginx + pm2 + TLS), security hardening,
backups, monitoring, load test, documentation site, versioning (VERSION/release).

## 4. Acceptance gates (automatic, every tick)

```bash
cargo test --workspace              # Rust side
pnpm typecheck && pnpm build        # web side
bash scripts/qa/run.sh              # browser walkthrough + vision review (zero high findings)
```

A new screen that never shows up in the walkthrough inventory is not accepted: the harness is
extended instead ("no untested screen" rule).

## 5. Scale and tempo

- Tick length: 15–30 min; a REQ takes 1–3 ticks on average.
- 64 REQs × ~2 ticks ≈ 130+ ticks; with the 5–10 min scheduler → **days** of continuous building.
- The loop pauses itself when the queue drains; "devam" resumes it where it left off.

## 6. Monitoring and reporting

- `docs/BUILD-LOG.md` — per-tick journal (what was done + proof + next).
- `docs/qa/QA-LATEST.md` + `docs/qa/ISSUES.md` — quality state.
- `Status` line in every REQ file — work-item state.
- After every wave: a summary report on notes.fermag.com.tr with before/after screenshots.

## 7. Status (2026-09-26)

- ✅ Core P00–P14 (169 commits, 12 crates, 3 apps, 332 tests, CI green).
- ✅ QA loop built; in its first four hours it found 5 defects (2 high), fixed and proved 4.
- ⏳ With this plan: the request queue grew to 64 work items, each being expanded into a detailed
  implementation spec; the build loop starts at Wave 1 (panel depth).
