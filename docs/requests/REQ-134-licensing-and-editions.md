# REQ-134 — Licensing & Edition Gating

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Open-core business model, enforced in code.

- Edition model: community (open) vs enterprise (licensed) with a signed licence file.
- Feature gating declared in one place (a capability registry) and enforced in the API, not just the UI.
- Licence screen: installed licence, expiry, seats, entitled features, renewal action.
- Grace behaviour for expired licences (read-only vs hard stop, configurable).
- Never break the open-source core: gating must never remove existing open functionality.

## Implementation spec

> **Band:** migration `0134` reserved; append-only ledger — take the next free number if taken · **Layer:** new `crates/licensing` plus a `guards` extension in the API · **Admin routes:** `/settings/licence` · **Constraint:** the community edition stays complete for everything that is open today, and nothing that ships open may ever move behind the gate.

### Scope (in / out)

**In**

- Capability registry: one declarative source (a registry file embedded at build time) listing every gated capability — key, human name, description, edition (`community` or `enterprise`), enforcement surfaces (API route, background job, engine path, screen) and a docs anchor. Nothing is gated outside the registry. A CI check fails when `guards::require_capability("…")` names an unknown key, when an entry is never enforced anywhere, or when an entry covers something the community baseline already provides.
- Editions: `community` (no licence file, every community capability works forever, no seat or time limits) and `enterprise` (a valid signed licence unlocks the registry's enterprise capabilities plus whatever limits the licence declares, such as seats and site count). Edition is derived from the licence, never configured by hand.
- Licence file: offline and self-contained — a text document carrying a JSON payload and a detached signature, so it can be pasted through an air gap or committed to an internal secrets store. The payload holds licence id, customer name, edition, format version, issued and expiry timestamps, seat limit, included capability keys and optional resource limits. Verification checks the signature against the public key embedded in the release, the format version, the dates against server time with a clock-rollback sanity check, and an optional installation binding. Nothing is transmitted anywhere; activation and renewal are file operations.
- Enforcement in the API: `guards::require_capability()` sits in the same guard chain as `guards::require`, returning `402 Payment Required` with a body naming the capability key and the edition that provides it, plus an audit row. Enforcement points are declared per capability in the registry and cover background jobs and engine paths, not only HTTP — a job that is not entitled must refuse its work rather than fail silently.
- Never break the core: the registry may only add. A boundary test keeps a snapshot of the community feature list (routes, jobs, screens) and fails when an entry disappears or when an existing open route starts requiring a capability. Removing a licence never deletes data: enterprise-gated features simply stop accepting work per the grace mode, and their stored data stays readable wherever the registry marks read visibility.
- Licence screen: current edition, customer, licence id, issue and expiry dates with a countdown, seats used against limit, a capability table (entitled or not, with the feature it gates and a plain-language reason), install and replace flows (upload or paste) with a pre-save validation and a change preview, removal behind a typed confirmation that names exactly what will stop, and renewal guidance.
- Grace behaviour: configurable `grace_mode` (`read_only` by default, `hard_stop` optional) with a soft window of warning days; warnings fire at 30, 14, 7 and 1 days through events, an admin banner and e-mail to configured recipients. In `read_only`, enterprise capabilities refuse writes but keep serving reads; in `hard_stop` they return `402` immediately. Both modes leave community functionality and all stored data untouched.
- Seats and limits: seat counting from active users with a warning threshold; when the licence says enforce, invitations beyond the limit are refused with a message naming the limit, otherwise the screen warns. Resource limits from the licence (active users, sites, engine instances) are each enforced at their creation point with the same message shape.
- Visibility and docs: edition and licence state appear in the admin overview and in a redacted support view; the edition comparison table is generated from the registry, so documentation and code cannot disagree.

**Out**

- Online licence servers, phone-home telemetry, aggressive hardware fingerprinting beyond an optional installation binding, per-user licence assignment and marketplace monetisation.
- Moving open functionality behind the gate — explicitly forbidden and guarded by the boundary test.
- Feature flags (REQ-123) and billing systems: the licence gates edition capability families, not individual toggles or invoices.
- Contract management, reseller portals and invoicing workflows.

### Screens (UI)

| Route | Screen |
|---|---|
| `/settings/licence` | Licence and editions: state card, details, seats, capability table, install and remove, renewal |
| `/settings/licence/diagnostics` | Verification detail: key fingerprint, format version, signature result, clock check, binding result |
| Admin chrome (global) | Expiry banner at 30/14/7/1 days, edition badge in the overview, upgrade hint wherever a gated feature appears |

- **State card.** Edition, customer, licence id, issued and expiry with a day countdown (amber inside 30 days, red at expiry), a status chip (`community`, `valid`, `expiring`, `expired`, `invalid`), a seats bar (used against limit) and `Replace licence` as the primary action. `Remove licence` sits behind a typed confirmation that lists the enterprise capabilities that will stop working.
- **Capability table.** Columns Capability, Feature, Status (entitled, not licensed, or requires a newer licence) and a `Why is this locked?` expander quoting the registry description and the edition needed. A `Copy support bundle` action produces a redacted text block — no keys, no licence content, no user data — for support tickets.
- **Install flow.** Paste area and file drop; validation runs before anything is stored (signature, format, dates, binding) and shows a preview of what changes (capabilities gained or lost, seats, expiry) with an explicit confirm. A rejected file reports the precise reason and never touches the active licence; the current licence stays valid until a new one verifies.
- **Diagnostics.** Public key fingerprint, format version, signature algorithm, issue and expiry timestamps, clock-drift result, installation binding result and last verification time — enough for support to solve a puzzle without asking for the licence file.
- **Locked-feature UX.** A gated screen renders an explanation card (what the capability does, which edition provides it, how to renew) instead of a bare error; disabled controls carry the same reason; the UI never implies that data was removed. Community installations show the comparison table as normal information, never as an error state.
- **States and mobile.** Skeletons, empty states, error strips with retry; at 390 px the capability table becomes a card list and the install flow stacks with the confirm action sticky.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/licence` | Current state: edition, customer, dates, seats, capabilities, grace mode | `licence.read` |
| POST | `/api/v1/licence` | Install or replace a licence file (validated, atomic) | `licence.manage` |
| DELETE | `/api/v1/licence` | Remove the licence and revert to community (typed confirmation) | `licence.manage` |
| GET | `/api/v1/licence/capabilities` | Registry joined with entitlement, for the UI and docs generation | `licence.read` |
| GET | `/api/v1/licence/diagnostics` | Verification details and fingerprints | `licence.read` |
| GET · PUT | `/api/v1/licence/settings` | Grace mode, warning days, recipients, seat enforcement | `licence.manage` |
| GET | `/api/v1/licence/seats` | Seat usage and role counts (counts only) | `licence.read` |
| POST | `/api/v1/licence/verify` | Re-run verification on demand (support workflow) | `licence.read` |
| GET | `/api/v1/capabilities` | Effective capability set for the installation (cache-friendly) | `licence.read` |
| GET | `/api/v1/capabilities/{key}` | Single capability with its enforcement surfaces and state | authenticated |

`402` is the single refusal status for missing entitlements, with a stable body shape (`capability`, `edition_required`, `grace_mode`, `message`) that browser sessions, API keys and service callers all receive identically. Gated routes remain visible in the internal OpenAPI document but are annotated with their capability, so generated clients can distinguish "not built" from "not entitled". Capability checks are cached in-process with a revision counter invalidated by install, replace, remove, expiry sweep and settings change.

### Data model

Migration `0134_licensing_and_editions.sql`.

```sql
licence_state (singleton bool pk default true check (singleton), edition text default 'community' in ('community','enterprise'),
  licence_id text unique, customer_name text, format_version int, issued_at timestamptz, expires_at timestamptz,
  seat_limit int, seats_in_use int default 0, capabilities text[] default '{}', limits jsonb default '{}',
  status text default 'community' in ('community','valid','expiring','expired','invalid'),
  signature_verified bool default false, verification_detail jsonb default '{}', raw_licence text,
  installed_at timestamptz, installed_by uuid -> users, updated_at timestamptz)
licence_events (id bigserial pk, kind text in ('installed','replaced','removed','verified','warning','expired','invalid_install','capability_denied'),
  capability text null, actor_user_id uuid null -> users, detail jsonb default '{}', created_at timestamptz)
  index (kind, created_at desc)
licence_usage_daily (day date pk, active_users int, active_sites int, engine_instances int)
licence_settings (id bool pk default true, grace_mode text default 'read_only' in ('read_only','hard_stop'),
  grace_days int default 30, warn_recipients text[] default '{}', enforce_seats bool default true,
  updated_by uuid -> users, updated_at timestamptz)
```

Notes: exactly one row exists in `licence_state` and `licence_settings`; community is represented by an empty state rather than a fake licence. `raw_licence` is stored so re-verification after a key rotation does not require a re-upload; it is signed data, not secret, and it is excluded from diagnostics output and support bundles. `capability_denied` rows are sampled (one per capability per hour) so a probing client cannot flood the audit table. Seat and limit counts are derived at read time from the source tables and only cached briefly, so there is no second inventory to drift.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `licence.installed` · `.replaced` · `.removed` | File lifecycle | `edition`, `licence_id`, `expires_at`, `actor_user_id` |
| `licence.expiring` | Warning thresholds at 30/14/7/1 days | `licence_id`, `expires_at`, `days_left` |
| `licence.expired` | Expiry reached | `licence_id`, `grace_mode` |
| `licence.invalid` | Verification failure at install or re-verify | `reason`, `actor_user_id` |
| `licence.capability.changed` | Effective capability set changed after install, expiry or removal | `added`, `removed` (keys only) |
| `licence.capability.denied` | A call hit a capability the installation lacks (sampled) | `capability`, `route`, `actor_user_id` |

Consumed: `user.created` · `user.deactivated` recompute seat counts; clock-related system events trigger a re-check of the licence dates; `updates.applied` (REQ-078) re-verifies after an upgrade so a newer format version is caught immediately.

Webhook relevance: yes — expiry, capability change and denied-capability events are what an owner subscribes to (renew before writes stop, or investigate a client probing gated routes). Payloads carry ids, dates and capability keys only; the licence file content never appears in an event.

### Acceptance criteria

- [ ] A community installation has no licence file and every registry-community capability works with no seat or time limit; the licence screen presents that state as normal, not as an error.
- [ ] Installing a valid signed licence unlocks exactly the capabilities it lists and changes nothing else.
- [ ] The CI registry check fails when a `require_capability` call names an unknown key, or when a registry entry has no enforcement surface.
- [ ] A licence with an edited payload, a corrupted signature, an unknown format version or a mismatched key is refused without touching the active licence, and the reason is shown.
- [ ] Direct API calls to a gated route on a community build return `402` with the capability key even when the UI never renders that route; the same call on an entitled installation succeeds unchanged.
- [ ] Enforcement is proven at the declared surface kinds, including at least one background job and one engine path, not only HTTP.
- [ ] The boundary test passes: the recorded community feature list retains every entry after installing, expiring and removing a licence.
- [ ] After expiry in `read_only` mode, gated capabilities serve reads, refuse writes with `402`, and community features plus stored data are untouched.
- [ ] After expiry in `hard_stop` mode, gated capabilities return `402` immediately with the same body shape, and community features remain untouched.
- [ ] Grace warnings fire at 30/14/7/1 days, raising banners and events and reaching configured recipients.
- [ ] The licence screen reflects install, expiry and removal within one refresh, with an accurate countdown; the support bundle contains no key material or personal data.
- [ ] Seat counts match the active-user count, the threshold warning renders, and with seat enforcement on, invitations beyond the limit are refused with a message naming the limit.
- [ ] Licence limits (users, sites, engine instances) are enforced at their creation points, and a malformed limit value cannot break a create path.
- [ ] Removing the licence reverts to community atomically, keeps all data, and the enterprise routes return `402` afterwards.
- [ ] A clock rollback beyond the threshold is detected and reported rather than extending validity, and diagnostics show the check.
- [ ] Install, verify and renewal all work with no network access, proven in an air-gapped container.

### QA plan

CI generates test licences with a throwaway key pair; the production public key ships in the release and the private key never enters the repository. Walkthrough: a community build shows the normal community state and the comparison table; a gated route is curled before and after installing a valid licence, proving the transition at the API layer; an edited payload, a corrupted signature and an expired fixture are each refused with the right reason and no state change; expiry fixtures drive both grace modes with reads, writes and community features checked; seat limits are exceeded with test users; the diagnostics screen is compared against the verification detail in the database; the support bundle is scanned for key material; every scenario repeats in an air-gapped container with networking disabled. Visual check: the expiry banner, the locked-feature card, the capability table states and the install preview all render legibly at 1280 and 390 px.

### Slices

1. **Registry, guard and boundary test.** Registry file, `require_capability` guard with the `402` envelope, CI checks, community snapshot test. *Done when:* acceptance 3, 5, 6 and 7 pass on the community build.
2. **Licence verification and state.** Parser, signature and date checks, storage, install and replace and remove endpoints, events, diagnostics data. *Done when:* acceptance 1–4 and 15 pass with all fixtures.
3. **Licence screen, seats and settings.** The screen set, seat counting, warnings, grace settings and the locked-feature UX. *Done when:* acceptance 11–13 pass and every status chip is exercised by a fixture.
4. **Grace behaviour, docs and air gap.** Both grace modes enforced across HTTP, jobs and engine paths; generated edition comparison; air-gapped proof; renewal documentation. *Done when:* acceptance 8–10, 14 and 16 pass.

### Risks / notes

- Boundary creep is the decisive risk: the temptation to gate something that is already open must be met by the snapshot test, which may only change through a deliberate reviewed edit — never as a side effect of a feature PR.
- Clock tampering can extend validity offline: dates are checked against server time with a drift check, and a rollback beyond the threshold is reported and audited rather than silently ignored.
- Key custody: only the public key ships; test keys are generated per pipeline run and never committed; a rotation plan keeps one previous key valid for verification so old licences do not break during a transition.
- A locked enterprise feature must explain itself: bare errors breed support tickets and suspicion, so the locked card names the capability, the edition and the renewal path, and it never suggests that data was deleted.
- Seat counting races on concurrent invitations: counts derive from source tables at read time, enforcement uses a transaction-level check, and brief overshoot is documented rather than hidden.
- Format changes are inevitable: the format version is explicit, unknown majors are refused with a clear message, and the parser is written to ignore unknown optional fields from newer minors.
- `402` may be retried by naive proxies — the docs state that it is a terminal refusal, and responses carry the capability key so retries are pointless and visible in logs.
- Air-gapped renewals cannot be automatic by design: the manual file path is documented as the supported route, and the licence screen links to it rather than implying a hidden online check.
- Capability checks sit in hot paths, so they must stay a cached boolean lookup; the revision counter invalidation is the one mechanism, and a test proves a newly installed licence takes effect without a restart.
