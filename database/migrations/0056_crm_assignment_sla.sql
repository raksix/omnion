-- 0056_crm_assignment_sla.sql — who gets the lead, and how long they have.
--
-- REQ-117, slice 2 (assignment and SLA). Slice 1 stored the lead; nothing decided an owner and
-- nothing kept a clock. This migration is the two rule tables, the business-hours window, and
-- the organization business-hours setting the deadline arithmetic reads.
--
-- Four choices are worth stating, because each is a place the obvious table is wrong.
--
-- 1. **The round-robin cursor lives on the rule row, and moves under a row lock.** A cursor in
--    the process, or even a read-then-write, hands the same pool member two leads in a row the
--    moment there are two app instances. `claim_pool_slot` does the read and the write in one
--    transaction with `for update`, so the second caller waits and then reads the *next* value.
--    Fairness under concurrency is a property of the schema here, not of a caller.
--
-- 2. **A pool is an array on the rule, not a join table.** The pool is "the people eligible for
--    this rule", it has no attributes of its own, and it is always read whole. A join table
--    would need a position column to order an array the database already orders, and would
--    make the round-robin read a second query whose row order is not the operator's order.
--
-- 3. **`business_hours` is a jsonb window, not a calendar.** v1 has no holidays (the spec says
--    so out loud), so the shape is {days: [1..7], start: "09:00", end: "17:00", timezone}:
--    a weekly window in the organization's own zone. Storing the zone *with* the window rather
--    than in a global setting is what lets two organizations in two zones keep different
--    working days without a per-lead computation reaching out to a settings table.
--
-- 4. **The seed is one catch-all rule and one policy per organization, and both are
--    `active = true` only when something reads them.** Slice 2 ships the worker, the
--    evaluator and the clock, so unlike slice 1's reasoning these rows are not decoration.
--    Still, the seed is deliberately *unassigned/240 minutes*: a lead that arrives before
--    anybody has configured the org lands in the visible unassigned queue with a deadline,
--    which is the state an operator can act on — rather than being silently owned by nobody
--    with no clock at all.

create table crm_assignment_rules (
    id                  uuid        primary key default gen_random_uuid(),
    organization_id     uuid        not null references organizations (id) on delete cascade,
    name                text        not null,
    -- Evaluation order, top-down. The position is dense-ish (the reorder endpoint renumbers
    -- on write) so "the first rule that matches" is a single indexed scan rather than an
    -- order-by over a small table that Postgres may choose to do badly.
    position            integer     not null default 0,
    -- {country: [...], region: [...], product_interest: [...], budget_band: [...],
    --  source_id: [...], source_name: [...], language: [...], has_email: bool}
    -- An absent key matches everything; a present key with an empty array matches nothing.
    -- That distinction is the whole reason this is jsonb and not a text[]: "no opinion about
    -- the country" and "the country must be one of these zero countries" are different rules.
    conditions          jsonb       not null default '{}'::jsonb,
    -- `user` hands every matching lead to one person; `pool` walks the pool round-robin;
    -- `queue` is the explicit "somebody must pick this up" target, which is the same as no
    -- owner but is *chosen*, so the rule is visible on the lead's timeline.
    target_kind         text        not null,
    target_user_id      uuid        references users (id) on delete set null,
    pool_user_ids       uuid[]      not null default '{}',
    -- The atomic round-robin cursor. Advanced only inside the row-locked claim, so it is a
    -- modulo of itself, never a counter incremented by a reader.
    round_robin_cursor  integer     not null default 0,
    active              boolean     not null default true,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint crm_assignment_rules_target_check
        check (target_kind in ('user', 'pool', 'queue')),
    -- A rule that claims to target something must actually target it. Without this, a typo
    -- saves a rule that matches every lead and silently hands it to nobody.
    constraint crm_assignment_rules_target_present_check
        check (
            (target_kind = 'user' and target_user_id is not null)
            or (target_kind = 'pool' and cardinality(pool_user_ids) > 0)
            or target_kind = 'queue'
        ),
    constraint crm_assignment_rules_organization_name_key unique (organization_id, name)
);

create index crm_assignment_rules_order_idx
    on crm_assignment_rules (organization_id, position)
    where active;

create table crm_sla_policies (
    id                      uuid        primary key default gen_random_uuid(),
    organization_id         uuid        not null references organizations (id) on delete cascade,
    name                    text        not null,
    first_response_minutes  integer     not null default 240,
    business_hours_only     boolean     not null default false,
    -- The reminder goes out this many minutes before the deadline. `null` is "no reminder";
    -- equal to the response target is refused, because a reminder that fires at the same
    -- instant as the breach is a reminder nobody reads.
    reminder_minutes        integer,
    escalate_to_user_id     uuid        references users (id) on delete set null,
    -- {days: [1,2,3,4,5], start: "09:00", end: "17:00", timezone: "Europe/Istanbul"}
    -- The organization's own weekly window. No holidays in v1, deliberately: a calendar that
    -- silently treats a public holiday as a working day is worse than a documented gap.
    business_hours          jsonb       not null default '{}'::jsonb,
    active                  boolean     not null default true,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    constraint crm_sla_policies_minutes_check
        check (first_response_minutes between 1 and 20160),
    constraint crm_sla_policies_reminder_check
        check (reminder_minutes is null
               or (reminder_minutes between 1 and 20160
                   and reminder_minutes <> first_response_minutes))
);

-- One catch-all per organization. It is the rule that explains the unassigned queue: a lead
-- that matched nothing above lands here, and "queue" is how a rule says "and then a person
-- picks it up" instead of "nobody claimed it by accident".
insert into crm_assignment_rules (organization_id, name, position, conditions, target_kind)
select o.id, 'Default (unassigned queue)', 1000, '{}'::jsonb, 'queue'
from organizations o;

-- The organization-wide business hours, on the policy itself rather than in a settings table:
-- the deadline arithmetic reads exactly one row, and a policy that overrides it can carry a
-- different window without a second concept of "the organization's hours".
insert into crm_sla_policies (organization_id, name, first_response_minutes, business_hours_only)
select o.id, 'Web default', 240, false
from organizations o;

-- The worker reads "every open lead with a deadline, oldest first" on each tick. The
-- partial index from slice 1 already covers exactly this predicate; the reminder pass is the
-- same read bounded to a window, so it needs no index of its own.
