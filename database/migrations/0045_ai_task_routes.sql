-- REQ-098 · Model registry & router — slice 2 (task routing and feature overrides).
--
-- The catalog (slice 1) answers "what can this model do and what does it cost". This adds the
-- thing that makes those facts *used*: a named task with an ordered list of candidates, a named
-- feature that may pin its own model, and the scopes those live at.
--
-- Four decisions worth stating, because each of them closes a way the data can lie:
--
-- 1. **A scope is a folded pair, not two nullable columns read by hand.** `scope_key` is a
--    stored generated column that spells the scope out — `installation`, `org:<uuid>` or
--    `site:<uuid>` — and every uniqueness rule is stated against *it*. Reading
--    `(organization_id, site_id)` as a scope means three call sites each re-deciding what
--    "both null" means, and a route that forgot the site would silently write to the
--    organization row. One generated column makes the scope a value the database compares.
--
-- 2. **A site belongs to an organization, so the pair is constrained.** A row naming site B
--    under organization A is a leak waiting to happen: the site filter would read B's map with
--    A's authority. The composite foreign key makes that unrepresentable rather than
--    merely unlikely.
--
-- 3. **`position` starts at 1 and is unique per (scope, task).** The candidate list is
--    ordered, and order is the whole point of a fallback chain: "the first one that works"
--    cannot be reconstructed from an unordered set. `>= 1` rather than `>= 0` so position 0
--    cannot be confused with "unset" in a payload that omits it.
--
-- 4. **An override may name a model that is later removed; a route candidate may not.** An
--    override points at *one* model, so a null `model_id` would be a row that says nothing.
--    A candidate is one element of an ordered list, and the spec asks for a removed model to
--    leave "the route row with a null candidate" that the panel marks as needing attention —
--    so the candidate's foreign key is `set null` (the row survives, the walk explains the
--    skip) while the override's is `restrict` (nothing useful is left to point at).

-- The composite key the scoped rows below need. A site id alone is already unique (primary
-- key), but a *composite* foreign key has to point at a declared unique constraint on exactly
-- those columns, and `sites` only carried `unique (organization_id, key)`. Without this key the
-- constraint that stops a route naming somebody else's site cannot be expressed at all.
create unique index sites_id_organization_id_key on sites (id, organization_id);

create table ai_task_routes (
    id              uuid        primary key default gen_random_uuid(),
    scope_key       text        generated always as (
                        case
                            when site_id is not null then 'site:' || site_id::text
                            when organization_id is not null then 'org:' || organization_id::text
                            else 'installation'
                        end
                    ) stored,
    organization_id uuid        references organizations (id) on delete cascade,
    site_id         uuid        references sites (id) on delete cascade,
    task            text        not null,
    position        integer     not null,
    model_id        uuid        references ai_models (id) on delete set null,
    requirements    text[]      not null default '{}',
    updated_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    -- A site is reached through its organization, so the pair has to be a real one. Without
    -- this, a route could name a site that belongs to somebody else and the scope filter would
    -- read it with the wrong tenant's authority.
    foreign key (site_id, organization_id)
        references sites (id, organization_id) on delete cascade,

    constraint ai_task_routes_position_check check (position >= 1),
    -- The task vocabulary is closed: the panel renders a row per key and a typo would render
    -- as a blank row nobody ever opens again.
    constraint ai_task_routes_task_check check (task in (
        'cheap', 'translation', 'coding', 'vision', 'long_context', 'embedding', 'critical'
    )),
    -- Four requirement chips, and no more. Each maps onto a capability the model must claim;
    -- a fifth kind would need a fifth place to enforce it.
    constraint ai_task_routes_requirements_check check (
        requirements <@ array['tools', 'vision', 'long_context', 'json']::text[]
    )
);

-- The ordered candidate list of one task at one scope. A candidate can be a null model (the
-- model was removed), so the uniqueness is on the position alone — two candidates of the same
-- task cannot share a slot.
create unique index ai_task_routes_scope_task_position_key
    on ai_task_routes (scope_key, task, position);

-- "What does this task resolve to here?" is the router's only read, on the hot path.
create index ai_task_routes_task_idx on ai_task_routes (task, scope_key, position);

create table ai_feature_overrides (
    id              uuid        primary key default gen_random_uuid(),
    scope_key       text        generated always as (
                        case
                            when site_id is not null then 'site:' || site_id::text
                            when organization_id is not null then 'org:' || organization_id::text
                            else 'installation'
                        end
                    ) stored,
    organization_id uuid        references organizations (id) on delete cascade,
    site_id         uuid        references sites (id) on delete cascade,
    feature         text        not null,
    model_id        uuid        not null references ai_models (id) on delete restrict,
    updated_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    foreign key (site_id, organization_id)
        references sites (id, organization_id) on delete cascade,

    -- One pin per feature per scope: a second row would make "which model does copilot use
    -- here?" depend on row order, and the resolver must be deterministic.
    constraint ai_feature_overrides_scope_feature_key unique (scope_key, feature)
);

-- The resolution walk asks "is this feature pinned at this site, that org, or the install?"
-- once per request, and the answer is a single row per scope.
create index ai_feature_overrides_feature_idx on ai_feature_overrides (feature, scope_key);
