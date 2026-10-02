# REQ-105 — AI Data Guard (PII protection)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Keeping sensitive data out of prompts.

- PII detection on outbound prompt content (names, e-mails, phones, IDs, IBAN-like patterns).
- Masking with placeholder mapping and response re-mapping so results stay coherent.
- Rules per provider/feature: block, mask, or allow; per-organization policy.
- Audit of blocked/masked events; test harness with sample payloads.
- Documentation of the residual risk (masking is defensive, not a guarantee).

## Implementation spec

### Scope (in / out)

**In** — the outbound checkpoint every request passes through before a provider sees it, plus the
response path back.

- **Detection** — a rule set evaluated over the assembled prompt (system instructions, memory,
  chunks, conversation turns) for the labelled patterns below. Built-in labels: `email`, `phone`,
  `national_id`, `iban`, `card`, `ip_address`, `tax_number`, `person_name` (when the pattern pack's
  name list is enabled), `secret_like` (long high-entropy tokens), and `custom:<key>` for
  organization rules. Each match carries a span, a label and a confidence used by the policy.
- **Actions** — per label and per provider/feature: `allow` (pass through, still counted),
  `flag` (pass through and audit), `mask` (replace and map), `block` (refuse the call). The
  organization policy sets the default action per label, and exemptions narrow it: a feature list, a
  provider list, or an explicit allow rule for a field the platform itself generated (for example
  the customer's own email in a support reply the agent is asked to draft — such exemptions are
  visible and audited, never implicit).
- **Masking** — a placeholder map per request: `[EMAIL_1]`, `[IBAN_2]`, and a deterministic form
  `[EMAIL:a1b2c3]` for repeated tokens so a provider sees the same placeholder for the same value.
  The map is held in memory for the request only; what is persisted is the placeholder, never the
  original, and the map is never written to a log.
- **Response re-mapping** — the answer is re-mapped for the requesting user: placeholders are
  substituted back on completion, inside the stored message and in the rendered view for the caller.
  While a stream is in flight, deltas render with placeholders visible; substitution happens on the
  completed message. A second reader (a shared conversation, an admin with `ai.logs.read`) sees the
  placeholders, not the originals.
- **Test harness** — a dry-run endpoint and screen where an operator pastes a sample payload and sees
  the matches, the masked text and which rules fired, without any provider call. Stored fixtures are
  synthetic by rule: the seeded samples are invented values, and the harness refuses payloads that
  match a real customer record loader (no lookups against tenant data).

**Out**

- Guarantees. The feature is defensive; every screen that reports it must not imply "no PII left the
  instance".
- Retroactive scanning of stored logs, media or documents — the checkpoint sits on the outbound
  path only.
- Detection on non-AI traffic (forms, imports, email); those are other requests' concerns.
- Named-entity models downloaded at runtime; if a name pack is used it ships as a static data file
  with its size stated.
- Encryption of the audit store, and any promise about the provider's own retention (REQ-104 labels
  the provider categories).

### Screens (UI)

- **`/ai/guard`** — policy panel: default action per label (a row per built-in label with action
  select, match count 30d and a "Test a sample" shortcut), the `allow_user_override` switch, and the
  masking style (numbered, deterministic-hashed). Stat cards (matches 30d, masked, blocked, flagged).
  A warning banner when every label sits at `allow`.
- **`/ai/guard/rules`** — table: Key, Label, Type (built-in/custom), Pattern (truncated), Validator,
  Action, Severity, Priority, Scopes (providers/features), Enabled, Updated. Filters: type, action,
  enabled, free text. Row actions Edit, Duplicate, Enable/Disable, Delete (confirm by key). Create/
  edit form: Key (`[a-z0-9_.-]{2,60}`, unique per organization), Label, Pattern (regex, compiled on
  save — an invalid expression is a field error, never a saved rule), Validator (`none`, `luhn`,
  `iban_mod97`, `plausible_phone`, `checksum_national_id`), Action, Severity (1–5), Priority
  (1–999), Scope (provider multi-select, feature multi-select, blank = everywhere), Sample text with
  a live "Test this rule" panel showing matches and the masked result.
- **`/ai/guard/events`** — table: Time, Action, Labels (chips), Matches, Feature, Provider, User,
  Site, Blocked, Value hash (short). Filters: range, action, label, feature, provider, user, blocked
  only. Detail drawer: the rule keys that fired with their counts, the decision, the refusal message
  when blocked, and links to the related log row (REQ-104) and run. No payload text is shown — the
  drawer says so explicitly.
