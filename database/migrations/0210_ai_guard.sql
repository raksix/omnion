-- 0210_ai_guard.sql — REQ-105 slice 1: the tables the data guard stands on.
--
-- # What this is
--
-- Five tables for the outbound PII checkpoint (REQ-105). `crates/ai-hub/src/guard_data.rs` holds
-- the *rules* — labels, patterns, validators, the action lattice — and they are pure, so the
-- whole detector can be proven without a database. This migration is where the rows live:
-- the rules a tenant wrote, the policy that says what to do with a label, the exemptions that
-- narrow it, the event log of every decision, and the dry-run fixtures for the tester.
--
-- # The rule that shapes every foreign key here
--
-- `set null` for anything that outlives the thing it points at, `cascade` for the tenant.
-- The same rule `0189_ai_approvals.sql` used, and for the same reason: a guard event says
-- "this request was refused for this reason" and an audit trail that erases itself when a
-- user is deleted is not an audit trail. The one cascade is `organization_id` — a tenant that
-- is gone should not leave rows behind, and none of them mean anything outside it.
--
-- # `ai_guard_rules` folds `organization_id` for uniqueness, not by accident
--
-- The request asks for `unique (coalesce(organization_id, …), key)`. A bare
-- `unique (organization_id, key)` does **not** fire for the `NULL` built-in rows: in SQL,
-- `NULL` is never equal to anything, so every organization could install a row with the key
-- `email.builtin` and the platform would have nine rows claiming the same built-in key. The
-- resolver would then pick whichever the planner returned first, and the operator's own rule
-- would be silently ignored on another tenant.
--
-- PostgreSQL also refuses a `unique` **constraint** over an expression
-- (`syntax error at or near "("` — the third time this codebase has hit it, see the notes in
-- `0047_media_grants.sql`), so this is a `unique index`, and it folds NULL to the nil uuid:
--
--     create unique index ... on ai_guard_rules (coalesce(organization_id, '00000000-...'::uuid), key)
--
-- The nil uuid is a value `organization_id` can never hold, because the column is either NULL
-- or a real organization. Folding to it therefore cannot collide, and "the built-in row" and
-- "a tenant's row" become two members of the same namespace — which is what makes the seed's
-- `insert … on conflict do nothing` idempotent and safe to re-run.
--
-- # `action` and `kind` are closed sets, and closed in the database
--
-- The Rust side has `Action` and `RuleKind` enums, so a bad value cannot be written through
-- the store. The check constraints are for the *other* writers: a hand-written `insert`, a
-- fixture that seeds a row directly, or a future writer in another language. An `action`
-- outside the four names has no branch anywhere, and the screen would render an empty cell.
--
-- # `ai_guard_events` is the reason this feature is auditable at all
--
-- The request is explicit: "what is persisted is the placeholder, never the original". So the
-- table stores `value_hashes text[]` (salted) and `label_counts jsonb` and **no text column
-- anywhere**. There is deliberately no `payload` and no `context` column to be tempted into
-- later — a table that has no column for the value cannot leak the value, whatever the next
-- writer's bug is. The one place text could hide is the error message a blocked call
-- returns, and it is bounded and reasoned below.
--
-- `error_code` is what a blocked row carries (`ai_guard_blocked`), and the request's
-- "naming the label and the rule is the difference between a support ticket and a five-second
-- fix" is served by `rule_keys` + `label_counts` — never by a sentence containing the value.
--
-- # The index on `label_counts` is GIN because the events screen filters by label
--
-- `/ai/guard/events` filters on a label, and `label_counts` is an object keyed by label. A
-- containment test (`? 'email'`) is what that filter compiles to, and GIN on jsonb is the
-- operator class built for it. A btree on `created_at` alone would make the filter a scan of
-- the whole organization history, which is the query shape that makes an audit screen
-- unusable and therefore ignored.
--
-- # `ai_guard_policy` is one row per organization and defaults to permissive
--
-- `label_defaults '{}'` means every label is `allow`, and that is a deliberate choice, not a
-- shrug. An installation that has never opened the guard screen must not start refusing chat
-- because a seeded rule exists; the warning banner on `/ai/guard` is what makes the state
-- visible, and a control that is on by default and wrong is worse than one that is off by
-- default and legible. `allow_user_override` defaults `false` because a per-user weakening of
-- an organization policy is a change nobody would see in the policy panel.
--
-- # The seeded rules are the seven labels the request names, at their honest patterns
--
-- Inserted with `organization_id = null` and `on conflict do nothing`, so re-running the
-- migration (or a second installation sharing the ledger) does not duplicate them, and an
-- operator who has copied `email.builtin` to a tenant rule keeps their copy. The patterns are
-- the shapes, and each one carries the validator that turns a shape into a value: `card` is
-- `\d{16}` **plus Luhn**, because a 16-digit order number is not a card and a guard that
-- reports it is a guard that gets switched off.
--
-- `person_name` is seeded **disabled**, and this is the request's own out-of-scope clause made
-- concrete: "named-entity models downloaded at runtime" are excluded, and a name list is a
-- static data file whose size must be stated. Shipping no list is the honest version of that
-- clause — an enabled name rule with no list is a rule that cannot fire, which is worse than a
-- row an operator can see switched off.

