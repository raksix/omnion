-- 0051_notification_routes.sql — the declarative router (REQ-021, slice 3).
--
-- Slice 1 gave the platform a way to say *you*; this table is what makes it say *you* without
-- every module knowing the notification crate exists. A ticket module records a fact on the
-- bus; a row here says that fact is a `ticket` notification for whoever is assigned. Neither
-- module imports the other.
--
-- Two decisions are worth stating, because each is a place the obvious schema is wrong:
--
-- 1. **`recipient` is a column, not a join table.** The four recipient shapes (the actor, a
--    permission, a role, a payload field) are closed, and a router with a fixed vocabulary is
--    one an administrator can reason about. The alternative — a `notification_route_targets`
--    table with a target type — expresses the same four things in three tables and permits a
--    fifth shape nobody has thought of, which is how a router becomes a workflow engine.
--
-- 2. **The rule is not scoped by organization, but the rows it produces are.** A rule is a
--    *platform* statement about what a fact means ("a ticket creation is a ticket
--    notification"); scoping it would mean every tenant re-declares the platform's semantics
--    and a fresh install has an empty router, which is the state in which the feature is
--    invisible and therefore untested. The event's own organization still filters the
--    recipients a permission or role rule can resolve to, so a rule can never deliver across a
--    tenant boundary — the scoping lives in the *query*, not in the rule.

create table notification_routes (
    id              uuid        primary key default gen_random_uuid(),
    event_name      text        not null,
    category        text        not null,
    priority        text        not null default 'normal',
    -- One of: `actor`, `permission:<key>`, `role:<slug>`, `payload_user:<field>`. Stored as
    -- text with a check rather than as a Postgres enum because the router adds a shape by
    -- changing a Rust enum plus this list, and an `alter type … add value` cannot run inside
    -- a transaction on older servers.
    recipient       text        not null,
    title_template  text        not null,
    url_template    text,
    enabled         boolean     not null default true,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    -- The category and priority checks are the same lists as 0050, and the crate's
    -- `vocabulary.rs` carries the test that keeps the three in agreement.
    constraint notification_routes_category_check
        check (category in ('approval', 'security', 'update', 'ticket', 'system', 'mention')),
    constraint notification_routes_priority_check
        check (priority in ('low', 'normal', 'high', 'critical')),
    constraint notification_routes_recipient_check
        check (recipient = 'actor'
            or recipient like 'permission:%'
            or recipient like 'role:%'
            or recipient like 'payload_user:%'),
    -- A rule with an empty target is the author's mistake, not the installation's state: it
    -- resolves to nobody forever and reads as "the router is broken". Refused at the door.
    -- `actor` carries no target, so it is the one value allowed to have no second segment.
    constraint notification_routes_recipient_not_empty_check
        check (recipient = 'actor' or length(split_part(recipient, ':', 2)) > 0),
    constraint notification_routes_title_check
        check (length(trim(title_template)) > 0)
);

-- The router's only read: "every live rule for this event name", oldest first so two rules on
-- one event produce notifications in the order an administrator wrote them.
create index notification_routes_event_idx
    on notification_routes (event_name, created_at)
    where enabled = true;

-- One rule per (event, category, recipient) triple. Without it, an administrator who adds the
-- same rule twice gets two identical notifications per event, and the dedupe key does not
-- save them because it is per-recipient, not per-rule.
create unique index notification_routes_unique
    on notification_routes (event_name, category, recipient);
