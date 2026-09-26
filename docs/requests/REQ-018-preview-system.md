# REQ-018 — Preview System

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/web` + core
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Before publishing:

```text
[Preview as Desktop]
[Preview as Tablet]
[Preview as Mobile]
[Preview as Google]
```

Plus: view the site exactly as a visitor sees it.

## Implementation spec

A draft is currently invisible until it is published. This request makes a draft renderable in three ways — inside the panel in
device frames, on the public renderer through a signed short-lived link, and through an SEO inspector that answers "what would a
search engine index?" — without ever exposing an unpublished revision to an anonymous visitor who lacks a valid token.

### Scope (in / out)

**In**

- **Draft preview** of any revision (working draft or a historical revision) rendered by the real public renderer, so "looks like
  a draft" can never diverge from "looks published".
- **Device frames**: Desktop 1440×900, Tablet 834×1112, Mobile 390×844, plus zoom 50/75/100 and a dark/light toggle when the
  active theme supports it. Honest labelling: frames are viewport sizes, not device emulators.
- **Shareable preview links**: a token scoped to one page + one revision + expiry, mintable from the preview screen, optionally
  requiring an Omnion sign-in, revocable at any time, counted, and never cached by shared caches.
- **"Preview as Google"**: a search-result snippet preview plus deterministic checks (title, description, canonical, H1 count,
  image alt coverage, Open Graph/Twitter tags, JSON-LD presence and validity, robots directives, rendered text extraction).
- **Preview center** listing active links with usage and revocation.
- **Locale and revision switch** inside the preview screen, so a translator can check their own locale before publishing (uses
  REQ-020's locale data when it exists, otherwise only the default).

**Out**

- Screenshot diffing, visual regression baselines and pixel comparison tooling.
- A/B testing and audience targeting (REQ-060 Marketing), comment threads on drafts (REQ-064).
- Cache pre-warming or CDN purge (REQ-011), publishing approvals (REQ-059).
- Pixel-accurate device emulation (touch gestures, device font scaling) — explicitly not claimed.

### Screens (UI)

- **`/pages/[id]/preview` — device preview.** Split layout: a left control rail (Device switcher, Zoom, Revision selector —
  `Working draft`, `Published revision v12`, and the last 10 historical revisions, Locale switcher when translations exist, Theme
  override, Dark/light toggle) and a right stage with a device frame, a size label under it (`1440 × 900`), and the renderer inside
  an iframe. Actions in the rail: `Reload` , `Open in new tab`, `Copy preview link`, `Share preview link…`, `Back to editor`.
  Keyboard: `1`/`2`/`3` switch devices, `r` reloads, `l` opens the share dialog, `Esc` returns to the editor.
  The frame is scrollable inside its own bounds so panel scrolling never moves the page under test.
- **`/pages/[id]/preview/seo` — Preview as Google.** Left column: a SERP card that mirrors the platform's own search UI style
  (site name, breadcrumb from the slug, title in blue, description, URL line), with character counters for title (target 30–60) and
  description (target 120–160). Right column: a checks list, each check with pass/warn/fail state and a one-line explanation — Title
  present and within range, Description present and within range, Canonical URL set, Exactly one H1, Images with alt text (`12 / 14`),
  Open Graph tags complete, Twitter card tags complete, JSON-LD present and parseable, Robots directives not blocking indexing,
  Rendered text length within reason;
  and a JSON-LD viewer that shows parse errors inline instead of failing silently.
- **`/preview` — preview center.** Table columns: Target (page title + slug + revision), Locale, Created by, Expires at
  (countdown), Views, Status (active / expired / revoked), actions (Open, Copy link, Extend 24 h, Revoke). Filters: status, target
  search, created-by, window. Bulk: Revoke selected (typed confirmation). Empty state:
  "No preview links yet — open a page preview and share one."
- **Public preview route.** `apps/web` serves `/preview/{token}`: renders the revision from the API in the site's active theme,
  prints a top banner `Draft preview — not published` with the revision number and locale, and answers `X-Robots-Tag: noindex, nofollow`
  plus `Cache-Control: private, no-store`. An expired or revoked token renders a calm explanation page (never a stack trace or an
  empty document) with a `Request a new link` hint;
  a link that requires sign-in redirects to the panel login and returns to the same URL afterwards.
- **States.** Iframe loading: a blurred placeholder with a spinner and the revision number. Render failure: an inline panel inside
  the stage with the API error code and `Reload`. Missing revision: `Choose a revision to preview`. Below `lg`:
  the rail collapses into a bottom sheet, one device frame is shown at a time, and the size label stays visible.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/pages/{id}/preview-tokens` | Mint a preview link for one revision (`revision_id`, `expires_in`, `require_sign_in`) | `content.pages.read` |
