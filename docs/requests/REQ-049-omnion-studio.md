# REQ-049 — Omnion Studio *(headline)*

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** new `apps/studio`
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

The unifying studio — every builder in one place:

```text
OMNION STUDIO

├── Data Model
├── App Builder
├── Page Builder
├── Workflow Builder
├── AI Agent Builder
├── Dashboard Builder
├── Form Builder
├── Role Builder
├── API Builder
└── Plugin/Theme Builder
```

## Notes

- Studio is the UX home for REQ-025/026/004/027/046 and the Theme Builder
  (docs/03-FRONTEND.md); every builder shares the same permission model and preview system
  (REQ-018).

## Implementation spec

### Scope (in / out)

**In**

- A new app `apps/studio` (Next.js, sharing `packages/ui`, the session and the organization/site
  scope with the panel): one shell, one navigation, ten builders reachable from one place.
- A **code-declared builder registry** in `packages/studio-kit` — the single source of truth for
  which builders exist, where each lives, which permission opens it, and whether Studio *hosts* it
  or *links* to the panel.
- Studio overview with permission-aware tiles, a command palette and a recents rail; a builder
  index page per entry; a preview pane with device modes that reuses the preview system's URLs
  (REQ-018) rather than inventing a second renderer.
- One-time **handoff tokens** so one builder can prefill another (generated draft → builder,
  page → theme preview) without stuffing state into a URL, plus “Open in Studio” entry points in
  the panel and recents recorded whenever an artifact is opened.

**Out**

- Re-implementing any builder: each stays owned by its REQ (data model REQ-026, app REQ-025, page
  REQ-063, workflow REQ-004, agents REQ-001, dashboards REQ-027, forms REQ-064, roles REQ-006,
  API REQ-022/040, plugins/themes REQ-023/062). Studio hosts or links; it never forks.
- Plugin-contributed builders in v1 — the registry is code because a data-driven registry would let
  a row point users at an unguarded route.
- Mobile authoring: Studio is desktop-first (≥ 1024 px); below that it explains itself and links
  back to the panel. White-labelling the shell (REQ-043) is also out.

### Screens (UI)

`apps/studio`, same session cookies as the panel (same-site), redirecting to the panel sign-in when
the session is missing.

| Route | Purpose |
|---|---|
| `/` | Overview: the ten builders as tiles in four groups, palette, recents rail |
| `/builders/[key]` | Builder index: what it is, status, recent artifacts, docs link, open button |
| `/recent` | Full recents list with a kind filter |
| `/h/[token]` | Handoff landing: token summary and a single Open action |
| `/preview/[kind]/[id]` | Preview pane with device modes |

- **Registry, one entry per builder** (key, label, permission, v1 state): `data_model`,
  `app_builder`, `page_builder`, `agent_builder`, `dashboard_builder`, `form_builder`,
  `role_builder`, `api_builder` and `extension_builder` are **planned** tiles carrying a Planned
  badge and the owning REQ number; `workflow_builder` links **live** to `/workflows`, and the AI
  Workflow Builder tile turns live at `/ai/workflows` when REQ-046 lands. A tile whose permission
  the caller lacks stays visible but disabled (lock icon, tooltip naming the required permission,
  never silently hidden).
- **Layout**: left rail (grouped nav), top bar with the shared organization/site scope, main canvas
  and a right inspector when a hosted builder needs one; below 1024 px a short notice links into the
  panel.
- **Palette**: `Cmd/Ctrl+K` searches builders, recent artifacts and panel screens, `Enter` opens;
  `g` then `d`/`w`/`p` jumps to builders when available, and tile focus order is the visual order.
- **Preview pane**: sandboxed iframe (no top navigation, no same-origin, no forms), device switcher
  (Desktop 1280 / Tablet 834 / Mobile 390), reload and “open full”; no preview → an explicit note.
