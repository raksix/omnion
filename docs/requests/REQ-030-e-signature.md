# REQ-030 — E-signature

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/esignature`)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

```text
Contract
 ↓
Send
 ↓
Sign
 ↓
Signed
 ↓
Archive
```

Signature provider abstraction:

```text
DocuSign
Adobe
Custom
Internal
```

## Notes

- Ties into Documents (docs/08-BUSINESS-SUITE.md) and the approval infrastructure
  (docs/07-IAM.md §17).

## Implementation spec

> Buildable contract. Domain: new crate `crates/esignature` (`omnion-esignature`); HTTP surface `apps/api/src/routes/esignature.rs`; panels under `/esignature` in `apps/admin` (`apps/admin/features/esignature/`), plus a token-scoped signing page that works with no panel session. Every provider sits behind one trait, so the internal flow, a commercial provider and a custom connector differ only by adapter.

### Scope (in / out)

**In**

- Signature requests over a generated document (REQ-029) or an uploaded file: title, message, expiry, recipients.
- Recipients with role `signer` or `cc`, sequential (ordered) or parallel signing, per-recipient reminders.
- Field placement: `signature`, `initials`, `date` and `text` fields positioned per page in normalized coordinates on a page preview.
- Lifecycle: `draft` → `sent` → `viewed` → per-recipient `signed` → `completed`, plus `declined`, `expired`, `voided`.
- Internal provider: an account-free signer opens a single-use tokenized link, types or draws a signature, accepts the consent disclosure.
- Provider abstraction: `internal`, `docusign`, `adobe`, `custom` adapters exposing the same normalized operations (create, send, remind, void, fetch status, fetch signed file).
- Append-only audit trail with a hash chain per request, and a completion certificate rendered through REQ-029.
- Archival: signed copy plus certificate land in the documents library; the original PDF is retained unchanged.

**Out** — any claim that a signature satisfies a jurisdiction's legal requirements and any qualified/advanced signature hardware path; in-person notarization; wet-signature scanning; envelope mail-merge beyond the batch generation REQ-029 already provides.

### Screens (UI)

| Route | Screen | Contents |
|---|---|---|
| `/esignature/requests` | List | Table: Title, Document, Signers (signed / total), Status, Sent, Expires, Provider. Filters: status, provider, date range, text. Bulk: remind, void. Empty state "No signature requests yet". |
| `/esignature/requests/new` | Wizard | Step 1 pick a document (generated document or upload); Step 2 recipients (name, e-mail, role, order); Step 3 field placement (page thumbnails with draggable field boxes); Step 4 message, expiry, provider, reminder cadence; Step 5 review and Send. Back/Next and Save as draft. |
| `/esignature/requests/{id}` | Detail | Status header and progress; timeline (created → sent → viewed → signed → completed); recipient table with status, timestamps and Remind; audit trail table (actor, action, timestamp, IP, user agent, chain link); actions Copy signing link, Resend, Void with reason, Download signed PDF, Download certificate. |
| `/esignature/signing/{token}` | Signer page (no session) | Document viewer with page navigation and "Sign here" markers; signature capture (draw or type with a font selector); consent checkbox with the disclosure text; Decline; finish screen with a download. Mobile-first and touch friendly. |
| `/esignature/providers` | Provider settings | Provider list with enable toggle and default marker; per-provider configuration form that selects a secret reference from the secrets manager (REQ-037) instead of accepting credentials; Test connection; routing rules by document type. |
| `/esignature/settings` | Settings | Default expiry days, reminder cadence, retention months, whether CC recipients receive the completed copy. |

- Empty: no requests yet; a review step without placed fields warns before sending; a request without a signer cannot be sent.
- Loading: list skeleton; the signing page shows a viewer skeleton then the pages; send shows progress and disables the actions.
- Error: an expired or already-used signing token shows one plain-language page with a support hint (identical for "unknown" and "expired" so tokens cannot be probed); a provider failure keeps the request in draft with a readable error and Retry; wizard validation errors are inline; a declined request highlights the reason.
- Keyboard: list `j`/`k` and `Enter`, wizard `Esc` cancels, review step `⌘Enter` sends, and signature capture always offers a typed-signature fallback because a drawn signature alone is not accessible.
- Mobile: the signing page is mobile-first (pinch-zoom viewer, touch canvas, ≥44 px targets), the wizard collapses to steps, admin tables become cards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/esignature/requests` | List requests in scope | `esignature.read` |
| POST | `/esignature/requests` | Create a draft request | `esignature.manage` |
| GET | `/esignature/requests/{id}` | Detail with recipients, timeline, audit | `esignature.read` |
| PUT | `/esignature/requests/{id}` | Update a draft | `esignature.manage` |
| PUT | `/esignature/requests/{id}/fields` | Replace field placement | `esignature.manage` |
| POST | `/esignature/requests/{id}/send` | Send invites through the selected provider | `esignature.send` |
| POST | `/esignature/requests/{id}/remind` | Remind pending signers | `esignature.send` |
| POST | `/esignature/requests/{id}/void` | Void with a reason | `esignature.manage` |
| GET | `/esignature/requests/{id}/document` | Download the current or signed file | `esignature.read` |
| GET | `/esignature/requests/{id}/certificate` | Download the completion certificate | `esignature.read` |
| GET | `/esignature/requests/{id}/signing-links` | Admin-visible internal links (audited) | `esignature.send` |
| GET | `/esignature/signing/{token}` | Signer view, token scoped | — (token) |
| POST | `/esignature/signing/{token}/accept` | Record consent and view | — (token, rate-limited) |
| POST | `/esignature/signing/{token}/sign` | Submit a signature for the field map | — (token) |
| POST | `/esignature/signing/{token}/decline` | Decline with a reason | — (token) |
| GET | `/esignature/providers` | Provider configurations | `esignature.providers.read` |
| PUT | `/esignature/providers/{kind}` | Configure or enable a provider | `esignature.providers.manage` |
| POST | `/esignature/providers/{kind}/test` | Test a provider connection | `esignature.providers.manage` |
| POST | `/esignature/webhooks/{kind}` | Provider callback, signature verified | — (provider-authenticated) |

