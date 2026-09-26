> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/inventory`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Stock across one or many warehouses.

- **Items** (SKU, barcode, unit, category, min/max thresholds, reorder point).
- **Locations & warehouses** with per-location stock, transfers between locations.
- **Movements ledger** (receipt, issue, transfer, adjustment) — append-only with reason codes.
- **Low-stock alerts** producing notifications + optional automation rule.
- **Counting / stocktake** session with variance report.
- **Views**: stock list with filters (below threshold, negative, idle), item detail with movement history.
- **Events**: `inventory.item.created`, `inventory.stock.low`, `inventory.movement.recorded`.

## Implementation spec

> **Module:** `modules/inventory` (crate `omnion-module-inventory`, workspace member) · **Migration:** `database/migrations/0013_inventory.sql` (next free slot at build time) · **Admin routes:** `/inventory/*` · **Permission family:** `inventory.*` · **Depends on:** core crates + `modules/sales` (REQ-052) for the sellable catalog and order reservations + `modules/approvals` (REQ-059) for adjustment approval above a threshold.

### Scope (in / out)

**In**

- Items: SKU, barcode (EAN-13/Code-128 as text), name, category, unit, min/max threshold, reorder point, reorder quantity, active/archived, optional link to a sales catalog product.
- Warehouses and locations (`WH-A` → `Raw materials`, `Finished goods`, `Returns`) with per-location stock and per-warehouse totals.
- Append-only movement ledger: receipt, issue, transfer out/in, adjustment, reserve, release — each with a reason code, actor, source document and timestamp.
- Transfers between locations with two-step (draft → dispatched → received) semantics and a printable pick list.
- Low-stock alerts: threshold crossings produce a notification (REQ-021) and emit an event an automation rule can react to.
- Stocktake sessions: freeze a scope, enter counted quantities, produce a variance report, close with adjustments written to the ledger (optionally through an approval).
- Views: stock list with filters (below threshold, negative, idle 30/60/90 days), item detail with movement history and per-location breakdown, movement ledger screen, barcode lookup box.

**Out (tracked elsewhere)**

- Sellable catalog fields (price, tax class, price lists) → REQ-052. Purchase orders from suppliers → a future `modules/purchases` request (this REQ only records receipts). Bills of materials and manufacturing consumption → REQ-061.
- Notifications transport → REQ-021. Report engine/charts → REQ-028 (inventory exposes its aggregates). Print/PDF templates → REQ-029.

### Screens (UI)

Module nav: **Overview · Items · Stock · Movements · Transfers · Stocktake · Warehouses · Reports**.

| Route | Screen |
|---|---|
| `/inventory` | Overview: stock value-lite (qty × item cost if set), items below threshold, negative stock, open transfers, open stocktake, movements today |
| `/inventory/items` | Item list (table) |
| `/inventory/items/new`, `/inventory/items/{id}` | Item create form / item detail (stock per location, movement history, thresholds, linked product) |
| `/inventory/stock` | Stock levels list (item × location) |
| `/inventory/movements` | Ledger list with filters and a "record movement" drawer |
| `/inventory/transfers`, `/inventory/transfers/new`, `/inventory/transfers/{id}` | Transfer list / create / detail with dispatch + receive steps |
| `/inventory/stocktake`, `/inventory/stocktake/{id}` | Stocktake list / counting sheet with variance column and close action |
| `/inventory/warehouses` | Warehouse + location tree editor (add/rename/deactivate; no hard delete with stock) |
| `/inventory/reports` | Stock value, movement summary by period/item/location, idle stock, variance history |

**Items list** — columns: `SKU`, `Name` (link), `Category`, `Unit`, `On hand` (sum across locations), `Reserved`, `Available`, `Threshold` (min / reorder), `Status` (badge: OK / Low / Below reorder / Negative), `Barcode`, `Updated`. Filters: search (SKU/name/barcode, barcode field accepts a scanner paste and searches on enter), category, warehouse, status (low / negative / idle), active toggle, "linked to catalog" toggle. Bulk: activate, archive, export CSV, set category. Row actions: open, adjust stock (drawer), transfer, print barcode label. Shortcuts: `/` search, `b` focus barcode box, `n` new item, `a` adjust selected, `enter` open, `?` help. States: skeleton table, empty state with "Create item" + "Import CSV" (import reuses REQ-031 mapping), error state with retry.

**Item form** — fields: SKU (required, unique per organization `^[A-Za-z0-9._-]{2,32}$`), Name (required ≤160), Category (combobox with create), Unit (select + custom, required), Barcode (optional, `^[0-9A-Za-z-]{6,32}$`, unique when set), Min threshold (≥ 0, default 0), Reorder point (≥ min), Reorder quantity (> 0), Cost (≥ 0, optional, currency from the organization), Link to catalog product (combobox over REQ-052 products, optional), Tracking notes (≤2000). Validation messages under the field, first invalid focused.

**Stock levels** (`/inventory/stock`) — columns: `Item`, `SKU`, `Warehouse`, `Location`, `On hand`, `Reserved`, `Available`, `Threshold`, `Last movement` (relative + absolute tooltip), `Status`. Filters: search, warehouse, location, status, idle days. Bulk: move to location, export. Row action: adjust (opens the same drawer as the ledger screen, prefilled with item + location). Negative or below-threshold rows carry a red/amber badge plus a text label (never colour alone).

**Adjust stock drawer** (used from items, stock, movements) — Item (locked when opened from a row), Location (required), New counted quantity **or** delta (radio; delta default), Reason code (required: `purchase_receipt`, `sale_shipment`, `customer_return`, `supplier_return`, `damage`, `loss`, `correction`, `internal_use`, `stocktake_variance`, `transfer`), Note (optional ≤500), Document reference. Validation: quantity non-zero, adjustment delta cannot make `on_hand` negative unless the reason is `correction` and the caller holds `inventory.negative.manage`; an absolute adjustment above the organization threshold requires `inventory.adjustment.approve` (or creates a REQ-059 approval). The drawer shows the resulting quantity before saving.

**Movements ledger** — an append-only table, no edit or delete control exists. Columns: `When`, `Item`, `SKU`, `Kind` (badge), `Quantity` (signed, green/red with sign), `Location`, `Reason`, `Source` (`order Q-2026-0007`, `transfer TR-12`, `manual`), `Actor`. Filters: date range (default last 30 days), item, kind (multi), reason, location, actor, source reference. Export CSV. Detail side panel shows the full row plus the resulting on-hand after that row.

**Transfers** — create form: From/To location (must differ), lines (item, quantity, note; each line must have enough available at the source), scheduled date, note. Detail: status stepper (Draft → Dispatched → Received → Cancelled), `Dispatch` moves stock out (in-transit), `Receive` books it in at the target, `Cancel` before dispatch releases the hold. Both actions write ledger rows and history. A partially received transfer is allowed per line and keeps the remainder open.

**Stocktake** — create: scope (warehouse or location), category filter, include zero-stock toggle; the session freezes the scope and produces a counting sheet (item, location, expected, counted input, variance computed live, note). The sheet supports barcode entry (scan fills the next empty row), keyboard `tab`-through, and a "filter: only deviations" toggle. Close: writes one `stocktake_variance` movement per non-zero variance in one transaction (above threshold → approval first), stores a variance report snapshot, marks the session closed.

**Warehouses** (`/inventory/warehouses`) — tree: warehouse → locations, add/rename/deactivate, per-node item count and stock value-lite. Deactivating a location with stock is refused with an explicit message.

**Mobile:** stock list and item detail are card-based with the status badge first; the adjust drawer becomes a full-screen sheet with number keyboard; stocktake sheet is optimised for one-hand scanning (large inputs, sticky item header); no drag-only control exists.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/inventory/items` | Item list / create | `inventory.items.read` / `.create` |
| GET/PATCH/DELETE | `/api/v1/inventory/items/{id}` | Item detail / update / archive | `inventory.items.read` / `.update` / `.delete` |
| GET | `/api/v1/inventory/items/lookup` | Resolve by barcode or SKU (scanner path) | `inventory.items.read` |
| GET | `/api/v1/inventory/stock` | Stock levels (filters, paging, aggregate) | `inventory.stock.read` |
| GET/POST | `/api/v1/inventory/movements` | Ledger list / record a movement (receipt, issue, adjustment) | `inventory.movements.read` / `.record` |
| POST | `/api/v1/inventory/movements/{id}/approve` | Approve an over-threshold adjustment | `inventory.adjustment.approve` |
| GET/POST | `/api/v1/inventory/warehouses` | Warehouse list / create | `inventory.locations.read` / `.manage` |
| PATCH/DELETE | `/api/v1/inventory/warehouses/{id}` | Rename / deactivate | `inventory.locations.manage` |
| GET/POST | `/api/v1/inventory/locations` | Location list / create (under a warehouse) | `inventory.locations.read` / `.manage` |
| GET/POST | `/api/v1/inventory/transfers` | Transfer list / create draft | `inventory.transfers.read` / `.manage` |
| POST | `/api/v1/inventory/transfers/{id}/dispatch` | Book out at the source (in-transit) | `inventory.transfers.manage` |
| POST | `/api/v1/inventory/transfers/{id}/receive` | Book in at the target (per line) | `inventory.transfers.manage` |
| POST | `/api/v1/inventory/transfers/{id}/cancel` | Cancel and release | `inventory.transfers.manage` |
| GET/POST | `/api/v1/inventory/stocktakes` | Session list / create (scope) | `inventory.stocktake.read` / `.manage` |
| GET | `/api/v1/inventory/stocktakes/{id}` | Counting sheet + variance | `inventory.stocktake.read` |
| PUT | `/api/v1/inventory/stocktakes/{id}/lines` | Save counted quantities (batch) | `inventory.stocktake.manage` |
| POST | `/api/v1/inventory/stocktakes/{id}/close` | Close and post variances | `inventory.stocktake.manage` |
| POST | `/api/v1/inventory/reservations` | Reserve / release for a sales order (service call) | service account or `inventory.movements.record` |
| GET | `/api/v1/inventory/reports/summary` | Value, movement summary, idle stock, variance history | `inventory.reports.read` |
| GET | `/api/v1/inventory/reports/export` | CSV of any of the above | `inventory.reports.read` |

Every list endpoint accepts `?warehouse=&location=&status=&item=&from=&to=&cursor=&limit=` and returns `{ items, next_cursor, total_estimate }`.

### Data model

```text
inventory_items(id uuid pk, organization_id uuid not null, sku text not null, name text not null,
  category text, unit text not null, barcode text, min_threshold numeric(14,3) not null default 0,
  reorder_point numeric(14,3) not null default 0, reorder_qty numeric(14,3) not null default 0,
  cost numeric(14,2), currency char(3) not null default 'USD', product_id uuid, notes text not null default '',
  active boolean not null default true, archived_at timestamptz)
inventory_warehouses(id uuid pk, organization_id uuid not null, code text not null, name text not null,
  active boolean not null default true)
inventory_locations(id uuid pk, organization_id uuid not null, warehouse_id uuid not null,
  code text not null, name text not null, kind text not null default 'internal', active boolean not null default true)
inventory_stock(id uuid pk, organization_id uuid not null, item_id uuid not null, location_id uuid not null,
  on_hand numeric(14,3) not null default 0, reserved numeric(14,3) not null default 0,
  last_movement_at timestamptz, updated_at timestamptz not null default now())
inventory_movements(id bigint identity pk, organization_id uuid not null, item_id uuid not null,
  location_id uuid not null, kind text not null, quantity numeric(14,3) not null, reason text not null,
  source_kind text, source_id uuid, note text not null default '', on_hand_after numeric(14,3) not null,
  reserved_after numeric(14,3) not null, actor_user_id uuid, created_at timestamptz not null default now())
inventory_transfers(id uuid pk, organization_id uuid not null, number text not null, from_location_id uuid not null,
  to_location_id uuid not null, transfer_status text not null default 'draft', scheduled_on date, note text,
  created_by uuid, dispatched_at timestamptz, received_at timestamptz, cancelled_at timestamptz)
inventory_transfer_lines(id uuid pk, transfer_id uuid not null references inventory_transfers(id) on delete cascade,
  item_id uuid not null, quantity numeric(14,3) not null, received_qty numeric(14,3) not null default 0)
inventory_stocktakes(id uuid pk, organization_id uuid not null, code text not null, scope jsonb not null,
  stocktake_status text not null default 'open', lines_total integer not null default 0,
  variance_lines integer not null default 0, created_by uuid, closed_at timestamptz)
inventory_stocktake_lines(id uuid pk, stocktake_id uuid not null references inventory_stocktakes(id) on delete cascade,
  item_id uuid not null, location_id uuid not null, expected numeric(14,3) not null, counted numeric(14,3), note text)
inventory_alerts(id uuid pk, organization_id uuid not null, item_id uuid not null, location_id uuid,
  kind text not null, threshold numeric(14,3), observed numeric(14,3) not null, raised_at timestamptz not null default now(),
  notified_at timestamptz, cleared_at timestamptz)
```

Checks: `kind in ('receipt','issue','transfer_out','transfer_in','adjustment','reserve','release')`; sign rule `(kind in ('receipt','transfer_in','release') and quantity > 0) or (kind in ('issue','transfer_out','reserve') and quantity > 0) or kind = 'adjustment'` with adjustments allowed either sign but never zero; `reorder_point >= min_threshold`; `reorder_qty >= 0`; `reserved >= 0 and on_hand >= 0` unless a `correction` was recorded (enforced in the service, mirrored by a check that forbids `reserved > on_hand`); unique `(organization_id, lower(sku))`, unique `(organization_id, barcode)` where barcode is not null, unique `(warehouse_id, lower(code))`, unique `(organization_id, number)` on transfers, unique `(stocktake_id, item_id, location_id)` on lines.

Indexes: `inventory_stock_item_loc_key (item_id, location_id)`, `inventory_stock_low_idx (organization_id) where on_hand - reserved < 0` (plus a per-item threshold check in the alert sweep), `inventory_movements_org_created_idx (organization_id, created_at desc)`, `inventory_movements_item_idx (item_id, created_at desc)`, `inventory_movements_source_idx (source_kind, source_id)`, `inventory_alerts_open_idx (organization_id, raised_at desc) where cleared_at is null`.

`inventory_stock` is a rollup of `inventory_movements`: every movement writes the ledger row **and** updates the stock row in the same transaction (`select … for update` on the stock row), so `on_hand_after`/`reserved_after` in the ledger always matches the rollup and a replay check can prove it. Migration: `database/migrations/0013_inventory.sql`, additive; seeds one warehouse (`MAIN`) with `STOCK` and `RETURNS` locations per existing organization.

### Events

Emitted: `inventory.item.created`, `inventory.item.updated`, `inventory.location.changed`, `inventory.movement.recorded` (kind, quantity, reason, resulting on-hand), `inventory.stock.low` (threshold crossed downwards), `inventory.transfer.dispatched`, `inventory.transfer.received`, `inventory.stocktake.closed` (variances count). Consumed: `sales.order.confirmed` reserves the ordered quantities, `sales.order.cancelled` releases them; `manufacturing.order.released` (REQ-061) consumes raw materials; `accounting.invoice.issued` never touches stock (it is a financial fact only).

Low-stock alerts are raised once per crossing (not on every movement): the sweep compares `available` against the item's `min_threshold`/`reorder_point`, writes an `inventory_alerts` row, notifies `inventory.alerts.receive` holders through REQ-021, and emits `inventory.stock.low` — which a REQ-003 automation can turn into a purchase request or a task. A cleared alert re-arms the trigger. All emitted names are subscribable webhooks with id-level payloads.

### Acceptance criteria

- [ ] Migration `0013_inventory.sql` applies on a populated database; `cargo test -p omnion-module-inventory` is green.
- [ ] Every `/api/v1/inventory/*` route is permission-guarded; a sibling organization's item id answers 404.
- [ ] Item create/update/archive, movement recording, transfer steps and stocktake actions write audit entries with before/after quantities.
- [ ] The ledger is append-only: no API path can update or delete a movement (a direct PATCH answers 405).
- [ ] Every movement updates `inventory_stock` in the same transaction; a reconciliation test replays the ledger and matches `on_hand` for every item × location.
- [ ] Recording a receipt, an issue and an adjustment produces the expected on-hand and the correct sign in the ledger and the stock list.
- [ ] A negative stock is refused by default and allowed only with reason `correction` plus the negative-stock permission; both cases are covered by tests.
- [ ] Adjustments above the organization threshold create an approval and do not change stock until it is approved.
- [ ] Low stock raises exactly one alert and one notification per crossing; a restock clears it, and the next crossing alerts again.
- [ ] Transfer dispatch moves the quantity out of the source and into in-transit, receive books it in at the target, cancel before dispatch leaves stock untouched.
- [ ] A transfer line cannot exceed the available quantity at the source (422 with the available number in the message).
- [ ] Stocktake freezes the scope, computes variance live, posts one variance movement per deviation on close, and writes a variance report that reopens correctly.
- [ ] Stock list filters (below threshold, negative, idle 30/60/90) return the right rows and the CSV export matches the table.
- [ ] Barcode lookup returns the right item, opens its detail and focuses the adjust drawer when started from the scanner box.
- [ ] Reports return stock value-lite, movement summary for the period and idle stock for the filter set; export works.
- [ ] `sales.order.confirmed` reserves stock and the reservation is visible in both the order and the stock list; cancel releases it.
- [ ] Global search finds items by SKU/barcode/name; ⌘K offers "Record movement" and "New item" gated by permission.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no fabricated numbers.
- [ ] Mobile 390×844: stock list, item detail, adjust drawer and stocktake sheet are usable one-handed.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/inventory`, `/inventory/items`, `/inventory/stock`, `/inventory/movements`, `/inventory/transfers`, `/inventory/stocktake`, `/inventory/warehouses`, `/inventory/reports` (desktop) and `/inventory/items`, `/inventory/stock` (mobile). The script must: create an item → record a receipt of 10 → record an issue of 4 → see on-hand 6 → adjust to a below-threshold value and observe the low-stock badge, the notification and the event → create a transfer and dispatch/receive it → run a stocktake with one deviation and close it → confirm the variance ledger row and report. It clicks every visible control on each screen (including the scanner box, the reason-code select and the stocktake `tab`-through inputs).

Visual check: stock list shows status badges with text labels (not colour alone), a sticky header and right-aligned quantities; the ledger shows signed quantities with distinct but accessible colours and a "no edits" affordance (no pencil icon on rows); the stocktake sheet shows expected/counted/variance columns with the deviation row highlighted; the timeline of the movement drawer shows the resulting quantity. Screenshots: `page-inventory-items`, `page-inventory-stock`, `page-inventory-movements`, `page-inventory-stocktake`, `mobile-inventory-stock`. Zero high findings; no horizontal overflow of the stock table at 1440 px; AA contrast on the red/amber badges.

### Slices

1. **Items, warehouses, locations + stock rollup (data, API, screens).** Migration, item CRUD, warehouse/location tree, stock list, reconciliation test. Done when an item is created and its on-hand matches the sum of its movements in a test.
2. **Ledger + adjust drawer.** Record movement (all kinds), reason codes, approval path for over-threshold adjustments, ledger screen with filters and CSV export. Done when QA records a receipt, an issue and an adjustment and the ledger, stock list and audit trail all agree.
3. **Transfers + low-stock alerts.** Transfer draft/dispatch/receive/cancel, alert sweep, notifications and the `inventory.stock.low` event with an automation example. Done when a transfer completes end to end and a stock crossing produces one notification plus one event.
4. **Stocktake + reports + sales integration.** Session create/count/close with variance report, reports screen + CSV, order reservation/release hooks. Done when a stocktake posts variances and the sales order flow reserves and releases stock visibly.

### Risks / notes

- **Concurrency is the whole game:** every movement locks the item × location stock row (`for update`) before computing the new quantity; the ledger row and the rollup must never disagree, and a test replays the ledger to prove it.
- **Never delete:** items, locations and movements are archived, never removed — a historical ledger that points at a deleted row is worthless.
- **Catalog duality:** items can exist without a catalog product and products without an item; the link is optional and the UI must say which side it is looking at (an inventory item is not necessarily sellable).
- **Threshold alerts must be edge-triggered**, otherwise a busy warehouse floods the notification centre.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