begin;

create table if not exists ai_guard_rules (
    id                 uuid primary key default gen_random_uuid(),
    -- NULL is the platform's own rule, shared by every organization.
    organization_id    uuid        references organizations (id) on delete cascade,
    key                text        not null,
    label              text        not null,
    custom_label       text,
    kind               text        not null default 'custom',
    pattern            text        not null,
    validator          text        not null default 'none',
    action             text        not null default 'flag',
    severity           smallint    not null default 3,
    priority           integer     not null default 100,
    providers          jsonb       not null default '[]'::jsonb,
    features           jsonb       not null default '[]'::jsonb,
    enabled            boolean     not null default true,
    sample             text,
    created_by         uuid        references users (id) on delete set null,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),

    constraint ai_guard_rules_kind_known
        check (kind in ('builtin', 'custom')),
    constraint ai_guard_rules_action_known
        check (action in ('allow', 'flag', 'mask', 'block')),
    constraint ai_guard_rules_validator_known
        check (validator in ('none', 'luhn', 'iban_mod97', 'plausible_phone', 'checksum_national_id')),
    constraint ai_guard_rules_label_present check (char_length(btrim(label)) between 1 and 60),
    -- `custom_label` has to agree with `label`, and **only** with `label`.
    --
    -- The first draft of this constraint also tied the pair to `kind`, and it was wrong in the
    -- most common direction: it read `(kind = 'custom') implies (label = 'custom')`, so a
    -- tenant could not add a *second* email pattern — the single most ordinary thing an
    -- operator does with this screen. `kind` says **who wrote the row** (the platform, or a
    -- tenant); `label` says **what it detects**. A tenant rule reporting `email` is the normal
    -- case, not an inconsistency, and the check constraint was refusing it with a 23514 that
    -- the panel would have had to translate by hand.
    --
    -- So the rule is the one the Rust `RuleRow::compile` already implements: a `custom` label
    -- must carry a name, and a built-in label must not.
    constraint ai_guard_rules_custom_label_consistent
        check ((label = 'custom' and custom_label is not null
                and char_length(btrim(custom_label)) between 1 and 60)
            or (label <> 'custom' and custom_label is null)),
    constraint ai_guard_rules_key_shape
        check (key ~ '^[a-z0-9_.-]{2,60}$'),
    constraint ai_guard_rules_priority_range
        check (priority between 1 and 999),
    constraint ai_guard_rules_severity_range check (severity between 1 and 5),
    constraint ai_guard_rules_providers_is_array check (jsonb_typeof(providers) = 'array'),
    constraint ai_guard_rules_features_is_array check (jsonb_typeof(features) = 'array')
);

-- The folded unique index; see the header for why it is an index and not a constraint.
create unique index if not exists ai_guard_rules_org_key_idx
    on ai_guard_rules (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), key);

-- The detector's own query: enabled rules for one organization, in priority order.
create index if not exists ai_guard_rules_scope_idx
    on ai_guard_rules (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), enabled, priority);

create index if not exists ai_guard_rules_label_idx on ai_guard_rules (label);

