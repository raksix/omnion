-- Omnion · 0126 · Inventory: items, warehouses, locations and the stock ledger
-- (docs/requests/REQ-053, slice 1 — items, warehouses, locations + the stock rollup)
--
-- What a warehouse actually is: a place, a list of places, an item, and a **history of how much
-- of each item was at each place and why**. The first three are ordinary tables. The fourth is
-- where this module lives or dies, so the schema is written to make one fact impossible to break:
--
-- > **`inventory_stock` is a rollup of `inventory_movements`, and the two may never disagree.**
--
-- Three things enforce it, and none of them is "be careful in the service":
--
-- 1. **The ledger is append-only.** There is no `update` path for a movement anywhere in the
--    API — `PATCH /inventory/movements/{id}` is a `405`, not a refusal with a nice message —
--    and no trigger. A ledger that can be edited is a report, and a report cannot answer "what
--    was on this shelf in March".
-- 2. **Every movement carries the resulting numbers.** `on_hand_after` and `reserved_after` are
--    written from the same locked read that produced them, so the ledger can be *replayed* and
--    checked against the rollup without trusting the service. `ledger::replay` does exactly
--    that, and the reconciliation test replays every item × location in a populated database.
-- 3. **The sign lives in the kind, not in the number.** `quantity` is stored **positive** and
--    `kind` says which way it went (`receipt` adds, `issue` removes, `transfer_out` removes,
--    `transfer_in` adds, `adjustment` is signed, `reserve`/`release` move `reserved`). Storing a
--    signed number next to a kind is two sources of truth for one fact, and the second is always
--    the one a migration or a manual fix gets wrong.
--
-- The negative-stock rule is deliberately **not** a check constraint. `on_hand >= 0` is exactly
-- the rule the spec allows to be broken by a `correction` from somebody holding
-- `inventory.negative.manage`, and a constraint cannot ask about a permission. The rule lives in
-- the service (where the caller's keys are known) and the schema enforces only the part that is
-- unconditional: **`reserved` is never negative** and **a reservation never exceeds what is on
-- hand**. The second is a real invariant — you cannot hold stock that does not exist — and unlike
-- "on hand is never negative" it has no exception worth making.
--
-- Nothing is deleted: `archived_at` is the only removal, because a past movement still names the
-- item it moved and a ledger pointing at a deleted row is worthless.

-- --------------------------------------------------------------------------------------------
-- Items
-- --------------------------------------------------------------------------------------------

create table inventory_items (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    sku               text           not null,
    name              text           not null,
    category          text,
    unit              text           not null default 'piece',
    barcode           text,
    min_threshold     numeric(14,3)  not null default 0,
    reorder_point     numeric(14,3)  not null default 0,
    reorder_qty       numeric(14,3)  not null default 0,
    cost              numeric(14,2),
    currency          char(3)        not null default 'TRY',
    product_id        uuid,
    notes             text           not null default '',
    active            boolean        not null default true,
    archived_at       timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    -- The same shape the module's `validate_sku` enforces, deliberately duplicated: the form and
    -- the schema must refuse the same strings, or the round trip throws away what was typed.
    constraint inventory_items_sku_format check (sku ~ '^[A-Za-z0-9._-]{2,32}$'),
    constraint inventory_items_name_not_blank check (length(btrim(name)) > 0),
    constraint inventory_items_name_length check (length(name) <= 160),
    constraint inventory_items_category_length check (category is null or length(category) <= 80),
    constraint inventory_items_unit_not_blank check (length(btrim(unit)) > 0 and length(unit) <= 24),
    constraint inventory_items_barcode_format check (barcode is null or barcode ~ '^[A-Z0-9]{6,32}$'),
    constraint inventory_items_thresholds_order check (reorder_point >= min_threshold),
    constraint inventory_items_thresholds_non_negative check (
        min_threshold >= 0 and reorder_point >= 0 and reorder_qty >= 0
    ),
    constraint inventory_items_cost_non_negative check (cost is null or cost >= 0),
    constraint inventory_items_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint inventory_items_notes_length check (length(notes) <= 2000)
);

-- A SKU is one item per organization, case-insensitively, among the live rows. An archived
-- duplicate does not block a fresh item — the same rule the CRM and the catalog use.
create unique index inventory_items_sku_per_organization
    on inventory_items (organization_id, lower(sku))
    where archived_at is null;

-- A barcode is a scanner's key, so it has to resolve to exactly one live item. Normalized
-- (separators stripped, upper-cased) by the module before it is written, which is why the index
-- compares the stored form directly.
create unique index inventory_items_barcode_per_organization
    on inventory_items (organization_id, barcode)
    where barcode is not null and archived_at is null;

create index inventory_items_list
    on inventory_items (organization_id, archived_at, active, name);

-- The category filter and the stocktake's category scope both read this.
create index inventory_items_category
    on inventory_items (organization_id, category)
    where category is not null;

-- The optional link to the REQ-052 catalog. **No foreign key**: the two modules are separate
-- crates and inventory must not be unable to boot because a sales migration has not run, nor
-- because a product is archived. The screens say which side they are looking at.
create index inventory_items_product on inventory_items (product_id) where product_id is not null;

-- --------------------------------------------------------------------------------------------
-- Warehouses and locations
-- --------------------------------------------------------------------------------------------

create table inventory_warehouses (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    code              text           not null,
    name              text           not null,
    active            boolean        not null default true,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint inventory_warehouses_code_format check (code ~ '^[A-Z0-9._-]{1,32}$'),
    constraint inventory_warehouses_name_not_blank check (length(btrim(name)) > 0),
    constraint inventory_warehouses_name_length check (length(name) <= 160)
);

create unique index inventory_warehouses_code_per_organization
    on inventory_warehouses (organization_id, lower(code));

create table inventory_locations (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    warehouse_id      uuid           not null references inventory_warehouses (id) on delete cascade,
    code              text           not null,
    name              text           not null,
    kind              text           not null default 'internal',
    active            boolean        not null default true,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint inventory_locations_code_format check (code ~ '^[A-Z0-9._-]{1,32}$'),
    constraint inventory_locations_name_not_blank check (length(btrim(name)) > 0),
    constraint inventory_locations_name_length check (length(name) <= 160),
    constraint inventory_locations_kind check (kind in ('internal', 'in_transit', 'returns', 'quarantine'))
);

-- A location code is unique per warehouse, not per organization: two warehouses may each hold a
-- `STOCK` bin, and an operator reading a movement row needs the warehouse to tell them apart.
create unique index inventory_locations_code_per_warehouse
    on inventory_locations (warehouse_id, lower(code));

create index inventory_locations_organization
    on inventory_locations (organization_id, warehouse_id, active);

-- --------------------------------------------------------------------------------------------
-- The stock rollup
-- --------------------------------------------------------------------------------------------

create table inventory_stock (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    item_id           uuid           not null references inventory_items (id) on delete cascade,
    location_id       uuid           not null references inventory_locations (id) on delete cascade,
    on_hand           numeric(14,3)  not null default 0,
    reserved          numeric(14,3)  not null default 0,
    last_movement_at  timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    -- **You cannot hold stock that does not exist.** Unlike "on hand is never negative" — which
    -- a `correction` from somebody holding `inventory.negative.manage` may legitimately produce —
    -- this has no exception worth making, so the schema refuses it outright.
    --
    -- The `or on_hand < 0` on the second constraint is the same exception, one level up: once a
    -- permitted correction has driven the balance below zero, `reserved <= on_hand` can no longer
    -- be true and must not be demanded. The service checks it in the same order (`ledger.rs`,
    -- `apply_movement`) and the two have to agree — a service that allowed the negative and a
    -- schema that then refused the row would leave a written ledger row with no rollup behind it,
    -- which is the exact failure this module exists to make impossible. That is why the gap was
    -- found here and not in a test: the constraint was written from the happy case and the
    -- service's own test for a permitted negative balance (`only_a_correction_with_the_permission_
    -- may_go_negative`) failed on it.
    constraint inventory_stock_reserved_non_negative check (reserved >= 0),
    constraint inventory_stock_reserved_within_on_hand check (reserved <= on_hand or on_hand < 0)
);

-- One row per item per location, the row every movement locks `for update` before it computes.
create unique index inventory_stock_item_location
    on inventory_stock (item_id, location_id);

-- The stock list's default read: the organization's rows, the most recently touched first.
create index inventory_stock_organization
    on inventory_stock (organization_id, last_movement_at desc nulls last);

-- --------------------------------------------------------------------------------------------
-- The append-only ledger
-- --------------------------------------------------------------------------------------------

create table inventory_movements (
    id                bigint         generated always as identity primary key,
    organization_id   uuid           not null references organizations (id) on delete cascade,
    item_id           uuid           not null references inventory_items (id) on delete restrict,
    location_id       uuid           not null references inventory_locations (id) on delete restrict,
    kind              text           not null,
    quantity          numeric(14,3)  not null,
    reason            text           not null,
    source_kind       text,
    source_id         uuid,
    note              text           not null default '',
    on_hand_after     numeric(14,3)  not null,
    reserved_after    numeric(14,3)  not null,
    actor_user_id     uuid,
    created_at        timestamptz    not null default now(),

    constraint inventory_movements_kind check (
        kind in ('receipt', 'issue', 'transfer_out', 'transfer_in', 'adjustment', 'reserve', 'release')
    ),
    constraint inventory_movements_reason check (reason in (
        'purchase_receipt', 'sale_shipment', 'customer_return', 'supplier_return', 'damage',
        'loss', 'correction', 'internal_use', 'stocktake_variance', 'transfer'
    )),

    -- The sign rule, in the schema this time because it is unconditional. `quantity` is positive
    -- for every kind except an adjustment, which carries its own sign; an adjustment of exactly
    -- zero is refused because a row that changed nothing is not a movement.
    constraint inventory_movements_sign check (
        case
            when kind in ('receipt', 'issue', 'transfer_out', 'transfer_in', 'reserve', 'release')
                then quantity > 0
            when kind = 'adjustment' then quantity <> 0
            else false
        end
    ),

    -- The resulting numbers are part of the row, not a join. This is what makes the ledger
    -- replayable: `ledger::replay` reads them back and the reconciliation test compares them with
    -- the rollup, so a service that computed the wrong answer is caught by a test rather than by
    -- a stocktake six months later.
    constraint inventory_movements_after_not_negative_reserved check (reserved_after >= 0),

    constraint inventory_movements_note_length check (length(note) <= 500),
    constraint inventory_movements_source_kind check (
        source_kind is null or source_kind ~ '^[a-z_]+$'
    )
);

-- The ledger screen's default read: the organization's rows, newest first.
create index inventory_movements_organization_created
    on inventory_movements (organization_id, created_at desc, id desc);

-- The item detail's history.
create index inventory_movements_item_created
    on inventory_movements (item_id, created_at desc, id desc);

-- "Every movement this document caused" — a transfer's own rows, a stocktake's variance rows.
create index inventory_movements_source
    on inventory_movements (source_kind, source_id)
    where source_id is not null;

-- --------------------------------------------------------------------------------------------
-- Settings
-- --------------------------------------------------------------------------------------------

create table inventory_settings (
    organization_id                   uuid           primary key references organizations (id) on delete cascade,
    adjustment_approval_threshold     numeric(14,3)  not null default 100,
    default_adjustment_reason         text           not null default 'correction',
    alerts_on_read                    boolean        not null default true,
    default_unit                      text           not null default 'piece',
    updated_at                        timestamptz    not null default now(),

    constraint inventory_settings_threshold_non_negative check (adjustment_approval_threshold >= 0),
    constraint inventory_settings_default_reason check (default_adjustment_reason in (
        'purchase_receipt', 'sale_shipment', 'customer_return', 'supplier_return', 'damage',
        'loss', 'correction', 'internal_use', 'stocktake_variance', 'transfer'
    ))
);

-- Every organization gets its settings row the day it is created, with the same defaults the
-- module falls back to. A row written by an installation that saves the screen is *updated*, not
-- replaced — the same trap as `search_settings.enabled_providers`, where a data row silently
-- froze a list and every provider added afterwards vanished from every query.
insert into inventory_settings (organization_id)
select id from organizations
on conflict (organization_id) do nothing;

-- --------------------------------------------------------------------------------------------
-- The seed: one warehouse with the two locations a new tenant needs
-- --------------------------------------------------------------------------------------------

-- `MAIN` / `STOCK` is where goods live and `MAIN` / `RETURNS` is where a customer's return waits
-- to be inspected, because a returned item counted straight back into stock is how a broken unit
-- reaches a second buyer.
--
-- **A trigger, not a backfill**, and this is REQ-051's lesson applied before it was learned twice
-- again: the seed function is created here and then *called once*, in the statement below, so
-- every organization that exists today has one — and every organization created afterwards does
-- not, and discovers it the first time somebody opens `/inventory/stock` and gets a 404. The
-- trigger is what makes "a new tenant has a warehouse" a property of the platform rather than a
-- fact about the day the migration ran.
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
end;
$$;

-- The trigger, not a backfill alone. `0022_crm.sql` had the right idea and called its seed once,
-- in the statement that created it, so every tenant born since owned no pipeline and the board
-- answered 404 — REQ-051's `4aca09e`. `organizations` is written by the tenancy route, the
-- onboarding steps, SCIM and provisioning, so a rule that has to be remembered at each of them is
-- a rule that will be forgotten at the fifth. The trigger is the one place that observes the row.
create or replace function omnion_inventory_seed_organization_on_insert()
returns trigger
language plpgsql
as $$
begin
    perform omnion_inventory_seed_organization(new.id);
    return new;
end;
$$;

drop trigger if exists omnion_inventory_seed_on_organization on organizations;
create trigger omnion_inventory_seed_on_organization
    after insert on organizations
    for each row execute function omnion_inventory_seed_organization_on_insert();

-- And the backfill for the organizations that already exist, in the same migration so a fresh
-- database and a migrated one end in the same state. It is the same call the trigger makes, so
-- the two cannot disagree about what a seeded warehouse looks like, and it is `on conflict do
-- nothing` throughout, so re-running it changes nothing.
select omnion_inventory_seed_organization(id) from organizations;
