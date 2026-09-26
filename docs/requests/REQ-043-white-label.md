# REQ-043 — White-label

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Another company can present Omnion as its own product:

- "Powered by Omnion" can be turned off.
- Logo:

```text
Acme Platform
```

- Domain:

```text
admin.acme.com
```

- The marketplace can be branded per organization.

## Notes

- Ties into multi-tenancy (REQ-005) and themes (docs/03-FRONTEND.md).

## Implementation spec

### Scope (in / out)

**In**

- One branding profile per organization: product name, tagline, light/dark logo, favicon,
  login illustration, support and docs links, footer text, and the "Powered by Omnion"
  credit toggle where the applicable license permits.
- Design-token overrides (accent, accent-strong, ink, canvas, surface, quiet, line, success,
  danger) with a WCAG AA contrast check per pairing before save.
- Custom admin hostnames per organization with ownership verification (TXT or CNAME record),
  automatic certificate issuance/renewal, and a single primary hostname that the panel and
  login page resolve their branding from.
- Branding resolution before authentication: `GET /api/v1/public/branding` resolves the profile
  by request hostname so the login screen is already branded.
- Marketplace branding fields stored per organization (display name, colours, banner) and
  consumed by the marketplace listing surface (REQ-048/REQ-023).
- Audit entries for every branding change and domain state transition.

**Out**

- Public site theming (`apps/web` uses the theme engine, REQ-062); branding here is the panel,
  login, e-mail shell and marketplace chrome.
- Per-site branding — scope is the organization; a site inherits its organization's profile.
- Arbitrary custom CSS/JS injection (security); only token overrides are accepted.
- Editing the open-source project's own licence text or attribution requirements.
- E-mail sender domain and SPF/DKIM setup (REQ-021).

### Screens (UI)

Admin routes — `/branding` with tabs (tab held in `?tab=`), scoped by the site switcher's
organization:

- **Identity** tab: form fields — Product name (required, 2–40 chars), Tagline (≤80),
  Logo light and Logo dark (media picker, SVG/PNG, ≤512 KB, 64–1024 px, aspect check),
  Favicon (PNG/ICO/SVG, exactly 32×32 or 180×180), Login background (image, ≤2 MB),
  Support e-mail (valid address), Support URL and Docs URL (https only), Footer text (≤200),
  "Powered by Omnion" credit toggle (with an inline note when the license requires it).
  Right column: **Live preview** card rendering the panel header, sidebar chip and login card
  with the draft values (client-side only until Save).
- **Colors** tab: token list (token name, colour picker, hex input, "Reset to default"), each
  row showing contrast against its paired surface with a Pass/Fail pill; Save is blocked while
  any critical pairing fails. "Import JSON" / "Export JSON" of the token set.
- **Domains** tab: table columns — Hostname, Status (`pending` / `verifying` / `active` /
  `failed` badge), Certificate (none / issued / renewing / expiring / failed + expiry date),
  Primary (radio, exactly one), Last checked (relative), Actions. Filters: status select, text
  search. Bulk: verify selected, copy all hostnames. Row actions: Verify now, Set as primary,
  Remove (confirm; refuses while traffic was seen in the last hour, explains why).
  Add form: hostname input validated with `^(?!-)[a-z0-9-]{1,63}(\.[a-z0-9-]{1,63})+$` and
  globally unique, plus a copyable DNS instruction block (record type, name, value) and a
  "Verify now" button.
- **Marketplace** tab: display name, short description (≤160), banner image, accent colour,
  and a preview of the marketplace card.
- States: no profile yet → form pre-filled with defaults and a "Save to brand this panel"
  hint; loading → skeleton form; save error → banner with the failing field highlighted;
  verification failure → row turns `failed` with the observed DNS answer and a retry action.
- Keyboard: `Cmd/Ctrl+S` save, `Cmd/Ctrl+Shift+P` toggle the preview panel, `Esc` on a dirty
  form asks to discard, `Tab` order follows field order, `/` focuses the domain filter.