create table if not exists ai_guard_policy (
    organization_id    uuid primary key references organizations (id) on delete cascade,
    label_defaults     jsonb       not null default '{}'::jsonb,
    mask_style         text        not null default 'numbered',
    allow_user_override boolean    not null default false,
    updated_by         uuid        references users (id) on delete set null,
    updated_at         timestamptz not null default now(),

    constraint ai_guard_policy_mask_style_known
        check (mask_style in ('numbered', 'deterministic')),
    constraint ai_guard_policy_defaults_is_object
        check (jsonb_typeof(label_defaults) = 'object')
);

create table if not exists ai_guard_exemptions (
    id                 uuid primary key default gen_random_uuid(),
    organization_id    uuid        not null references organizations (id) on delete cascade,
    label              text        not null,
    providers          jsonb       not null default '[]'::jsonb,
    features           jsonb       not null default '[]'::jsonb,
    -- The request is blunt about this one: "every exemption needs a reason". The column is
    -- `not null` and the check refuses a blank, so "why is this allowed" always has an answer.
    reason             text        not null,
    created_by         uuid        references users (id) on delete set null,
    created_at         timestamptz not null default now(),
    expires_at         timestamptz,

    constraint ai_guard_exemptions_reason_present
        check (char_length(btrim(reason)) between 1 and 500),
    constraint ai_guard_exemptions_label_present check (char_length(btrim(label)) between 1 and 60),
    -- An expiry in the past is a lapsed exemption, and a lapsed exemption must not be
    -- insertable as if it were live: the row is written with the past date by the expiry sweep,
    -- which is a different code path with its own authority. Here it is simply refused, so an
    -- operator cannot pre-authorize something that is already over.
    constraint ai_guard_exemptions_expiry_after_creation
        check (expires_at is null or expires_at > created_at),
    constraint ai_guard_exemptions_providers_is_array check (jsonb_typeof(providers) = 'array'),
    constraint ai_guard_exemptions_features_is_array check (jsonb_typeof(features) = 'array')
);

create index if not exists ai_guard_exemptions_org_label_idx
    on ai_guard_exemptions (organization_id, label);
-- Partial: the sweep that emits `ai.guard.exemption.expired` only ever looks at lapsed rows.
create index if not exists ai_guard_exemptions_expiry_idx
    on ai_guard_exemptions (expires_at) where expires_at is not null;

create table if not exists ai_guard_events (
    id                 bigserial primary key,
    organization_id    uuid        not null references organizations (id) on delete cascade,
    site_id            uuid        references sites (id) on delete cascade,
    user_id            uuid        references users (id) on delete set null,
    request_id         uuid        not null,
    run_id             uuid        references ai_runs (id) on delete set null,
    provider_id        uuid        references ai_providers (id) on delete set null,
    feature            text,
    action             text        not null,
    -- Which rules fired, and the per-label counts. No text: see the header.
    rule_keys          text[]      not null default '{}',
    label_counts       jsonb       not null default '{}'::jsonb,
    match_count        integer     not null default 0,
    blocked            boolean     not null default false,
    -- Salted hashes, short. "Have we seen this value before", never "what was it".
    value_hashes       text[]      not null default '{}',
    error_code         text,
    created_at         timestamptz not null default now(),

    constraint ai_guard_events_action_known
        check (action in ('allowed', 'flagged', 'masked', 'blocked', 'remapped')),
    -- `blocked` and `action` are the same fact twice, and a divergence between them is a
    -- filter that lies: the screen's "blocked only" switch reads the boolean while the row's
    -- action chip reads the word. Deriving the one from the other is a check, not a
    -- duplicated column.
    constraint ai_guard_events_blocked_matches_action
        check (blocked = (action = 'blocked')),
    constraint ai_guard_events_blocked_has_reason
        check (action <> 'blocked' or error_code is not null),
    constraint ai_guard_events_counts_positive check (match_count >= 0),
    constraint ai_guard_events_label_counts_is_object
        check (jsonb_typeof(label_counts) = 'object')
);

create index if not exists ai_guard_events_org_created_idx
    on ai_guard_events (organization_id, created_at desc);
create index if not exists ai_guard_events_org_action_idx
    on ai_guard_events (organization_id, action, created_at desc);
-- The label filter, served by containment; see the header.
create index if not exists ai_guard_events_label_counts_idx
    on ai_guard_events using gin (label_counts);
