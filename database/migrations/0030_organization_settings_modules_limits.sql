-- Omnion · 0030 · organization settings, module enablement and plan limits (REQ-005, slice 3)
--
-- Slices 1 and 2 made the tenant layer first class: `organization_members` says *who* belongs
-- to a tenant, `departments` says *how they are arranged*. This migration adds the three tables
-- that turn a tenant from a container into something you can configure, restrict and bill:
--
-- * `organization_settings` — the organization's own preferences: invite policy, default invite
--   role, locale, timezone, accent colour, logo, audit retention.
-- * `organization_modules` — which parts of the platform this organization uses. An
--   organization can be granted or denied a module, and both the navigation and the API read
--   this one table (a feature the panel can hide but the API still serves is not disabled).
-- * `organization_limits` — the plan and its ceilings. Every limit is nullable, and a `null`
--   means *unlimited*; a value must be strictly positive when it is there at all. The REQ is
--   explicit that a limit which only decorates the UI is a lie, so these rows are read by the
--   invite-acceptance, site-creation and AI-spend paths.
--
-- Additive by design (docs/05-VERSIONING.md): three new tables, no column dropped, no
-- constraint on an existing table tightened. The backfill inserts with `on conflict do
-- nothing` and never deletes, so a live installation survives it.
--
-- Usage numbers are *not* denormalised into counters here. A stale seat count is worse than a
-- cheap `count(*)`, so every number in the Billing tab is an aggregate query computed at read
-- time (see `omnion_identity::tenancy_limits::measure_usage`).

create table organization_settings (
    organization_id     uuid primary key references organizations (id) on delete cascade,
    locale              text        not null default 'en',
    timezone            text        not null default 'UTC',
    invite_policy       text        not null default 'owner_approval',
    default_invite_role_id uuid    references roles (id) on delete set null,
    logo_media_id       uuid        references media (id) on delete set null,
    accent_color        text,
    audit_retention_days integer    not null default 365,
    updated_at          timestamptz not null default now(),

    constraint organization_settings_locale_check      check (length(btrim(locale)) > 0),
    constraint organization_settings_timezone_check    check (length(btrim(timezone)) > 0),
    constraint organization_settings_accent_check
        check (accent_color is null or accent_color ~ '^#[0-9a-fA-F]{6}$'),
    -- 30 days is a month of operational history; 3650 is ten years of it. Both ends are the
    -- REQ's, and a value outside them is a mistake worth refusing at the database too.
    constraint organization_settings_retention_check
        check (audit_retention_days between 30 and 3650)
);

-- The invite policy is the switch the REQ names: `closed` refuses new invitations outright,
-- `self_serve` lets any member holding `organizations.manage` invite, and `owner_approval`
-- queues an invitation until an owner releases it. A typo here would silently widen or
-- narrow who may add people, so the database refuses unknown values too.
create index organization_settings_invite_policy_idx
    on organization_settings (invite_policy) where invite_policy <> 'owner_approval';

create table organization_modules (
    organization_id uuid        not null references organizations (id) on delete cascade,
    module_key      text        not null,
    enabled         boolean     not null default true,
    enabled_at      timestamptz,
    updated_at      timestamptz not null default now(),
    primary key (organization_id, module_key),

    -- Same shape rule `sites.key` and `departments.key` follow: lowercase, dash separated,
    -- starting with a letter and ending with an alphanumeric. A module key is addressed in
    -- URLs, event payloads and the navigation filter, so it has to be predictable.
    constraint organization_modules_key_check
        check (module_key ~ '^[a-z][a-z0-9-]*[a-z0-9]$' and length(module_key) <= 64)
);

create index organization_modules_disabled_idx
    on organization_modules (organization_id) where not enabled;

create table organization_limits (
    organization_id         uuid primary key references organizations (id) on delete cascade,
    plan                    text   not null default 'standard',
    seat_limit              integer,
    site_limit              integer,
    storage_bytes_limit     bigint,
    ai_monthly_limit_micros bigint,
    updated_at              timestamptz not null default now(),

    constraint organization_limits_plan_check
        check (plan in ('standard', 'business', 'enterprise')),
    -- A limit is either absent (unlimited) or strictly positive. `0` would be a ceiling
    -- nothing can pass, which is never what a plan means — and it is indistinguishable from
    -- "not configured" in a UI that renders `null` as an em dash.
    constraint organization_limits_seat_limit_check
        check (seat_limit is null or seat_limit > 0),
    constraint organization_limits_site_limit_check
        check (site_limit is null or site_limit > 0),
    constraint organization_limits_storage_limit_check
        check (storage_bytes_limit is null or storage_bytes_limit > 0),
    constraint organization_limits_ai_limit_check
        check (ai_monthly_limit_micros is null or ai_monthly_limit_micros > 0)
);

-- Backfill: every organization gets a settings row and a limits row with the installation
-- defaults, so the Settings and Billing tabs never render an empty form for a tenant that
-- exists. `on conflict do nothing` makes a re-run of the migration idempotent, and a platform
-- that has already written settings keeps them.
insert into organization_settings (organization_id)
select id from organizations
on conflict (organization_id) do nothing;

insert into organization_limits (organization_id)
select id from organizations
on conflict (organization_id) do nothing;

-- `organization_modules` is deliberately **not** backfilled: a module row means "this
-- organization has made a decision about this module", and a fresh tenant has made none. An
-- absent row therefore reads as the platform default (enabled) rather than as a silent
-- disable, which is why the read path treats `missing = enabled`, not `missing = off`.
