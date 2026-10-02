-- Omnion · 0138 · Stock transfers and low-stock alerts (docs/requests/REQ-053, slice 3)
--
-- Slice 1 shipped the `in_transit` location kind, the `transfer_out`/`transfer_in` movement kinds
-- and the rule that a hand-written movement may **not** be a transfer. Slice 2 shipped the
-- approval path. Both left the same shape of gap this migration closes: **the schema can be right
-- and the feature absent.** A transfer kind with no transfer document is two kinds nobody can
-- produce; an alert event with no alert table is a notification that fires forever.
--
-- ## Why a transfer is a document and not two movements
--
-- The obvious cheaper design is "a dispatch is a `transfer_out`, a receive is a `transfer_in`, and
-- the two find each other by `source_id`". It is wrong for a reason the ledger module already
-- knows: the ledger is append-only and replayable, and a pairing that lives in the *application*
-- rather than in a row means a replay cannot see it. Worse, it makes the two halves of one
-- physical move two independent facts, and the state "the goods left and nobody knows where they
-- are" becomes representable — which it is not, physically.
--
-- So `inventory_transfers` is a **document** with a status, and each step writes its movement
-- rows through the module's one write path (`record_movement`), carrying the transfer as
-- `source_kind = 'transfer'`. The document is what says "these two rows are one move", and
-- `replay` still proves the rollup from the movements alone.
--
-- ## The two-step shape, and what each step does to stock
--
-- `draft → dispatched → received`, with `cancelled` reachable from `draft` (nothing has moved) and
-- from `dispatched` (the goods come back; the return is itself a transfer out of transit and in at
-- the source, so the ledger still balances).
--
--   dispatch:  one `transfer_out` at the source (stock leaves), one `transfer_in` at the
--              **in-transit** location of the same organization (stock arrives, somewhere real).
--   receive:   one `transfer_out` at in-transit, one `transfer_in` at the target.
--
-- The in-transit leg is not decoration. Without it, dispatching would delete stock from the
-- organization for as long as the goods are on a van, and the stock list — which sums
-- `inventory_stock` — would report a hole that does not exist anywhere physical. The transit
-- location is created per organization by the seed in `0126`; it is a real row in
-- `inventory_locations`, so the sum stays true and the transit quantity is *visible* as its own
-- line rather than hidden inside an arithmetic expression.
--
-- ## A transfer line cannot exceed what the source has
--
-- Enforced in the service, and the schema cannot mirror it: the available number is
-- `on_hand − reserved` **at dispatch time**, which is a function of the ledger, not a column.
-- The refusal is a `422` carrying the number in the sentence as well as in `details`, because
-- the person reading it is standing at the shelf with a cart — the same shape the issue movement
-- already uses.
--
-- ## Low-stock alerts: one per crossing, cleared by a restock, re-armed by the next crossing
--
-- The **edge** was shipped in slice 1: `inventory.stock.low` is emitted on the downward crossing
-- only, so a busy warehouse does not flood the automation log. This table is the half that was
-- missing, and it exists because an event is not a record:
--
-- * `inventory_alerts` is one row per open alert, unique per (organization, item, location, kind)
--   **while it is open** — the partial unique index is what makes "exactly one alert per
--   crossing" a database fact rather than a promise in a route handler;
-- * `cleared_at` is set when a movement takes the balance back above the threshold, and a
--   later crossing raises a **new** row rather than reviving the old one, so the history reads
--   as a series of episodes instead of one row whose meaning depends on two timestamps;
-- * `notified_at` records that the notification went out, so the sweep is idempotent: a second
--   run over the same balance sends nothing.
--
-- The threshold is stored **on the alert row** (`threshold`) as well as read from the item. A
-- historical alert whose threshold later changed is still a true statement about the moment it
-- was raised; an alert that re-reads the item is a historical record that changes with the item.

