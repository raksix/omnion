-- Omnion · 0146 · The order line a reservation belongs to (docs/requests/REQ-053, slice 5)
--
-- Slice 5 made a confirmed sales order actually hold stock, and it found a question the schema
-- could not answer: **"how much of this order is already held?"**
--
-- The ledger answers "how much is reserved at this location", and the rollup answers "how much
-- this organization has promised in total". Neither answers the third question, which is the one a
-- double-click raises, and the two available answers are both wrong in a way that is invisible:
--
-- * Asking the rollup returns the **organization's** total, so a retry would refuse a legitimate
--   second order for goods somebody else is waiting for — or, if it ignored the total, hold the
--   same stock twice.
-- * `sales_order_reservations` has the right answer, but it is the **sales** module's table. A
--   module reading another module's table to decide whether to write to a third table is a
--   dependency this crate's header explicitly says it does not have: the reservation bridge reads
--   `sales_orders` and `sales_order_lines` for the *lines* (which is the caller telling it what was
--   ordered) and owns everything after that.
--
-- So the ledger names the line itself, and the guard becomes a plain equality on a uuid rather than
-- a `like` over a note somebody can retype.
--
-- Additive and nullable: an existing row has no line, and a reserve written before this migration
-- is still a reserve. The column is a **pointer**, not a second copy of the quantity — the amount
-- comes from the ledger row, so a bad pointer makes the idempotence guard conservative (it holds
-- again) rather than destructive (it releases the wrong amount).

alter table inventory_movements
    add column order_line_id uuid references sales_order_lines (id) on delete set null;

-- The idempotence guard, and the release path that reads the same rows back. A reserve of one
-- order's line lives at one item × location, so this is the index that makes a second confirm a
-- lookup rather than a scan of the organization's whole ledger.
create index inventory_movements_order_line_idx
    on inventory_movements (organization_id, order_line_id, id)
    where order_line_id is not null;