- **Handoff landing**: token summary (from → to, artifact title, expiry), a redacted payload
  preview, Open and a “what happens next” note; an expired or used token explains itself and links
  to the source builder. Empty states, skeletons and retry exist throughout; Lucide icons only,
  light and dark mode.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/studio/registry` | Builders with hosting state, permission-filtered | `studio.read` |
| GET | `/api/v1/studio/recents` | The caller's recently opened artifacts (`limit`, `kind`) | `studio.read` |
| POST | `/api/v1/studio/recents` | Upsert an opened artifact (idempotent per user+kind+id) | `studio.read` |
| DELETE | `/api/v1/studio/recents` | Clear the caller's recents | `studio.read` |
| POST | `/api/v1/studio/handoffs` | Mint a one-time handoff token | `studio.read` |
| GET | `/api/v1/studio/handoffs/{token}` | Consume a token (single use, then dead) | `studio.read` |

New catalogue key: `studio.read` (category `studio`) grants nothing on its own — every builder
re-checks its own permission when it opens. Recents are the caller's own rows, which is why writing
them rides the read key rather than inventing `studio.manage`.

### Data model

Migration `database/migrations/00NN_studio_state.sql` (number = next free at land time).

- `studio_recents` — `id uuid pk`; `user_id uuid not null references users (id) on delete cascade`;
  `kind text not null` check in (`data_model`,`app`,`page`,`workflow`,`agent`,`dashboard`,`form`,
  `role`,`api`,`extension`); `artifact_id text not null` (≤ 128); `title text not null` (1–200);
  `href text not null` (≤ 512, starts with `/`); `opened_at timestamptz not null default now()`;
  unique `(user_id, kind, artifact_id)`; index `(user_id, opened_at desc)`.
- `studio_handoffs` — `id uuid pk`; `token_hash text not null unique` (SHA-256 hex of the issued
  token; the raw token is never stored); `from_builder`/`to_builder text not null`; `payload jsonb
  not null default '{}'` (≤ 8 KB, depth ≤ 4); `created_by uuid not null references users (id) on
  delete cascade`; `created_at`; `expires_at timestamptz not null` (default lifetime 15 minutes);
  `consumed_at timestamptz`; index `(expires_at) where consumed_at is null`.
- The registry is **code**, not a table: `packages/studio-kit/src/registry.ts` exports the ten entries
  and both apps import it, so Studio and the panel cannot disagree about where a builder lives.

### Events

| Event | When | Payload |
|---|---|---|
| `studio.handoff_created` | a token is minted | `handoff_id`, `from_builder`, `to_builder`, `expires_at` |

Studio emits that one event and consumes none — it is a navigation surface, so it adds no delivery
surface of its own, and builders hosted in Studio keep emitting their own events unchanged. The
handoff event is organization-scoped and low-volume; webhook subscribers can use it to audit
cross-builder hand-offs in an enterprise setup.

### Acceptance criteria

- [ ] The registry is not a database table: a test asserts `studio_recents` and `studio_handoffs`
      are the only Studio tables, so no row can point users at an unguarded route.
- [ ] `/studio/registry` filters server-side: a session without `workflows.read` gets the Workflow
      Builder tile disabled, not missing, and never receives its `href`.
- [ ] Every tile behaves: live tiles navigate to a working screen; planned tiles show the badge and
      the owning REQ number and lead nowhere dead (no 404, no blank page).
- [ ] Opening an artifact records a recent; re-opening it updates `opened_at` instead of inserting
      a duplicate (asserted at the database level), and recents stay per user.
- [ ] A handoff token works exactly once — the second use answers `410` with an explanation — and
      expires after 15 minutes; the raw token never appears in the database (only `token_hash`),
      and a payload over 8 KB or deeper than 4 levels is refused with `422`.
- [ ] “Open in Studio” round trip: a panel screen opens Studio with the artifact recorded as a
      recent, and the handoff landing shows the payload summary before the builder opens.
- [ ] The preview pane renders a published page through the preview system's URL in all three
      device modes inside a sandboxed iframe; an artifact without a preview says so explicitly.
- [ ] Below 1024 px Studio shows the small-screen notice instead of a broken canvas, and the notice
      links into the panel.
- [ ] Keyboard and access states hold: `Cmd/Ctrl+K` opens the palette, `Enter` opens the highlighted
      entry, `Esc` closes it, tile focus order matches the visual order, and a signed-out session is
      redirected to the panel sign-in on every route.
- [ ] Studio emits only `studio.handoff_created` (asserted by a test) and consumes nothing.
- [ ] Light and dark mode render without clipped tiles, and no emoji glyphs appear anywhere (Lucide
      icons only).
- [ ] `pnpm typecheck && pnpm build` covers `apps/studio`, `cargo test --workspace` stays green and
      `bash scripts/qa/run.sh` passes with every Studio route in the walkthrough inventory.

### QA plan

- The walkthrough gains a Studio segment: open it from the panel nav, click **every** tile (live
  ones navigate, planned ones show the badge), open the palette, filter recent artifacts, open a
  workflow and confirm it appears under `/recent`.
- Permission pass: sign in with a role lacking `workflows.read` and screenshot the disabled tile
  with its lock tooltip — the review must see a legible tooltip and no missing-tile gap.
- Handoff pass: create a handoff from the panel, follow it into `/h/[token]`, then re-open the same
  URL to prove the single-use state renders the expired explanation, not an error page.
- Preview pass: open a published page, switch through the three device modes and screenshot each —
  the review must see the artifact framed without double scrollbars; mobile pass at 390 px shows the
  small-screen notice with no horizontal overflow. `scripts/qa/probe-studio.cjs` asserts registry
  filtering, recents upsert semantics, and handoff single-use and expiry.

### Slices

1. **Registry + API** — registry, both tables, the six routes, `studio.read` key, tests. *Done
   when:* `cargo test --workspace` is green and two roles receive different registry payloads.
2. **Studio shell + overview + palette + recents** — new app, shared layout, tile grid with
   planned/disabled states, palette, recents rail, small-screen notice. *Done when:* the walkthrough
   walks every tile with zero high findings on Studio.
3. **Handoffs + preview pane + panel entry points** — mint/consume end to end, `/h/[token]`,
   `/preview/[kind]/[id]`, “Open in Studio” in the panel. *Done when:* the probe passes and the
   preview pane shows a published page in three device modes.
4. **Host contract + first hosted screen** — `BuilderFrame` in `packages/studio-kit` (session,
   scope, inspector slot) plus the panel's workflow list extracted into a shared component, driven
   by the registry's hosting flag. *Done when:* the workflow list renders inside Studio from the
   shared component and opening it records a recent.

### Risks / notes

- The biggest risk is divergence: two copies of a screen drifting apart. The registry plus shared
  components in `packages/studio-kit` (slice 4) is the answer — never a hand-copied view.
- Declared permissions must match the guarded routes they open; a test compares every live
  builder's declared key with the route's guard where the route lives in this repository, and the
  rest are reviewed when their REQ lands.
- Planned tiles are a promise about ordering — each carries its owning REQ number, so the overview
  doubles as the build map. Handoff payloads carry identifiers and text only, capped in size and
  depth, and the single-use hashed 15-minute token keeps them from becoming a bypass.