Token rules: signing tokens are 256-bit random values stored only as hashes, single-use per signing action, expiring, invalidated on void, rate-limited per token and per IP; a token resolves exactly one document and exposes nothing that enumerates.

### Data model

`database/migrations/0015_esignature.sql` (numeric prefix = next free slot at tick time). Additive-only.

- `signature_requests` — `id uuid pk`, `organization_id uuid not null → organizations on delete cascade`, `title text not null`, `document_id uuid → documents on delete set null`, `media_id uuid → media on delete set null`, `provider text not null default 'internal'`, `provider_reference text`, `status text not null default 'draft'`, `message text`, `expires_at timestamptz`, `sent_at`, `completed_at`, `source_checksum text`, `signed_checksum text`, `created_by uuid → users`, `created_at`, `updated_at`. Checks: status within the eight states, `document_id is not null or media_id is not null`.
- `signature_recipients` — `id uuid pk`, `request_id uuid not null → signature_requests on delete cascade`, `name text not null`, `email text not null`, `role text not null default 'signer'`, `order_no integer not null default 1`, `status text not null default 'pending'`, `viewed_at`, `signed_at`, `declined_reason text`, `signature_media_id uuid → media on delete set null`, `token_hash text unique`, `ip_address inet`, `user_agent text`, `created_at`. Checks: role in `('signer','cc')`, status in `('pending','viewed','signed','declined')`, e-mail format; index `(request_id, order_no)`.
- `signature_fields` — `id uuid pk`, `request_id uuid not null → signature_requests on delete cascade`, `recipient_id uuid not null → signature_recipients on delete cascade`, `kind text not null`, `page integer not null`, `x numeric not null`, `y numeric not null`, `width numeric not null`, `height numeric not null`, `required boolean not null default true`, `value text`. Checks: `page >= 1`, coordinates within 0..1, kind in `('signature','initials','date','text')`.
- `signature_events` — `id uuid pk`, `request_id uuid not null → signature_requests on delete cascade`, `recipient_id uuid → signature_recipients on delete set null`, `kind text not null`, `actor text not null`, `ip_address inet`, `user_agent text`, `metadata jsonb not null default '{}'::jsonb`, `prev_hash text`, `hash text not null`, `occurred_at timestamptz not null default now()`. Append-only: the codebase exposes no update or delete path, and `hash` is the SHA-256 of the previous hash plus the canonical payload, so the panel can verify the chain.
- `signature_providers` — `id uuid pk`, `organization_id uuid not null → organizations on delete cascade`, `kind text not null`, `config jsonb not null default '{}'::jsonb` (secret references only), `enabled boolean not null default true`, `is_default boolean not null default false`, `created_at`, `updated_at`; `unique (organization_id, kind)`.

### Events