- **`/ai/guard/tester`** — a two-pane harness: paste a payload on the left (or pick a seeded sample),
  choose provider/feature context, press Run, and read on the right the detected matches with labels
  and spans, the masked text, the action the policy would take, and a "would be blocked" verdict.
  Keyboard `⌘Enter` runs, `⌘K` clears.
- **`/ai/guard/about`** — the residual-risk statements, the label list with what each pattern does
  and does not catch, and the note that masking applies to prompts assembled by the platform.
- **Keyboard** — `/` focuses the table search, `N` new rule, `T` opens the tester, `G` then `G` goes
  to the guard, `↑/↓` + `Enter` move and open, `Esc` closes drawers and returns focus.
- **Mobile (<1024px)** — rule rows become cards with the pattern truncated behind a "Show pattern"
  toggle, the tester becomes two stacked panes with the result below the input, the events table
  becomes cards and the policy panel collapses each label row into a labelled select.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/PUT | `/api/v1/ai/guard/policy` | Label defaults, masking style, override switch | `ai.guard.read` / `ai.guard.manage` |
| GET/POST | `/api/v1/ai/guard/rules` | List / create detection rules | `ai.guard.read` / `ai.guard.manage` |
| PATCH/DELETE | `/api/v1/ai/guard/rules/{id}` | Change / remove a rule | `ai.guard.manage` |
| POST | `/api/v1/ai/guard/test` | Dry-run a payload; returns matches, masked text and verdict | `ai.guard.manage` |
| GET | `/api/v1/ai/guard/events` | Paged audit of masked/flagged/blocked requests | `ai.guard.read` |
| GET | `/api/v1/ai/guard/events/{id}` | One event with rule keys and counts (no payload) | `ai.guard.read` |
| GET/PUT | `/api/v1/ai/guard/exemptions` | Feature/provider exemptions with reasons | `ai.guard.read` / `ai.guard.manage` |

New catalogue keys: `ai.guard.read`, `ai.guard.manage`. The guard runs inside `crates/ai-hub` before
every provider call, so a blocked request never reaches the network and answers
`403 code = "ai_guard_blocked"` naming the label and the rule that refused it.

### Data model

Migration `database/migrations/00NN_ai_guard.sql` (00NN = next free integer at land time; 0019 was
free when this was written). Built-in labels and their default patterns are seeded rows with
`organization_id = null` (immutable, editable only by copying to a tenant rule); tenant rules are
rows with an organization.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_guard_rules` | id uuid pk, organization_id uuid null → organizations cascade, key text, label text, kind text ('builtin','custom'), pattern text, validator text default 'none', action text ('allow','flag','mask','block'), severity smallint default 3, priority int default 100, providers jsonb default '[]', features jsonb default '[]', enabled bool default true, sample text null, created_by uuid null → users set null, created_at, updated_at | unique `(coalesce(organization_id, …), key)`; `(organization_id, enabled, priority)`; `(label)` |
| `ai_guard_policy` | organization_id uuid pk → organizations cascade, label_defaults jsonb default '{}', mask_style text ('numbered','deterministic'), allow_user_override bool default false, flag_only_labels text[] default '{}', updated_by uuid null → users set null, updated_at | pk only |
| `ai_guard_exemptions` | id uuid pk, organization_id uuid → cascade, label text, providers jsonb default '[]', features jsonb default '[]', reason text, created_by uuid null → users set null, created_at, expires_at timestamptz null | `(organization_id, label)`; `(expires_at)` where not null |
| `ai_guard_events` | id bigserial pk, organization_id, site_id uuid null, user_id uuid null → users set null, request_id uuid, run_id uuid null, message_id bigint null, provider_id uuid null → ai_providers set null, feature text, action text ('allowed','flagged','masked','blocked','remapped'), rule_keys text[] default '{}', label_counts jsonb default '{}', match_count int default 0, blocked bool default false, value_hashes text[] default '{}', error_code text null, created_at timestamptz not null | `(organization_id, created_at desc)`; `(organization_id, action, created_at desc)`; `(label_counts)` gin; `(request_id)`; `(created_at)` for purge |
| `ai_guard_tests` | id uuid pk, organization_id → cascade, name text, payload text, context jsonb default '{}', expected jsonb default '{}', last_run_at, last_result jsonb, created_by uuid null → users set null, created_at | `(organization_id, name)` |

Salted hashes use a per-organization salt read from the secret store; the hash exists to answer "did
we see this value before" and is useless without the salt. Detector performance is bounded: rules are
compiled once at load, evaluated in priority order, and a request with more than 50 rules enabled
refuses to start in the guard (the API answers a configuration error) rather than slowing every call.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.guard.masked` | emitted | labels, counts, feature, provider, request id |
| `ai.guard.blocked` | emitted | label, rule key, feature — the alert hook for a policy refusal |
| `ai.guard.policy.changed` | emitted | which labels changed action, actor |
| `ai.guard.exemption.created` / `.expired` | emitted | label, providers, features, reason, actor — the paper trail for "why was this allowed" |