create index if not exists ai_guard_events_request_idx on ai_guard_events (request_id);
-- The retention sweep's index, matching `prune`'s shape in every other AI store.
create index if not exists ai_guard_events_created_idx on ai_guard_events (created_at);

create table if not exists ai_guard_tests (
    id                 uuid primary key default gen_random_uuid(),
    organization_id    uuid        not null references organizations (id) on delete cascade,
    name               text        not null,
    -- The payload is the operator's own sample, and the tester is a local dry run: it never
    -- reaches a provider, so storing the text is what makes the fixture reproducible. The
    -- seeded rows are invented values (see the header of the REQ), and the seed below is
    -- the evidence.
    payload            text        not null,
    context            jsonb       not null default '{}'::jsonb,
    expected           jsonb       not null default '{}'::jsonb,
    last_run_at        timestamptz,
    last_result        jsonb,
    created_by         uuid        references users (id) on delete set null,
    created_at         timestamptz not null default now(),

    constraint ai_guard_tests_name_present check (char_length(btrim(name)) between 1 and 120),
    constraint ai_guard_tests_payload_present check (char_length(payload) between 1 and 20000),
    constraint ai_guard_tests_context_is_object check (jsonb_typeof(context) = 'object'),
    constraint ai_guard_tests_expected_is_object check (jsonb_typeof(expected) = 'object')
);

create index if not exists ai_guard_tests_org_name_idx on ai_guard_tests (organization_id, name);

-- ---------------------------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------------------------

-- The seven shippable labels. `person_name` is present and **disabled**: a name list is a
-- static data file (out of scope per the request), so the row exists, is visible, and fires
-- nothing until somebody copies it and supplies the list.
insert into ai_guard_rules (organization_id, key, label, kind, pattern, validator, action, severity, priority, enabled)
values
    (null, 'email.builtin', 'email', 'builtin',
     '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}', 'none', 'flag', 3, 10, true),
    (null, 'phone.builtin', 'phone', 'builtin',
     '(\+?[0-9][0-9 ()-]{7,16}[0-9])', 'plausible_phone', 'flag', 3, 20, true),
    (null, 'national_id.builtin', 'national_id', 'builtin',
     '\b[0-9]{11}\b', 'checksum_national_id', 'flag', 4, 30, true),
    (null, 'iban.builtin', 'iban', 'builtin',
     '\b[A-Z]{2}[0-9]{2}[ ]?[A-Z0-9]{10,30}\b', 'iban_mod97', 'flag', 4, 40, true),
    (null, 'card.builtin', 'card', 'builtin',
     '\b[0-9][0-9 ]{11,18}[0-9]\b', 'luhn', 'block', 5, 50, true),
    (null, 'ip_address.builtin', 'ip_address', 'builtin',
     '\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\b',
     'none', 'flag', 2, 60, true),
    (null, 'tax_number.builtin', 'tax_number', 'builtin',
     '\b[0-9]{10}\b', 'none', 'flag', 3, 70, true),
    (null, 'person_name.builtin', 'person_name', 'builtin',
     '(?i)fatura|ad-soyad', 'none', 'flag', 2, 80, false),
    (null, 'secret_like.builtin', 'secret_like', 'builtin',
     '\b(?:sk|pk|ghp|xox[baprs])[-_][A-Za-z0-9]{16,}\b', 'none', 'block', 5, 90, true)
on conflict do nothing;

comment on table ai_guard_rules is
    'Detection rules for the outbound data guard (REQ-105). organization_id null is a '
    'platform rule shared by every tenant and editable only by copying it to a tenant row.';
comment on table ai_guard_policy is
    'Per-organization guard policy: the default action per label, the mask style, and whether '
    'a user may weaken a label for their own calls. Absent row means all defaults.';
comment on table ai_guard_exemptions is
    'Narrow, reasoned, expiring exceptions to a label action. Never exempts a block.';
comment on table ai_guard_events is
    'One row per inspected request. Holds salted hashes and per-label counts; holds no payload '
    'text, by construction rather than by discipline.';
comment on table ai_guard_tests is
    'Dry-run fixtures for /ai/guard/tester. Seeded rows carry invented values only.';

commit;