- Emitted: `esignature.request.sent`, `esignature.request.viewed`, `esignature.request.signed`, `esignature.request.declined`, `esignature.request.completed`, `esignature.request.expired`, `esignature.request.voided`, `esignature.certificate.ready`.
- Consumed: `document.rendered` (REQ-029) attaches a fresh version when a document is regenerated; provider callbacks update status; the approval infrastructure (REQ-059) can request a signature and wait for `esignature.request.completed`.
- Webhook relevance: high — a signed contract closing a deal elsewhere is a classic external trigger. Payloads carry request id, status, recipient code and timestamps; never the signature image, the document bytes or the signer's e-mail address.

### Acceptance criteria

- [ ] A request can be built in the wizard, saved as a draft and sent.
- [ ] Recipients can be signers or CC, and ordering produces sequential release.
- [ ] Field placement persists per page in normalized coordinates and renders correctly at any zoom or viewport width.
- [ ] The signing page opens from a token with no panel session and displays the document.
- [ ] A typed signature works keyboard-only and is accepted on the same terms as a drawn one.
- [ ] Consent is recorded before any signature is accepted.
- [ ] Signing updates recipient and request status and appends an audit event with IP and user agent.
- [ ] In a sequential request the next signer is released only after the previous signature.
- [ ] Completing all signers produces the signed PDF and the completion certificate.
- [ ] The downloaded signed file's SHA-256 matches the stored `signed_checksum`, and the audit chain verifies end to end.
- [ ] Remind targets pending signers only and appears in the timeline.
- [ ] Void with a reason stops pending signers, invalidates their tokens and records the actor.
- [ ] An expired request moves to `expired` and its tokens stop working.
- [ ] A replayed or already-used token is refused with the neutral plain-language page.
- [ ] Declining records the reason and notifies the request owner.
- [ ] The internal provider can be enabled alongside one external provider, tested for connectivity, and routed by document type without exposing secrets in the UI.
- [ ] A provider outage leaves the draft intact with a readable error and a retry path.
- [ ] `cargo test -p omnion-esignature` covers token hashing, sequential release, chain verification and every status transition.
- [ ] All `/esignature` routes appear in the QA walkthrough inventory, and the signing page is usable at 390×844.

### QA plan

Walkthrough: create a request from a generated invoice; add one signer and one CC sequentially;
place a signature field on page 1; send; copy the internal signing link from the detail screen
(the internal provider surfaces it so the flow is testable end to end without a real inbox);
sign as the first signer in a fresh browser context, then as the second; confirm the status
becomes Completed; download the signed PDF and the certificate; check the audit trail lists
every event in order. Then create a second request, void it, and confirm the old token is dead.

A focused probe should recompute the audit chain and the checksum of the downloaded signed file
and assert both verify — that turns tamper-evidence into a number instead of a claim. The visual
check must see: a usable signing page at 390 px with large tap targets and no clipped canvas,
an ordered timeline, consistent status badges, readable decline/expiry pages, and no raw i18n
keys.

### Slices

1. **Requests + internal signing** — migration, request/recipient/field services, wizard, detail with timeline and audit trail, token-scoped signing page, completion on the last signature. Done when a two-recipient internal request completes end to end with every event in the trail.
2. **Signed artifact + lifecycle** — signed PDF assembly, checksum, certificate through REQ-029, reminders, expiry sweeper, void rules, decline path. Done when the probe verifies the chain and the checksum.
3. **External providers + settings** — provider adapters (DocuSign, Adobe, custom), settings screen with secret references, connection test, signature-verified callbacks, routing by document type. Done when an external provider is configurable without any secret in the panel and its callbacks drive the request status.

### Risks / notes

- Compliance wording: Omnion ships the workflow and the evidence trail, not a legal guarantee. The UI and docs must state plainly that validity depends on the deployment's jurisdiction and configuration, and must never claim qualified-signature status.
- Token security is the whole feature: hashes only, single use, short expiry, rate limits per token and per IP, no enumeration, revoke on void and expiry, and identical failure pages so tokens cannot be guessed or probed.
- Provider credentials never reach the panel or the logs: they are referenced from the secrets manager (REQ-037) and every callback is verified against the provider's signing secret before any state change.
- The append-only chain is the compliance artifact, so no update or delete path may exist in code, and the certificate renders the chain as a readable table a third party can verify without panel access.
- Sequential release is event-driven on signature; the background sweeper only sends reminders and expires stale requests, never re-sends a release it cannot prove.
- Invite and reminder deliverability depends on the platform mail path; development and QA use the local mail catcher, never real inboxes.
- Provider callback timestamps can skew against local time: store both the reported time and the local receipt time, and order the timeline by local receipt.
- Keep the original, the signed copy and the certificate as three immutable document versions — never overwrite one with another, or the evidence trail loses its meaning.