### Acceptance criteria

- [ ] A prompt containing an email address is masked to `[EMAIL_1]` before the provider call (proven with a stub provider that records the exact body it received) and the same value keeps the same placeholder across two calls in one request.
- [ ] The answer is re-mapped on completion: a stub provider that echoes `[EMAIL_1]` produces the original address in the stored message for the requester.
- [ ] While streaming, deltas show the placeholder and the completed message shows the original — asserted by capturing the SSE frames and then reading the stored message.
- [ ] A second reader (shared conversation, admin) sees the placeholder, not the original (asserted with two readers on the same message).
- [ ] An exemption for one label and one feature allows that label through for that feature only; another feature with the same label stays masked.
- [ ] An expired exemption stops applying on the next request and emits `ai.guard.exemption.expired`.
- [ ] Saving an invalid regex is refused with a field error and stores nothing; the validate endpoint returns the same message.
- [ ] The tester returns matches with labels and spans, the masked text and the verdict, and performs no provider call (stub provider records zero calls).
- [ ] The payload never appears in `ai_guard_events`, in the audit log or in the API response of the events endpoints — asserted by a test that greps the stored row text for the original value.
- [ ] A caller without `ai.guard.manage` sees Rule actions and the tester disabled with the missing permission named; the API answers 403.
- [ ] `/ai/guard/about` states the residual risk, and no other guard screen claims detection is complete.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/ai/guard`, read the label table, raise one label to `mask` and
another to `block`; open `/ai/guard/rules`, create a custom rule with a bad regex (field error),
fix it, save, toggle it off and on again; open `/ai/guard/tester`, run a seeded sample, then a
payload written by hand containing an email and an IBAN-like value, and read the matches, the masked
text and the verdict; run a chat that contains an email, open `/ai/guard/events` and inspect the
event row and the related log row; create an exemption for the `block` label on one feature, retry
that feature (passes), retry another feature (refused) and then expire the exemption and retry;
finally open `/ai/guard/about` and confirm the residual-risk text is present and legible.

The visual check must see: the action select readable at a glance per label row, chips for labels
without truncation in the events table, truncated patterns with a visible "Show pattern" affordance,
the tester's two panes aligned with match highlights visible, no payload text anywhere in the events
list, no raw i18n keys, and a mobile pass (390×844) over policy, rules and the tester.

### Slices

1. **Detector, policy and checkpoint** — rule model with built-in patterns and validators, the
   pre-call checkpoint inside `crates/ai-hub`, the organization policy, `403 ai_guard_blocked`, and
   the stub-provider test harness that proves what leaves the process.
   *Done when:* a blocked payload never reaches the stub provider and an allowed one arrives
   unchanged.
2. **Masking and re-mapping** — placeholder maps, deterministic mode, response re-mapping on
   completion, placeholder rendering in streams, per-reader substitution rules.
   *Done when:* a stub that echoes a placeholder produces the original for the requester and the
   placeholder for every other reader.
3. **Audit, events and stats** — `ai_guard_events`, the salted hash, `/ai/guard/events` with filters
   and the detail drawer, stats per label, purge alongside the log retention window.
   *Done when:* a masked request produces one event row with counts and no payload text, and the
   stats add up to the same numbers.
4. **Tester, exemptions and docs** — `/ai/guard/tester` with the seeded fixtures, exemptions
   UI/API/expiry, `/ai/guard/about`, permission polish, mobile layouts.
   *Done when:* the tester reproduces every seeded fixture, an exemption is provably narrow, and the
   residual-risk page is reachable from the guard screen.

### Risks / notes

- Placeholders must be inert to the provider: a chatty model may translate or mangle `[EMAIL_1]`;
  re-mapping must tolerate near misses (case and spacing) and, when it cannot, leave the placeholder
  visible rather than guess a value into an answer.
- A blocked call must be legible: naming the label and the rule is the difference between a
  support ticket and a five-second fix.
- Detector cost sits on the hot path of every request; a rule budget, compiled patterns and a
  benchmark in the pull request keep it from becoming the reason chat feels slow.
- Exemptions are the likeliest way this control erodes: every exemption needs a reason, an actor and
  an expiry, and the guard screen should show the count of active exemptions so they stay visible.