create table inventory_transfers (
    id                  uuid           primary key,
    organization_id     uuid           not null references organizations (id) on delete cascade,
    number              text           not null,
    status              text           not null default 'draft',
    from_location_id    uuid           not null references inventory_locations (id),
    to_location_id      uuid           not null references inventory_locations (id),
    scheduled_on        date,
    note                text           not null default '',
    created_by          uuid           references users (id),
    dispatched_at       timestamptz,
    received_at         timestamptz,
    cancelled_at        timestamptz,
    created_at          timestamptz    not null default now(),
    updated_at          timestamptz    not null default now(),

    constraint inventory_transfers_status check (status in ('draft', 'dispatched', 'received', 'cancelled')),
    -- A transfer to the location it came from moves nothing, and the in-transit legs below are
    -- written as separate `transfer_in` rows, so allowing it would produce a transfer whose
    -- dispatch and receive are the same place.
    constraint inventory_transfers_distinct_locations check (from_location_id <> to_location_id),
    -- Exactly the steps a status claims. `draft` has moved nothing, `dispatched` has left the
    -- source, `received` has landed, `cancelled` has stopped. A `dispatched` row with a null
    -- `dispatched_at` is a transfer the stock thinks happened and the document does not.
    constraint inventory_transfers_timestamps check (
        (status = 'draft' and dispatched_at is null and received_at is null and cancelled_at is null)
     or (status = 'dispatched' and dispatched_at is not null and received_at is null and cancelled_at is null)
     or (status = 'received' and dispatched_at is not null and received_at is not null and cancelled_at is null)
     or (status = 'cancelled' and received_at is null)
    )
);

-- The number is what a person reads on the paper pick list, so it is unique per organization and
-- the index is the constraint's business: a `create table ... unique` would be fine, but a
-- named index can be added by this migration to a table that already exists in a fork.
create unique index inventory_transfers_number_per_organization
    on inventory_transfers (organization_id, number);

-- The list screen opens on the open transfers, so the index is partial on those.
create index inventory_transfers_open
    on inventory_transfers (organization_id, created_at desc)
    where status in ('draft', 'dispatched');

create index inventory_transfers_organization
    on inventory_transfers (organization_id, updated_at desc);

create table inventory_transfer_lines (
    id              uuid           primary key,
    transfer_id     uuid           not null references inventory_transfers (id) on delete cascade,
    item_id         uuid           not null references inventory_items (id),
    quantity        numeric(14,3)  not null,
    received_qty    numeric(14,3)  not null default 0,
    note            text           not null default '',

    constraint inventory_transfer_lines_quantity_positive check (quantity > 0),
    -- "A partially received transfer is allowed per line" (the spec), so a line may sit between
    -- nothing and all of it. What may never happen is receiving more than was sent, or receiving
    -- a negative amount, and both are refused by the schema as well as by the service — the
    -- service has to check the in-transit balance, which the schema cannot see.
    constraint inventory_transfer_lines_received_in_range check (received_qty >= 0 and received_qty <= quantity)
);

create unique index inventory_transfer_lines_item_once
    on inventory_transfer_lines (transfer_id, item_id);

create index inventory_transfer_lines_transfer
    on inventory_transfer_lines (transfer_id);

-- --------------------------------------------------------------------------------------------
-- Low-stock alerts
-- --------------------------------------------------------------------------------------------

create table inventory_alerts (
    id                  uuid           primary key,
    organization_id     uuid           not null references organizations (id) on delete cascade,
    item_id             uuid           not null references inventory_items (id) on delete cascade,
    location_id         uuid           references inventory_locations (id) on delete cascade,
    kind                text           not null,
    threshold           numeric(14,3)  not null,
    observed            numeric(14,3)  not null,
    raised_at           timestamptz    not null default now(),
    notified_at         timestamptz,
    cleared_at          timestamptz,

    constraint inventory_alerts_kind check (kind in ('low_stock', 'negative_stock')),
    -- The threshold is recorded because an alert is a statement about one moment. Re-reading the
    -- item today would make yesterday's alert claim something that never happened.
    constraint inventory_alerts_threshold_non_negative check (threshold >= 0)
);