| GET | `/api/v1/pages/{id}/preview-tokens` | Links of one page | `content.pages.read` |
| GET | `/api/v1/preview-tokens` | Preview center list (status, creator, window, cursor) | `content.pages.read` |
| POST | `/api/v1/preview-tokens/{id}/extend` | Extend expiry by a bounded amount (≤ 30 days) | `content.pages.update` |
| DELETE | `/api/v1/preview-tokens/{id}` | Revoke a link | `content.pages.update` |
| GET | `/api/v1/preview/{token}` | Payload for the preview renderer: site, page, revision, locale, alternates | token capability (no session permission) |
| GET | `/api/v1/pages/{id}/preview/seo` | SEO inspection for one revision (`revision_id`, `locale`) | `content.pages.read` |
| GET | `/api/v1/pages/{id}/preview/render` | Panel-side preview payload for a signed-in editor (no token needed) | `content.pages.read` |

Token handling: the response of `POST …/preview-tokens` carries the plaintext token exactly once; the database stores only a
SHA-256 hash.
`GET /api/v1/preview/{token}` is rate limited per token and per IP, never logs the token, and answers the same `404` shape for
unknown, expired and revoked tokens.

### Data model

Migration `0013_preview_tokens.sql` (number is a placeholder — renumber to the next free slot):