- Mobile: tabs become a segmented control, the preview collapses behind a "Preview" button,
  the domains table becomes cards with a status chip and an action sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/branding` | Current organization's branding profile | `branding.read` |
| PUT | `/api/v1/branding` | Upsert the profile (identity, tokens, marketplace fields) | `branding.manage` |
| POST | `/api/v1/branding/assets` | Upload logo/favicon/banner, returns a media id | `branding.manage` |
| GET | `/api/v1/branding/domains` | List admin hostnames | `branding.read` |
| POST | `/api/v1/branding/domains` | Add a hostname (pending) | `branding.manage` |
| POST | `/api/v1/branding/domains/{id}/verify` | Re-check DNS and certificate state | `branding.manage` |
| POST | `/api/v1/branding/domains/{id}/primary` | Make this hostname primary | `branding.manage` |
| DELETE | `/api/v1/branding/domains/{id}` | Remove a hostname | `branding.manage` |
| GET | `/api/v1/public/branding` | Hostname → branding for the unauthenticated login screen | none (public) |

Validation failures return field-level errors; hostname collisions return `409`; the public
endpoint returns the platform defaults when the hostname has no profile.

### Data model

Migration `database/migrations/0013_white_label.sql` (next free number at build time).

- `branding_profiles` — `organization_id uuid pk references organizations(id) on delete
  cascade`, `product_name text not null default 'Omnion'`, `tagline text not null default ''`,
  `logo_media_id uuid references media(id) on delete set null`, `logo_dark_media_id uuid`,
  `favicon_media_id uuid`, `login_background_media_id uuid`, `support_email text`,
  `support_url text`, `docs_url text`, `footer_text text not null default ''`,
  `show_platform_credit boolean not null default true`, `tokens jsonb not null default
  '{}'::jsonb`, `marketplace_display_name text`, `marketplace_description text`,
  `marketplace_banner_media_id uuid`, `created_at`, `updated_at`,
  `updated_by uuid references users(id) on delete set null`.
- `branding_domains` — `id uuid pk`, `organization_id uuid not null references
  organizations(id) on delete cascade`, `hostname text not null`, `status text not null default
  'pending' check (status in ('pending','verifying','active','failed'))`, `is_primary boolean
  not null default false`, `verification_token text not null`, `verification_detail text`,
  `certificate_status text not null default 'none' check (certificate_status in
  ('none','issuing','issued','renewing','expiring','failed'))`,
  `certificate_expires_at timestamptz`, `last_checked_at timestamptz`, `created_at`,
  `updated_at`.
- Indexes: unique `branding_domains_hostname_key (lower(hostname))`; unique partial
  `branding_domains_primary_idx (organization_id) where is_primary`; index
  `branding_domains_status_idx (status)`.
- A background job re-checks non-active hostnames every 15 minutes and renews certificates
  inside 30 days of expiry.

### Events

- Emitted: `branding.updated`, `branding.credit.disabled`, `branding.domain.added`,
  `branding.domain.activated`, `branding.domain.failed`.
- Consumed: `media.deleted` clears any logo/favicon reference that pointed at the removed file
  and reverts the profile to defaults.
- Webhook relevance: domain activation/failure is operationally useful; profile updates are
  low-value but harmless and stay opt-in per endpoint.

### Acceptance criteria

- [ ] Saving a profile changes the sidebar product name, header and login card on reload.
- [ ] The public branding endpoint returns the profile for a verified hostname and defaults otherwise.
- [ ] "Powered by Omnion" toggle hides the credit line in the panel footer and login page.
- [ ] Logo upload rejects oversized or wrong-type files with a field-level message.
- [ ] Token overrides apply live in the preview and after save across panel screens.
- [ ] A failing contrast pairing blocks save and names the pair that fails.
- [ ] Reset to default restores a single token without touching the others.
- [ ] Adding a hostname with an invalid format is refused before submit.
- [ ] A duplicate hostname returns a clear conflict message.
- [ ] Verification marks a correctly configured hostname `active` and a wrong one `failed`
      with the observed DNS answer.
- [ ] Exactly one hostname can be primary; switching primaries is atomic.
- [ ] Removing a hostname with recent traffic is refused with an explanatory message.
- [ ] Marketplace display name, description and banner render on the marketplace card preview.
- [ ] Branding changes and domain transitions appear in the audit log with actor and diff.
- [ ] Loading, empty and error states render on all four tabs.
- [ ] Keyboard save and discard-confirm work; the form is usable at 390 px width.
- [ ] Permission keys `branding.read` / `branding.manage` exist in the catalogue.
- [ ] `cargo test`, `pnpm typecheck && pnpm build` and the walkthrough are green.

### QA plan

- Walkthrough: `/branding` → rename the product to "Acme Platform", add a tagline, upload a
  logo, save, reload and confirm the sidebar/login show the new brand; toggle the credit off
  and confirm the footer changes; open Colors, break a contrast pair and confirm save is
  blocked, then fix and save; open Domains, add an invalid hostname (blocked), add a valid one
  (row appears as `pending`), run "Verify now" against the QA environment and read the state
  message; set it primary; try to remove it.
- Visual check: no layout shift when the preview is toggled, logo areas keep aspect ratio,
  status badges are distinguishable, DNS instruction block is copyable, mobile segmented
  control keeps all four tabs reachable.
- Regression: login page still renders without a profile; `/sites` and `/media` unaffected.

### Slices

1. **Profile + panel branding** — migration `0013_white_label.sql`, `GET/PUT /branding` and
   asset upload, Identity tab with preview, token application to the panel shell.
   **Done when:** a saved rename is visible across the panel and on the login card.
2. **Colors + validation** — token editor, contrast checker, reset/import/export, live preview.
   **Done when:** a failing pairing blocks save and a passing set applies everywhere.
3. **Domains** — hostname CRUD, DNS verification, primary switch, certificate state, removal
   guard, plus marketplace branding fields and their preview. **Done when:** a hostname moves
   `pending → active` through verification and becomes primary without breaking the panel.

### Risks / notes

- Hostname verification must never trust user input: query DNS from the server, store the
  observed answer, and only mark `active` when it matches what the panel expects.
- Certificate automation needs a sane retry/backoff story; a failed issuance must leave the
  hostname `failed` and the previous hostname serving, never a dead panel.
- Token overrides can destroy readability — the contrast gate is the guard and it evaluates the
  real pairings, not just raw luminance.
- Media references must survive deletion of the underlying asset without leaving broken images.
- Branding resolves per request hostname; cache it briefly and invalidate on save to avoid a
  database read on every page load.
