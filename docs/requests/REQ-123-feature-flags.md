# REQ-123 — Feature Flags

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Turning things on safely.

- Flag registry: key, description, default, rollout state, owner.
- Scopes: environment, organization, site, user segment.
- Panel screen to toggle flags with an audit trail and "who changed this" history.
- Flags readable in backend code, admin UI and theme/rendered output.
- Kill-switch semantics documented (rollout, percentage, schedule).

## Implementation spec

### Scope (in / out)

**In**

- **Registry**: flag key (`^[a-z][a-z0-9_]*\.[a-z0-9_]+$`, module-namespaced, e.g. `cms.scheduled_publish`, ≤ 96 chars — keys are global and never reused), description, owner label (a person or team name in text), default state, rollout state (`off` | `on` | `partial`), rollout percentage, tags, and creation/update metadata.
- **Scopes** and a documented precedence: environment override (deploy configuration, read-only in the panel, shown with its source) > organization rule > site rule > user-segment rule > flag default. The most specific matching rule wins; within one scope exactly one rule per subject, and the panel renders the live precedence table so the answer to "why is this on?" is never guesswork.
- **Deterministic rollout**: `bucket = first 32 bits of SHA-256(flag_key + ':' + subject_id) % 100`; a subject is included when `bucket < rollout_percent`. Stable across restarts and nodes, monotonic when the percentage increases (raising the percentage only adds subjects), and documented as a breaking change if the key or hash input ever changes.
- **Segments**: named predicate sets over subject attributes (role, organization, site, locale, plan tier, account age, permission) evaluated at request time, plus optional explicit member lists for edge cases; a size preview runs the predicates before saving so a segment is never a mystery.
- **Kill switch**: a `force_off` boolean per flag that beats every rule, percentage and schedule. Engaging or clearing it requires `flags.kill`, a reason string and writes an audit row; the flag detail renders a loud banner while forced, and the resolved endpoint reports the forced state so no UI implies a rollout that is not live.
- **Schedule**: optional time windows (start/end with a timezone) stored on the rule; evaluation is computed from the stored windows (correct without any cron), and an optional job emits transition events for observability only — correctness never depends on it. DST boundaries are handled by evaluating instants, not wall-clock comparisons.
- **Read paths, all cheap**: Rust `flags::enabled(key, ctx)` resolving from an in-process rule cache (TTL ≤ 30 s, invalidated on change through a dedicated internal bus notification so every API node reloads within the TTL); admin UI through a `useFlag(key)` hook fed by the resolved endpoint; theme/rendered output receives a `flags` map for the current site and subject so server-side rendering can branch without a client waterfall.
- **Panel**: list with filters, detail with a rules editor per scope, rollout slider showing the live percentage, schedule editor, history with diffs, and a debug evaluator ("why does subject X get Y") naming the winning rule or the default.
- **Cleanup hygiene**: flags at 100% for longer than their `stale_after_days` (default 30) are listed as cleanup candidates; flags that reference nothing and sit at 0% are listed as removable; nothing is ever auto-deleted — the list is an invitation, not an action.

**Out**

- Experiment statistics, significance testing, holdout analysis (REQ-007 reports raw `flag_usage_daily` counts; no math here), mobile SDKs and remote config for native clients, edge/CDN-level evaluation (v1 evaluates in the API and renderer; an edge rollout is later work feeding REQ-011), and per-user one-off override UIs beyond segment includes — kept minimal on purpose because every added scope multiplies the precedence questions.

### Screens (UI)

Admin app.

| Route | Purpose |
|---|---|
| `/settings/flags` | Flag list: key, state, rollout, scopes, owner, last change, stale/removable badges |
| `/settings/flags/new` | Create: key format check, module namespace suggestion, description, owner, default |
| `/settings/flags/{key}` | Detail: precedence table, per-scope rules, rollout slider, schedule, kill switch, history |
| `/settings/flags/{key}/evaluate` | Debug evaluator: subject attributes → decision, winning rule, bucket value |
| `/settings/flags/segments` | Segment list, predicate editor, size preview |