- `preview_tokens` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null references organizations(id) on delete cascade`,
  `site_id uuid not null references sites(id) on delete cascade`, `page_id uuid not null references pages(id) on delete cascade`,
  `revision_id uuid not null references page_revisions(id) on delete cascade`, `token_hash text not null unique check (length(token_hash) = 64)`,
  `label text`, `require_sign_in boolean not null default false`, `max_views integer check (max_views between 1 and 10000)`,
  `view_count integer not null default 0 check (view_count >= 0)`, `expires_at timestamptz not null`, `revoked_at timestamptz`,
  `revoked_by uuid references users(id) on delete set null`, `last_viewed_at timestamptz`, `created_by uuid references users(id) on delete set null`,
  `created_at timestamptz not null default now()`, constraint `expires_at > created_at`, constraint `view_count <= coalesce (max_views, view_count)`.
- Indexes: `(organization_id, created_at desc)`, `(page_id, created_at desc)`, `(expires_at) where revoked_at is null`,
  `(token_hash)` (unique, already implied).
- Privacy: no IP address, user agent or geolocation is stored for a preview view — only the counter and the last-view timestamp.
  This is a deliberate limit and is stated in the panel help text.
- SEO inspection has no table: it is computed per request from the revision (title, summary, body), the site settings and the
  active theme's metadata contract, and is cached in memory for 60 seconds per `(revision_id, locale)`.

### Events

- **Emitted:** `preview.link.created` (page, revision, expiry, require-sign-in), `preview.link.revoked`, `preview.link.expired`
  (from the expiry sweeper, batched per organization), and `preview.seo.checked` only when the caller asks for it explicitly (kept
  quiet by default to avoid feed noise).
- **Consumed:** nothing.
- **Webhook relevance:** a marketing or QA system can subscribe to `preview.link.*` to announce a review round or clean up its own
  records; `preview.link.expired` is what REQ-021 turns into a low-priority notification. Payloads carry page/revision ids and the
  token id — never the token.

### Acceptance criteria

- [ ] Every page detail screen has a `Preview` action that opens `/pages/[id]/preview` and renders the working draft, not the
  published revision.
- [ ] Switching Desktop / Tablet / Mobile changes the frame size and the label, and the iframe re-renders at the new viewport
  width.
- [ ] The revision selector lists the working draft, the published revision and the last 10 historical revisions; selecting a
  historical revision renders that revision's content.
- [ ] Zoom 50 / 75 / 100 changes the rendered scale without breaking layout; dark/light toggle follows the theme's capability and
  is hidden when the theme has no dark mode.
- [ ] A preview link created with `require_sign_in = false` renders in a browser session with no Omnion cookie, and its response
  carries `X-Robots-Tag: noindex` and `Cache-Control: private, no-store`.
- [ ] A token created with a 1-hour expiry answers `410`-style "expired" explanation after its expiry (or `404` with the shared
  shape) and the preview center shows the row as `expired`.
- [ ] Revoking a token makes the same URL stop rendering immediately (no cache serves it).
- [ ] A `require_sign_in = true` token redirects an anonymous visitor to the panel login and returns to the preview URL after a
  successful sign-in.
- [ ] The preview URL never appears with the token in a `Referer` header sent to third parties (asserted by a test on the
  renderer's referrer policy).
- [ ] `GET /api/v1/pages/{id}/preview/seo` returns pass/warn/fail for all ten checks, and a crafted page with a 20-character
  title, no description and a broken JSON-LD block reports exactly those failures with readable explanations.
- [ ] "Preview as Google" renders the SERP card with the page title, description and slug, and the counters turn amber outside the
  target ranges.
- [ ] Preview center lists links with creator, expiry countdown and view count, and bulk revoke removes selected links after the
  typed confirmation.
- [ ] Minting more than 10 links per page per hour is refused with a named rate-limit error.
- [ ] Requests carrying an unknown, expired or revoked token all answer the same body shape and status, disclosing nothing about
  which case applied.
- [ ] Preview of a staging environment's revision (REQ-017) works with the same screens and shows the staging host in the frame
  label.
- [ ] The QA walkthrough inventory covers `/pages/[id]/preview`, `/pages/[id]/preview/seo` and `/preview` with zero high findings.

### QA plan

The walkthrough opens a seeded page, edits the draft, opens Preview, cycles all three device frames and the zoom levels, switches
to a historical revision and back, then opens "Preview as Google" and reads the checks (at least one deliberate failure must be
visible and explained). It copies a preview link, opens it in a fresh browser context with no session to prove anonymous
rendering, inspects the noindex header, then revokes the link in `/preview` and reloads the URL to see the revoked state. It
finishes on mobile width at 390 px, where the rail becomes a bottom sheet. The visual check must see:
real page content inside every frame (never a blank iframe or a JSON dump), a device frame that actually changes width, the draft
banner on the public preview, readable SEO explanations rather than bare icons, and no dead buttons.

### Slices

1. **Panel preview + preview payload.** `preview/render` endpoint, `/pages/[id]/preview` with device frames, zoom, revision
selector and reload; iframe CSP allowances (`frame-ancestors` for the panel origin only).
*Done line:* an editor can preview the working draft on all three frames and switch between draft, published and two historical
revisions.
2. **Shareable links + preview center.** Token table and minting, `GET /api/v1/preview/{token}`, public `/preview/{token}` in
`apps/web` with banner + noindex + no-store, share dialog (expiry, require sign-in), `/preview` list with revoke/extend, expiry
sweeper.
*Done line:* a link shared to a signed-out browser renders the draft with a visible banner, and revoking it stops the render.
3. **Preview as Google.** SEO inspection endpoint with the ten checks, SERP card, checks list, JSON-LD viewer, caching. *Done
line:* a deliberately under-optimized page reports its failures with explanations, and a well-formed page reports all pass.
4. **Staging + locale integration.** Preview picks up the active environment (REQ-017) and locale (REQ-020), frame labels name
both, and the QA walkthrough covers the combined path.
*Done line:* a staging revision of a translated page previews with the right environment and locale labels on all three frames.

### Risks / notes

- Token leakage is the main risk: tokens travel in a URL, so nothing may log them, the renderer must set a strict referrer policy,
  and the hash-only storage must be asserted by a test.
- Iframe embedding requires an explicit CSP `frame-ancestors` allowance for the panel origin; opening this up to `*` is forbidden
  and noted in the code review checklist.
- Preview must never be cached by a shared cache: `private, no-store` is asserted in tests for both the panel payload and the
  public token route.
- "Preview as Google" is an inspector, not a ranking simulation — the UI wording must avoid implying Google position or indexing
  guarantees.
- Frames are viewport sizes; touch emulation, device pixel ratio and font scaling are out of scope, and the help text says so.
- Rendering the draft goes through the real renderer, so a theme that crashes on malformed content must fail inside the frame with
  a readable error — never take the panel down with it.
