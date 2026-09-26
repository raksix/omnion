# REQ-119 — Notification Channels & Templates

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/notifications`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Reaching people where they are.

- Channels: in-app, e-mail (SMTP), webhook, and a pluggable channel interface (SMS/chat later).
- Template editor per notification type with variables, preview and test send.
- Per-user preferences: which categories, which channels, quiet hours; digest options (daily/weekly).
- Delivery log with retries and failure reasons; unsubscribe handling for outbound e-mail.
- Events from every module can raise notifications without code changes.

## Implementation spec

### Scope (in / out)

**In**

- This REQ is the layer above REQ-021 (Notification Center), which owns the notification record, the preference matrix, quiet hours, the digest
  job and the delivery queue with its retry runner. Here: the versioned **template registry** (per type, channel, locale), the **channel-adapter
  contract**, the render pipeline, outbound e-mail **unsubscribe** compliance, the event→template rule table and the template/channel/delivery
  review screens.
- `crates/notifications/src/templates/`: store, variable schema, renderer (plain text plus a restricted HTML subset), preview and test send,
  locale resolution with fallback (exact tag → base tag → organization default, two hops maximum).
- Channel adapter trait `ChannelAdapter` — `key()`, `capabilities()`, `readiness()`, `send(RenderedMessage)` — registered at boot from code.
  In-app, e-mail (SMTP) and webhook (rides the REQ-016 bus, no second stack) ship here; `sms`/`chat` are interface-only adapters reporting
  `not_ready` until a transport lands.
- Send-time resolution: category → template key → locale → channel → render with the payload → delivery row handed to REQ-021's runner. The
  renderer never sends; the runner never renders.
- Seeded defaults: every category × channel has a published template per migration and `omnion seed`, so a fresh installation delivers readable
  mail with no configuration.
- Unsubscribe: signed per-recipient, per-category token; `List-Unsubscribe` and `List-Unsubscribe-Post` headers; a public unsubscribe page; a
  suppression check before every outbound e-mail. Transactional categories (security, account) are `unsubscribable = false` and the API refuses
  to suppress them, stated in the UI.
- Event→template rules fulfil "any module can raise notifications": a row maps bus event name → template key → recipient rule → variable mapping,
  consumed by REQ-021's router. A module ships one rule and one template; no notification code changes.
- Delivery log: the REQ-021 outbox read paths gain template/channel filters, a failure-reason taxonomy (transport, render, suppressed, invalid
  recipient) and per-template/day statistics.

**Out**

- Notification records, queue mechanics, backoff/retry, push subscriptions, digest, preference matrix, quiet hours — all REQ-021; this REQ must
  not create a second runner.
- Rich WYSIWYG or arbitrary HTML editing; attachments; freehand per-recipient bodies.
- Real SMS/chat transports (contract only), inbound bounce processing, A/B testing.

### Screens (UI)

Admin app, `pages/settings/notifications/*`, plus one public route.

| Route | Purpose |
|---|---|
| `/settings/notifications/templates` | Template list: key, category, channels, locales, version, updated |
| `/settings/notifications/templates/[key]` | Editor: variables, locale tabs, preview, test send, history |
| `/settings/notifications/channels` | Adapter readiness matrix and per-channel configuration |
| `/settings/notifications/deliveries` | Delivery log with failure reasons and stats |
| `/unsubscribe/[token]` | Public signed page: confirm category or all-category unsubscribe |

- **Editor**: variable palette (inserter for `{{ variable }}`), subject, body tabs per channel (HTML tab only where the adapter declares
  capability), locale selector with a "missing — falls back to `<default>`" badge, fixture payload editor, server-side live preview, `Test send`
  to the caller or a typed address.
- **Channels** matrix rows: in-app / e-mail / webhook / sms / chat with state, capability chips and a config form; SMTP host, port and sender are
  stored as secret references (REQ-037) and never rendered in clear; `Verify` runs the readiness probe and writes `readiness`, `readiness_note`,
  `last_checked_at`.
- **Deliveries** columns: time, recipient (masked), category, template+version, channel, status, attempts, failure reason; a row expands to the
  attempt list and the rendered body snapshot; filters for template, channel, status, date range; failed-first ordering; keyset pagination.
- All screens: skeleton/empty/error with retry, field-pinned validation errors, unsaved-changes guard, `Esc` closes panels, keyboard path with
  visible focus, light/dark parity, one column at 390 px.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/notification-templates` | List with `category`, `channel`, `locale`, `q` | `notifications.templates.read` |
| GET | `/api/v1/notification-templates/{key}` | Template with versions and variable schema | `notifications.templates.read` |
| GET | `/api/v1/notification-templates/{key}/preview` | Render against a payload, no send | `notifications.templates.read` |
| POST | `/api/v1/notification-templates/{key}/test` | Test send through one channel | `notifications.templates.manage` |
| PUT | `/api/v1/notification-templates/{key}` | Save a draft version | `notifications.templates.manage` |
| POST | `/api/v1/notification-templates/{key}/publish` | Publish; later sends use it immediately | `notifications.templates.manage` |
| POST | `/api/v1/notification-templates/{key}/revert` | Copy an old version into a new draft | `notifications.templates.manage` |
| GET | `/api/v1/notification-channels` | Adapter readiness and capability matrix | `notifications.channels.read` |
| PUT | `/api/v1/notification-channels/{key}` | Configure an adapter (secret references) | `notifications.channels.manage` |
| POST | `/api/v1/notification-channels/{key}/verify` | Run the readiness probe | `notifications.channels.manage` |
| GET | `/api/v1/notification-deliveries` | Delivery log with filters and stats rollup | `notifications.admin` |
| GET | `/api/v1/notification-rules` | Event→template rules | `notifications.templates.read` |
| PUT | `/api/v1/notification-rules/{id}` | Edit a rule | `notifications.templates.manage` |
| POST | `/api/v1/unsubscribe/{token}` | Public: apply the suppression the token describes | none (token is the grant) |

Errors: `400` unknown variable or malformed body, `403` permission miss, `404` another organization's template, `409` publishing with a required
variable unmapped, `422` body over the channel size cap. New REQ-068 keys, category `notifications`: `notifications.templates.read`,
`notifications.templates.manage`, `notifications.channels.read`, `notifications.channels.manage`; delivery-log reads reuse `notifications.admin`.

### Data model

Migration `database/migrations/00NN_notification_templates.sql` (next free slot at land time; additive-only, commented in the `0009` style). No
transport secret is stored here — only secret-store references.

- `notification_templates` — `id uuid pk`; `key text not null` unique on `lower(key)`, `^[a-z0-9_.]+$`, ≤ 96; `category text not null`; `name
  text not null`; `description text not null default ''`; `unsubscribable boolean not null default true`; `created_by`; `created_at`,
  `updated_at`.
- `notification_template_versions` — `id uuid pk`; `template_id` fk cascade; `version int not null` from 1; `locale text not null` (BCP-47
  shape); `channel text not null` check in (`in_app`,`email`,`webhook`,`sms`,`chat`); `subject text not null default ''`; `body text not null`;
  `body_html text null`; `variables jsonb not null default '[]'` (`{name,type,required,sample}`); `status text not null default 'draft'` check in
  (`draft`,`published`,`superseded`); `published_at`, `created_by`; unique `(template_id, version)`; partial unique `(template_id, locale,
  channel) where status = 'published'` — exactly one live version per locale+channel.
- `notification_channel_adapters` — `key text pk`; `display_name`; `kind text not null` check in (`built_in`,`external`); `capabilities jsonb not
  null default '{}'`; `config jsonb not null default '{}'` (secret references only); `enabled boolean not null default false`; `readiness text
  not null default 'unknown'` check in (`ready`,`not_ready`,`unknown`,`error`); `readiness_note`, `last_checked_at`, `updated_by`, `updated_at`.
- `notification_event_rules` — `id uuid pk`; `event_name text not null`; `template_key text not null` (soft reference: a missing template makes
  the rule inert and the API lists it as a warning); `recipients jsonb not null` (user / role / permission / source-linked actors); `variable_map
  jsonb not null default '{}'`; `conditions jsonb not null default '{}'`; `organization_id uuid null`; `enabled boolean not null default true`;
  `priority int not null default 0`; `created_by`, timestamps; index `(event_name, enabled)`.
- `notification_unsubscribes` — `id uuid pk`; `email_hash text not null` (SHA-256 of the lowercased address; the address itself is not kept);
  `category text not null` or `'*'`; `scope text not null` check in (`category`,`all`); `reason text null`; `created_at`; unique `(email_hash,
  category, scope)`.
- `notification_send_stats` — `day date`; `template_key text`; `channel text`; `sent`, `failed`, `suppressed bigint`; primary key `(day,
  template_key, channel)` — updated from the runner's completion events, never by per-send writes from web requests.
- Extra index if absent (guarded additive alter): `notification_deliveries (channel, status)`.

### Events

| Event | When | Payload |
|---|---|---|
| `notification.template.published` | a version goes live | `key`, `version`, `locale`, `channel` |
| `notification.template.updated` | draft saved or reverted | `key`, `version`, `status` |
| `notification.channel.configured` | adapter saved or probed | `key`, `readiness` |
| `notification.render.failed` | required variable missing | `template_key`, `version`, `reason` |
| `notification.delivery.suppressed` | send skipped | `delivery_id`, `reason` |
| `notification.unsubscribed` | public page applies a token | `category`, `scope` (hashed address only) |
| `notification.delivery.retried` | REQ-021's runner requeues | `delivery_id`, `attempt` |

Consumed: `notification.delivery.succeeded` / `notification.delivery.failed` (REQ-021) update stats and the failure taxonomy; any bus event named
in `notification_event_rules` is consumed by the router — an event with no rule is a documented no-op. Payloads never carry a rendered body or an
address.

### Acceptance criteria

- [ ] Every seeded category × channel has exactly one published template; a fresh installation sends readable e-mail with no configuration.
- [ ] Save → publish → the next send uses the new version without a restart (the delivery row names the new version).
- [ ] Preview output for a payload equals the body recorded on a delivery row for the same payload and version.
- [ ] A missing required variable fails with the variable named in `notification.render.failed`, the delivery marked failed (render) — never a
      blank body.
- [ ] A locale without a template falls back to the organization default and the delivery row records the fallback.
- [ ] Outbound e-mail carries `List-Unsubscribe` and `List-Unsubscribe-Post`; the signed link works signed out; confirming suppresses that
      category and the next mail is recorded `suppressed` with no further retry.
- [ ] Transactional categories cannot be unsubscribed: the API refuses, the page explains, mail keeps flowing.
- [ ] A `not_ready` channel shows its readiness note, test send explains the failure inline, and deliveries to it are suppressed rather than
      retried forever.
- [ ] `notifications.templates.*` / `notifications.channels.*` holders pass; a user without them gets `403` and no nav entry; the delivery log
      requires `notifications.admin`.
- [ ] A bus event from another module produces a notification through a rule and template with **no code change in the producing module** (probe
      publishes the event).
- [ ] A rule pointing at a missing template is inert, listed as a warning, and does not fail the router.
- [ ] HTML bodies are sanitized: script tags and `on*` attributes are stripped, and the sanitizer test enumerates the blocked cases.
- [ ] Revert creates a new draft equal to the old version; published versions are immutable (update refused).
- [ ] Editor keyboard path, light/dark parity and 390 px layout pass with no clipped chips.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; every new screen is in the walkthrough
      inventory.

### QA plan

- Walkthrough: edit the security-alert e-mail template, preview with a fixture payload, test-send to a local SMTP capture, publish, trigger a
  real alert, read the delivery row naming the version; then unsubscribe on the public page and prove the next mail is suppressed with a reason.
- Visual review: three-column editor, channel matrix legibility, delivery table with an expanded attempt list, and all error states.
- `scripts/qa/probe-notification-templates.cjs` (new, exits non-zero) asserts through the API: preview equals body; render failure recorded;
  suppression stops retries; `notification.template.published` and `notification.delivery.suppressed` reach the feed; a rule+template pair alone
  drives a send.
- Signed-out and mobile passes: `/unsubscribe/[token]` works signed out, invalid tokens are refused without revealing why, and the panel
  redirects to sign-in.

### Slices

1. **Template store, renderer, preview API** — migration, templates module, resolution, seeds. *Done when:* `cargo test --workspace` is green and
   preview equals the recorded body.
2. **Editor and channels screens** — templates editor, channels matrix, publish/revert, test send, readiness probe. *Done when:* the walkthrough
   edits, previews, publishes and test-sends; screens are in the inventory.
3. **Unsubscribe, rules, delivery log** — signed tokens, public page, suppression, event rules, deliveries screen with stats. *Done when:* the
   probe proves suppression and event-driven sending with no code change.

### Risks / notes

- REQ-021 owns the queue; this REQ may only enqueue a rendered delivery. Two runners would double-send — an invariant test asserts a single
  runner path.
- Address hashing keeps the suppression list out of the PII surface; an unsubscribe never reveals whether an address exists.
- Template bodies can carry payload data: preview/test-send are permission-gated, body snapshots are admin-only, and payloads never ride bus
  event fields.
- Sanitization is the HTML attack surface: one allowlist module, tested; extending it is a deliberate change.
- Suppression precedence is documented and logged: unsubscribe → quiet hours → disabled channel → digest hold; the log names which one fired.
- `sms`/`chat` adapters are honest stubs (`not_ready`); no fake success anywhere.