- **List**: filters for state (`off`/`on`/`partial`/`forced off`), owner, tag, stale-only, removable-only; search by key; URL-persisted so filtered views are shareable; stale and removable rows carry badges explaining why.
- **Detail**: the precedence table lists environment override (if any), organization rule, site rules, segment rules, then default, each row with its effective state and last change; the rules editor is a full replace with server-returned precedence (so what is saved is what is shown); the rollout slider previews the bucket count at the new percentage before saving; the history drawer shows actor, time, diff and the optional reason, with the kill-switch rows visually distinct.
- **Evaluator**: pick a subject (search user/site) or hand-edit attributes, run, and read the decision, the matched rule chain and the computed bucket — the same data the server used, not a re-implementation.
- **Segments**: predicate rows (attribute, operator, value) with validation against the attribute catalogue, plus the size preview and a sample of matched subjects; a cycle or unknown attribute is refused with the row named.
- All screens: skeleton/empty/error with retry, `Esc` closes drawers, keyboard path with visible focus, light/dark parity, one column at 390 px.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/flags` | List with filters (state, owner, tag, stale) | `flags.read` |
| GET | `/api/v1/flags/{key}` | Definition, rules, effective precedence | `flags.read` |
| POST | `/api/v1/flags` | Create a flag | `flags.manage` |
| PUT | `/api/v1/flags/{key}` | Update metadata, default, rollout percentage | `flags.manage` |
| PUT | `/api/v1/flags/{key}/rules` | Full replace of the rule set (server returns precedence) | `flags.manage` |
| POST | `/api/v1/flags/{key}/kill-switch` | `{on: bool, reason}` engage/clear the kill switch | `flags.kill` |
| DELETE | `/api/v1/flags/{key}` | Delete (history rows survive; confirmation required) | `flags.manage` |
| GET | `/api/v1/flags/{key}/history` | Change history with actor, diff, reason | `flags.read` |
| POST | `/api/v1/flags/{key}/evaluate` | `{attributes}` → decision, matched rule, bucket | `flags.read` |
| GET | `/api/v1/flags/resolved` | Flags resolved for the calling subject and current site (client bootstrap) | `flags.read` |
| GET | `/api/v1/flag-segments` | Segment list | `flags.read` |
| POST | `/api/v1/flag-segments` | Create a segment | `flags.segments.manage` |
| PUT | `/api/v1/flag-segments/{id}` | Update predicates or members | `flags.segments.manage` |
| DELETE | `/api/v1/flag-segments/{id}` | Delete (refused while rules reference it) | `flags.segments.manage` |
| POST | `/api/v1/flag-segments/preview` | Size and sample for a predicate set without saving | `flags.read` |

- Errors: `400` bad key format or unknown attribute in a predicate, `403` permission miss, `404` unknown flag or a flag outside the caller's visible scope, `409` duplicate key or deleting a segment in use, `422` a rule set that references a missing segment.
- New REQ-068 keys, category `flags`: `flags.read`, `flags.manage`, `flags.kill`, `flags.segments.manage`. The kill switch is deliberately a separate power from `flags.manage`.

### Data model

Migration `database/migrations/00NN_feature_flags.sql` (next free slot at land time; additive-only).

- `flags` — `key text pk`; `name text not null`; `description text not null default ''`; `owner_label text not null`; `default_state text not null default 'off'` check in (`off`,`on`); `force_off boolean not null default false`; `rollout_percent smallint not null default 0` check 0–100; `tags text[] not null default '{}'`; `stale_after_days smallint not null default 30`; `created_by uuid null`; `created_at`, `updated_at`; partial index `(force_off) where force_off`, index `(updated_at)`.
- `flag_rules` — `id uuid pk`; `flag_key` fk cascade; `scope text not null` check in (`organization`,`site`,`segment`); `organization_id uuid null`, `site_id uuid null`, `segment_id uuid null` — exactly one non-null, matching the scope (check constraint); `state text not null` check in (`on`,`off`); `rollout_percent smallint null` check 0–100; `schedule jsonb null` (`{starts_at, ends_at, tz}`); `note text null`; `created_by`; `created_at`, `updated_at`; indexes `(flag_key)`, `(organization_id)`, `(site_id)`, `(segment_id)`.
- `flag_segments` — `id uuid pk`; `key text not null` unique on `lower(key)`; `name text not null`; `description text not null default ''`; `definition jsonb not null` (predicate tree, `jsonb_typeof = 'object'`); `created_by`; `created_at`, `updated_at`.
- `flag_segment_members` — `segment_id` fk cascade; `user_id` fk cascade; `added_by uuid null`; `created_at`; primary key `(segment_id, user_id)` — explicit includes only; computed membership stays a query over attributes and is never materialised.
- `flag_audit` — `id bigserial pk`; `flag_key text not null` (no foreign key: history survives deletion); `at timestamptz not null default now()`; `actor_user_id uuid null`; `action text not null` check in (`created`,`updated`,`rule_changed`,`kill_on`,`kill_off`,`deleted`); `diff jsonb not null default '{}'`; `reason text null`; index `(flag_key, at desc)`. The same change also writes the central audit log (REQ-039) so the platform-wide view sees it.
- `flag_usage_daily` — `day date`; `flag_key text`; `evaluation_count bigint not null default 0`; `on_count bigint not null default 0`; primary key `(day, flag_key)` — written by an in-process aggregator flushed on an interval, never by per-request writes; feeds the stale/removable view and REPORTS in REQ-007.

### Events

| Event | When | Payload |
|---|---|---|
| `flag.created` | a flag is created | `key`, `owner_label` |
| `flag.updated` | metadata, default or rollout changed | `key`, `changed_fields[]` |
| `flag.rule_changed` | the rule set was replaced | `key`, `scopes[]` |
| `flag.kill_switch.changed` | kill switch engaged or cleared | `key`, `on`, `reason`, `actor` |
| `flag.schedule_transition` | a schedule window opened or closed (observability only) | `key`, `scope`, `opened` |
| `flag.rollout_completed` | rollout reached 100% | `key` |
| `flag.deleted` | a flag was deleted | `key` |

Consumed: none in v1 — flags do not subscribe to business events. The same change events ride a dedicated internal channel that drives cache invalidation on every node; an external subscriber sees ids, keys and field names only, never a subject's attributes.

### Acceptance criteria

- [ ] Creating a flag with a malformed or duplicate key fails with the field named; a key reused after deletion is refused (keys are never recycled).
- [ ] Precedence is proven by a table-driven test: an identical subject resolves environment > organization > site > segment > default, and the detail screen renders the same winning order the API returns.
- [ ] Rollout is stable and monotonic: the same subject gets the same decision across 1,000 evaluations and a service restart; raising 10% → 20% only adds subjects, proven by bucket containment.
- [ ] Kill switch overrides rules, percentage and schedule within one cache TTL (≤ 30 s) on every node, the flag detail shows the forced banner, and no endpoint reports a rollout while forced.
- [ ] Schedule windows evaluate correctly at DST boundaries in tests, and a flag outside its window resolves to its scoped state or default deterministically.
- [ ] Saving rules is a full replace; the response carries the effective precedence, and the history diff shows exactly what changed with actor, time and reason.
- [ ] `flags.kill` is enforced separately: a `flags.manage` holder without `flags.kill` gets `403` on the kill-switch route, and the UI hides the control for them.
- [ ] Cross-organization isolation: an organization rule affects only that organization (resolved output compared across two organizations), and a flag key outside the caller's visible scope returns `404`.
- [ ] The debug evaluator reproduces the server decision, names the winning rule (or "default") and shows the computed bucket for the chosen subject.
- [ ] The resolved endpoint returns only visible flags with precedence labels (no rule internals beyond that), and the admin `useFlag` hook renders a flagged component with no layout shift when the flag flips after invalidation.
- [ ] Theme/rendered output: a flag flipped on is visible in the next server-rendered page with no client JavaScript required (RENDER test comparing two requests).
- [ ] `flag_usage_daily` for a test window is within one flush interval of a directly counted value, and no per-request write path to it exists (code-level check).
- [ ] Stale and removable badges appear exactly for the documented conditions and nothing is auto-deleted (deletion always requires `flags.manage` plus confirmation).
- [ ] Segment preview sizes match an actual evaluation of the same predicates against the subject table; a segment referenced by a rule cannot be deleted until the rule is removed.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are green; every new screen is in the walkthrough inventory.

### QA plan

- Walkthrough: create a flag, add an organization rule, add a segment rule with a preview, use the evaluator on a seeded user, engage the kill switch and watch the flagged component disappear after invalidation (no deploy), clear it, then walk the history drawer entries.
- Visual review: precedence table clarity, rollout slider with bucket preview, kill-switch banner visibility, stale/removable badges, and light/dark parity.
- `scripts/qa/probe-flags.cjs` (new, exits non-zero) asserts through the API: precedence order table, monotonic rollout containment, cache invalidation timing (change then re-evaluate on a warm node), `403` matrix for `flags.kill`, cross-organization isolation, and resolved-endpoint payload shape.
- Honest note: the environment override lives in deploy configuration and therefore cannot be flipped from the panel — the walkthrough states this and verifies the read-only display instead.

### Slices

1. **Registry, schema, service read path, resolved endpoint** — migration, `flags` module with cache and invalidation, evaluation order. *Done when:* a hardcoded default is replaced by `flags::enabled` somewhere real and a test proves the override order.
2. **Panel: list, detail, rules editor, history, kill switch** — the panel screens with the precedence table and dirty-state handling. *Done when:* a toggle propagates to a running node within the TTL.
3. **Segments, evaluator, usage rollup, stale view** — predicate editor with preview, debug evaluator, `flag_usage_daily` flush, cleanup badges. *Done when:* the evaluator explains a decision naming the winning rule.
4. **Docs and rendered-output integration** — precedence documentation, theme/render `flags` map, SSR verification. *Done when:* a flagged theme block flips with the flag in server rendering.

### Risks / notes

- Cache staleness is bounded and stated: TTL ≤ 30 s plus change-driven invalidation; any future caching layer must consume the same invalidation events, and the docs name this as a contract.
- Precedence confusion is the top support risk: every rule row links to the evaluator, and the precedence table is rendered from live data rather than hand-written help text.
- Rollout monotonicity depends on the bucketing algorithm being frozen: changing the hash input or key is a documented breaking change with a migration note, because it reshuffles every subject.
- Hot-path cost is a design constraint: rules are loaded into memory per node and resolved without SQL; per-request database evaluation of flags is forbidden, and a test exercises the cached path.
- Environment overrides are deploy-time by design; the panel shows them read-only with the source named, so nobody hunts for a toggle that does not exist.
- Deleted flags keep their history rows (no foreign key) and their keys stay retired forever — recycling a key would silently change behaviour for anyone who cached it.
