-- Omnion · 0143 · Stocktake sessions and the variance report (docs/requests/REQ-053, slice 4)
--
-- Slice 1 shipped `stocktake_variance` as a **reason code** and slice 1 shipped the
-- reconciliation report as a list of disagreements carrying both numbers. Both were the shape
-- with nothing behind it: a reason no code posts, and a report over a rollup nobody is holding
-- still. This migration is the feature.
--
-- ## A stocktake is a document, for the same reason a transfer is
--
-- A count is not a number somebody types at the end. It is a **document with a frozen scope**:
-- which locations were counted, which items were on the sheet when the count began, and what
-- each line was expected to be at that moment. Freeze the *expected* quantity and the sheet
-- becomes a statement about one instant; without that, "expected" is re-read at close time from
-- a rollup that the counting itself may have changed, and a variance of zero would be an
-- accident rather than a proof.
--
-- The expected quantity lives on the **line**, not on the session header, for the same reason
-- slice 3 put the threshold on the alert row: a historical record that re-reads a mutable column
-- is a record that changes with the column. A stocktake closed in March and reopened in June
-- must print March's expectations.
--
-- ## Closing writes the ledger, through the module's one write path
--
-- One `adjustment` movement with `reason = 'stocktake_variance'` per non-zero deviation, written
-- through `ledger::record_movement`. Not a bulk `update inventory_stock` — a session that
-- "corrects the rollup" in one statement leaves ledger rows that do not describe what happened,
-- and the next `replay` reports a disagreement the module manufactured itself. This is the exact
-- trap the transfer's in-transit leg avoided in slice 3, and it is why the same argument is
-- worth repeating here rather than assuming the reader carries it over.
--
-- ## Why a close is refused while a line is uncounted
--
-- The naive design posts a variance for every line and leaves uncounted ones alone. It is wrong
-- because an uncounted line is *not* a line of zero: it is a line nobody looked at, and posting
-- zero for it would quietly destroy stock to make the ledger agree with a sheet that was only
-- half finished. A close therefore requires every line to carry a counted quantity, and says how
-- many are missing.
--
-- ## The report reopens
--
-- A variance report that cannot be reopened is a screenshot. `GET /stocktake/{id}` reads the
-- document, its lines and the movements it caused, so a number that was posted in March is
-- still there in June, and the screen can show *both* numbers beside the deviation — the same
-- shape the reconciliation report already has.

create table inventory_stocktakes (
    id                  uuid           primary key,
    organization_id     uuid           not null references organizations (id) on delete cascade,
    number              text           not null,
    status              text           not null default 'open',

    -- The frozen scope. `location_ids` is the set of locations being counted; `category` is an
    -- optional narrowing to one category — a **text** column, because that is how `0126` stores
    -- an item's category, and a scope that could name a category the items do not use would be
    -- a filter the schema could not check. Both are stored as written: a stocktake that had been
    -- narrowed to a category and later widened would be a different document, and the sheet it
    -- printed last week is not the sheet it is closing today.
    location_ids        uuid[]         not null default '{}',
    category            text,

    -- What the count was for. A closing date is not derivable from `closed_at` when a session is
    -- open, and the screen groups by it, so it is written by the caller.
    counted_on          date,

    note                text           not null default '',
    created_by          uuid           references users (id),
    closed_by           uuid           references users (id),
    closed_at           timestamptz,
    created_at          timestamptz    not null default now(),
    updated_at          timestamptz    not null default now(),

    constraint inventory_stocktakes_status check (status in ('open', 'closed', 'cancelled')),

    -- Exactly the steps a status claims. An `open` session has posted no variance, so a
    -- `closed_at` on it is a row that says the count was finished and the ledger disagrees with
    -- it. The check constraint is the cheap half; the service holds the expensive half.
    constraint inventory_stocktakes_timestamps check (
        (status = 'open'     and closed_at is null     and closed_by is null)
     or (status = 'closed'   and closed_at is not null and closed_by is not null)
     or (status = 'cancelled' and closed_at is not null)
    ),

    -- The frozen counts, carried on the document so the list can sort by them without reading
    -- every line of every session. They are **written at close** and never afterwards, so they
    -- are a statement about the close rather than a live total.
    lines_counted       integer        not null default 0,
    variances_count     integer        not null default 0,
    variance_total      numeric(14,3)  not null default 0
);

-- The paper number, unique per organization — the same rule as a transfer.
create unique index inventory_stocktakes_number_per_organization
    on inventory_stocktakes (organization_id, number);

-- The list screen opens on the open sessions, which is what the partial index serves.
create index inventory_stocktakes_open
    on inventory_stocktakes (organization_id, created_at desc)
    where status = 'open';

create index inventory_stocktakes_organization
    on inventory_stocktakes (organization_id, updated_at desc);

create table inventory_stocktake_lines (
    id              uuid           primary key,
    stocktake_id    uuid           not null references inventory_stocktakes (id) on delete cascade,
    item_id         uuid           not null references inventory_items (id),
    location_id     uuid           not null references inventory_locations (id),

    -- **The frozen expectation.** Taken when the sheet was opened, not when the session closes:
    -- the count itself may post a variance, and a line that re-read the rollup at close time
    -- would report the variance it just wrote as a zero deviation.
    expected_qty    numeric(14,3)  not null,

    -- Null until somebody counts it. **Null and zero are different facts**: null is "nobody
    -- looked", zero is "there is nothing here", and a close that treated null as zero would
    -- destroy stock to match a sheet nobody finished.
    counted_qty     numeric(14,3),

    note            text           not null default '',

    constraint inventory_stocktake_lines_expected_non_negative check (expected_qty >= 0),

    -- A counted quantity can never be negative, and the null case is what makes this one
    -- constraint rather than two: **null and zero are different facts** — null is "nobody
    -- looked", zero is "there is nothing here", and a close that treated null as zero would
    -- destroy stock to match a sheet nobody finished. Letting a negative through would let a
    -- session drive stock below zero without the negative-stock rule ever being consulted;
    -- a shortfall is posted by the close as a signed `adjustment`, where the ledger's own
    -- negative rule applies.
    constraint inventory_stocktake_lines_counted_non_negative check (counted_qty is null or counted_qty >= 0)
);

-- One line per item × location, which is the shape of a stock row and therefore the shape of
-- something a person counts.
create unique index inventory_stocktake_lines_item_location_once
    on inventory_stocktake_lines (stocktake_id, item_id, location_id);

create index inventory_stocktake_lines_session
    on inventory_stocktake_lines (stocktake_id);

-- The report reads "this session's variance movements" through `source_kind`/`source_id`. The
-- index for that **already exists** (`0126`, partial on `source_id is not null`, and its own
-- comment names the stocktake), so this migration does not add a second one — a duplicate index
-- on the same predicate is dead weight that later reads as two answers to "which index serves
-- this read".

-- --------------------------------------------------------------------------------------------
-- The count permission
-- --------------------------------------------------------------------------------------------
--
-- A stocktake **changes the numbers on the shelf**, so closing one is a write to stock and is
-- guarded by `inventory.stocktake.manage` — a key of its own rather than a reuse of
-- `inventory.movements.record`, for the same reason that key is not implied by
-- `inventory.items.manage`: holding the general ledger key must not silently hand over the
-- ability to overwrite a location's balance in bulk.
--
-- The key is registered in the **Rust catalogue** (`crates/permissions/src/catalogue.rs`), not
-- here, which is where every other permission in this platform lives. A migration that inserted
-- into `permissions` directly would create a row the catalogue does not know about — and a
-- permission the admin UI cannot render is a permission nobody can grant.