-- **This index is the "exactly one alert per crossing" rule.** It is partial on `cleared_at is
-- null`, so a closed episode does not block the next one: the restock clears the row, and the
-- next downward crossing inserts a new episode rather than reviving the old. A plain
-- `(item, location, kind)` unique index would make the second crossing fail, and a service-level
-- check would be a promise the database could not keep.
create unique index inventory_alerts_one_open_per_item_location
    on inventory_alerts (organization_id, item_id, coalesce(location_id, '00000000-0000-0000-0000-000000000000'::uuid), kind)
    where cleared_at is null;

-- The alert inbox is "open, newest first", which is what the partial index is for.
create index inventory_alerts_open
    on inventory_alerts (organization_id, raised_at desc)
    where cleared_at is null;

create index inventory_alerts_item
    on inventory_alerts (item_id, raised_at desc);

-- --------------------------------------------------------------------------------------------
-- Seed the in-transit location
-- --------------------------------------------------------------------------------------------

-- `0126` seeds `MAIN` / `STOCK` and `MAIN` / `RETURNS` for every organization. A transfer needs a
-- third one, and it is seeded by **extending the existing seed function** rather than by adding a
-- second seed with a second trigger.
--
-- That is the cheaper design for a week and the wrong one for a year. A second function means a
-- second `after insert on organizations` trigger, and the next reader has to know that *both*
-- have to run; the failure mode is a tenant that gets `STOCK` and `RETURNS` but no transit
-- location, which is indistinguishable from a bug in the dispatch handler until somebody
-- dispatches a transfer on a platform created after this migration. `create or replace` keeps
-- one seed, one trigger, and one place where "what a new tenant has" is answered.
--
-- The whole function is re-declared because PL/pgSQL has no "add a statement" — a replace that
-- left the old body out would delete the settings row and the other two locations, which is why
-- this migration carries the complete body rather than a fragment.
create or replace function omnion_inventory_seed_organization(target uuid)
returns void
language plpgsql
as $$
begin
    insert into inventory_settings (organization_id)
    values (target)
    on conflict (organization_id) do nothing;

    insert into inventory_warehouses (organization_id, code, name)
    values (target, 'MAIN', 'Main warehouse')
    on conflict (organization_id, lower(code)) do update set name = excluded.name;

    -- The two locations a new tenant needs, in one statement so a warehouse never exists without
    -- somewhere to put its stock. `STOCK` is where goods live; `RETURNS` is where a customer's
    -- return waits to be inspected, because a returned item counted straight back into stock is
    -- how a broken unit reaches a second buyer.
    insert into inventory_locations (organization_id, warehouse_id, code, name, kind)
    select target, w.id, seed.code, seed.name, seed.kind
    from inventory_warehouses w
    cross join (values
        ('STOCK', 'Stock', 'internal'),
        ('RETURNS', 'Returns', 'returns')
    ) as seed(code, name, kind)
    where w.organization_id = target and w.code = 'MAIN'
    on conflict (warehouse_id, lower(code)) do update
        set name = excluded.name, kind = excluded.kind;

    -- The third location, added by REQ-053 slice 3: where a dispatched transfer sits between two
    -- locations. It is a real row so that the stock list — which sums `inventory_stock` — reports
    -- goods in transit as a line rather than as stock that has vanished into an arithmetic
    -- expression. Nothing books stock here by hand, and a person cannot pick it as a transfer
    -- target either: a transfer that *ends* in transit is goods that arrived and were never put
    -- away, which the service refuses.
    insert into inventory_locations (organization_id, warehouse_id, code, name, kind)
    select target, w.id, 'TRANSIT', 'In transit', 'in_transit'
    from inventory_warehouses w
    where w.organization_id = target and w.code = 'MAIN'
    on conflict (warehouse_id, lower(code)) do update
        set name = excluded.name, kind = excluded.kind;
end;
$$;

-- Backfill for the organizations that exist today. The trigger `0126` installed on
-- `organizations` already points at this function, so every tenant created **after** this
-- migration gets the transit location from the same call — there is no second trigger to forget.
select omnion_inventory_seed_organization(id) from organizations;
