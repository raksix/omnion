> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/manufacturing`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Light manufacturing for product businesses.

- **Bills of materials** (BoM) with components, quantities, waste factor, per finished item.
- **Work orders**: create from a BoM (manually or from a sales order), states (planned/started/done), assignee.
- **Component consumption & output** posts inventory movements automatically.
- **Shop-floor view**: today's work orders, start/complete with quantity.
- **Costing-lite**: material + time estimate vs. actual.
- **Events**: `manufacturing.workorder.created`, `manufacturing.workorder.completed`.

## Implementation spec

### Scope (in / out)

**In**
- Bills of materials: one BoM per (product, version), component lines with quantity per finished unit, per-line waste percentage, optional scrap note, unit-cost roll-up preview, active/archived states, duplicate-as-new-version.
- Work orders: created manually (pick BoM + quantity) or from a sales order line; states `planned` → `started` → `done` (plus `cancelled`); assignee (one owner, optional team note); planned start, due date, priority.
- Consumption and output post inventory movements through the inventory ledger port (`modules/inventory`, REQ-053): required = component quantity × produced quantity × (1 + waste), consume in one or several steps, produce finished quantity, and every posting writes a `inventory_movements` row with `reason = 'manufacturing_consume' | 'manufacturing_produce'` and `source_ref = work_order_id`.
- Shop-floor view: today's and overdue work orders as large cards, start / pause / complete with a quantity stepper, built for tablets and gloves — big hit targets, numeric keypad friendly.
- Costing-lite: material cost from the inventory item's current unit cost at consumption time, time cost from logged minutes × rate, estimated vs actual variance per work order and per product.
- Shortages: consuming more than available is refused with the missing quantity; a partial consume is allowed when the inventory item allows negative stock (setting per item) and is then flagged.
- Events and audit for every state change.

**Out**
- MRP/planning runs, capacity scheduling, work centres and machines (a later phase, listed in docs/08-BUSINESS-SUITE.md), subcontracting, quality control, lot/serial traceability (comes with inventory depth).
- Cost accounting postings (accounting module owns journals); manufacturing exposes costs, it does not post them.
- BoM multi-level explosion (v1 is one level: a component may itself be a manufactured item, but the module does not recursively explode it in v1; a note on the line says so).

### Screens (UI)

| Route | Screen |
|---|---|
| `/manufacturing/boms` | BoM list |
| `/manufacturing/boms/<id>/edit` | BoM editor with component lines and cost roll-up |
| `/manufacturing/work-orders` | Work order list |
| `/manufacturing/work-orders/<id>` | Work order detail — components, output, time, movements, notes |
| `/manufacturing/shop-floor` | Shop-floor board |
| `/manufacturing/reports/costing` | Costing and variance report |

- **BoM list.** Columns: Product, Reference, Version, Components, Unit cost, Status (`active` / `archived`), Updated. Filters: product, status, text search. Bulk: Archive, Duplicate. Row actions: Edit, Duplicate, Archive, Create work order (prefills the work-order dialog with this BoM).
- **BoM editor.** Header: product picker (inventory items of type `product`), produced quantity (default 1) with unit, reference (auto-suggested `BoM-0007`), notes, active toggle. Lines table: item picker, quantity per produced unit, unit, waste % (0–100), scrap note, unit cost (read-only, from inventory), line total, remove. Footer: materials total, waste-adjusted total, per-unit cost, and a "cost preview" chip that warns when an item has no cost. Validation: at least one line, quantity > 0, no duplicate item on the same position, waste between 0 and 100. `⌘Enter` saves, `Esc` cancels, `⌥↑/↓` reorders a line. Mobile: lines become stacked cards with numeric fields full width.
- **Work order list.** Columns: Number, Product, Quantity (planned/produced), BoM, Status, Assignee, Planned start, Due, Progress. Filters: status (segmented), product, assignee, due range, `overdue only`. Bulk: Assign (one user), Start, Cancel (with reason input). Row: click opens detail; hover shows a quick "start" for planned orders with the current user as assignee.
- **Work order detail.** Header: number, product, quantity planned vs produced, status badge, assignee, due, actions `Start`, `Consume`, `Produce`, `Complete`, `Cancel`, `Assign`, `Log time`. Tabs: Components — table (Item, Required, Consumed, Remaining, Availability, Consume button opening a quantity dialog); Output — produced quantity, target vs produced progress, `Produce` dialog; Time — entries table (user, minutes, note, at) with a `Log time` form (minutes, note) and estimated vs actual bar; Movements — inventory ledger rows sourced by this work order (link to the inventory movement screen); Notes — free text plus audit trail. Completing with a shortage opens a confirm dialog showing exactly what is missing and, when the item forbids negative stock, blocks with a clear message plus a link to the item's stock.
- **Shop-floor board.** Cards grouped by status columns (`Planned today`, `In progress`, `Overdue`) with card content: number, product, planned/produced quantity, assignee, due time. Buttons on the card: `Start`, `Pause`, `+1 / +5 / custom quantity`, `Complete`. A "day" selector switches between today, tomorrow, and a specific date. Auto-refresh every 30 s with a manual refresh button and a "last updated" line. Offline/stale state shows a banner instead of silently showing stale numbers. Keyboard: `s` start, `p` pause, `c` complete on the focused card, arrow keys move focus. Touch targets at least 48 px; the layout is tested at 1024×768 and on a 390 px phone (cards stack, no horizontal scroll).
- **Costing report.** Columns: Work order, Product, Quantity, Material cost, Time cost, Total actual, Total estimated, Variance, Variance %. Filters: date range, product, status. Totals row and a per-product summary toggle; CSV export. Empty state explains that costs appear once work orders consume components.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/manufacturing/boms` | List · create BoM with lines | `manufacturing.read` · `manufacturing.boms.manage` |
| GET · PUT · DELETE | `/api/v1/manufacturing/boms/{id}` | Read · update (lines included) · archive | `manufacturing.read` · `manufacturing.boms.manage` |
| POST | `/api/v1/manufacturing/boms/{id}/duplicate` | Copy as a new version | `manufacturing.boms.manage` |
| GET | `/api/v1/manufacturing/boms/{id}/cost-preview` | Roll up costs for a proposed quantity | `manufacturing.read` |
| GET · POST | `/api/v1/manufacturing/work-orders` | List (`?status=&product=&assignee=&due_before=`) · create (bom_id + quantity, or sales_order_id + line) | `manufacturing.read` · `manufacturing.workorders.create` |
| GET · PATCH | `/api/v1/manufacturing/work-orders/{id}` | Read with components and movements · assign, edit dates, notes | `manufacturing.read` · `manufacturing.workorders.manage` |
| POST | `/api/v1/manufacturing/work-orders/{id}/start` · `/pause` · `/resume` | State transitions with audit rows | `manufacturing.workorders.manage` |
| POST | `/api/v1/manufacturing/work-orders/{id}/consume` | Consume an array of `{item_id, quantity}`; writes movements | `manufacturing.workorders.manage` |
| POST | `/api/v1/manufacturing/work-orders/{id}/produce` | Produce finished quantity; writes the output movement | `manufacturing.workorders.manage` |
| POST | `/api/v1/manufacturing/work-orders/{id}/complete` | Finish (refuses on shortage unless allowed) | `manufacturing.workorders.complete` |
| POST | `/api/v1/manufacturing/work-orders/{id}/cancel` | Cancel with reason; consumes nothing | `manufacturing.workorders.complete` |
| POST · DELETE | `/api/v1/manufacturing/work-orders/{id}/time-entries` | Log · remove a time entry | `manufacturing.workorders.manage` |
| GET | `/api/v1/manufacturing/shop-floor` | Today's board (`?date=&status=`) | `manufacturing.read` |
| GET | `/api/v1/manufacturing/reports/costing` | Costing rows with variance (`?from=&to=&product=`) | `manufacturing.read` |

Errors: `bom_empty`, `bom_item_duplicate`, `work_order_shortage` (payload lists missing quantities), `work_order_wrong_state`, `work_order_negative_stock_forbidden`.

### Data model

Migration: `0107_manufacturing.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
manufacturing_boms (id uuid pk, organization_id uuid not null -> organizations,
  product_item_id uuid not null,          -- inventory item of kind 'product'
  reference text not null, name text, quantity numeric(14,3) not null default 1 check (> 0),
  version integer not null default 1, notes text,
  status text in ('active','archived') not null default 'active',
  created_by uuid null -> users, created_at/updated_at timestamptz not null default now())
  unique (organization_id, reference, version); index (organization_id, product_item_id, status)
manufacturing_bom_lines (id uuid pk, bom_id uuid not null on delete cascade, position integer not null,
  item_id uuid not null, quantity numeric(14,3) not null check (> 0),
  unit text not null, waste_percent numeric(5,2) not null default 0 check (between 0 and 100),
  scrap_note text)                      unique (bom_id, position)
manufacturing_work_orders (id uuid pk, organization_id uuid not null, number text not null,
  bom_id uuid not null, product_item_id uuid not null,
  quantity_planned numeric(14,3) not null check (> 0),
  quantity_produced numeric(14,3) not null default 0 check (>= 0),
  status text in ('planned','started','paused','done','cancelled') not null default 'planned',
  assignee_user_id uuid null -> users, sales_order_id uuid null, sales_order_line text null,
  priority smallint not null default 0, planned_start timestamptz, due_at timestamptz,
  started_at/completed_at/cancelled_at timestamptz, cancel_reason text,
  created_by uuid null, created_at/updated_at timestamptz not null default now())
  unique (organization_id, number); index (organization_id, status, due_at)
  index (assignee_user_id, status); index (sales_order_id)
manufacturing_wo_components (id uuid pk, work_order_id uuid not null on delete cascade,
  item_id uuid not null, quantity_required numeric(14,3) not null,
  quantity_consumed numeric(14,3) not null default 0)   unique (work_order_id, item_id)
manufacturing_wo_time_entries (id uuid pk, work_order_id uuid not null on delete cascade,
  user_id uuid not null -> users, minutes integer not null check (between 1 and 1440),
  note text, rate_snapshot numeric(12,4), created_at timestamptz not null default now())
manufacturing_wo_costs (work_order_id uuid pk -> manufacturing_work_orders on delete cascade,
  material_cost numeric(14,2) not null default 0, time_cost numeric(14,2) not null default 0,
  estimated_cost numeric(14,2), currency text(3) not null default 'TRY',
  computed_at timestamptz not null default now())
```

Inventory integration is a port: `InventoryPort::post_movement(kind, item_id, quantity, reason, source_ref)` and `::availability(item_id)`. Until REQ-053 lands, the port is implemented by a stub that writes to the same table shape so the module is testable; the swap is a dependency change, not a rewrite.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `manufacturing.workorder.created` | Work order created | `work_order_id`, `number`, `bom_id`, `product_item_id`, `quantity_planned` |
| `manufacturing.workorder.started` · `.paused` · `.cancelled` | State transitions | `work_order_id`, `status`, `actor_user_id` |
| `manufacturing.workorder.consumed` | Components consumed | `work_order_id`, `lines[] {item_id, quantity}` |
| `manufacturing.workorder.completed` | Finished with produced quantity | `work_order_id`, `quantity_produced`, `material_cost`, `time_cost` |
| `manufacturing.bom.updated` | BoM lines changed | `bom_id`, `reference`, `version` |
| `manufacturing.shortage` | Consumption refused for lack of stock | `work_order_id`, `item_id`, `missing` |

Consumed: `sales.order.confirmed` (offer/or auto-create work orders for lines whose product has an active BoM), `inventory.item.updated` (refresh availability and cost caches), `inventory.stock.low` (surface a banner on the shop-floor board). Webhook relevance: `manufacturing.workorder.completed` is what downstream costing/reporting integrations listen to; payloads carry ids and quantities, never supplier or pricing details of third parties.

### Acceptance criteria

- [ ] A BoM with three lines saves, reopens with identical values, and its cost roll-up equals the sum of `quantity × unit cost × (1 + waste)` per line.
- [ ] Waste is applied at consumption: 10 units of a product with waste 5% and component quantity 2 requires 21 units.
- [ ] Creating a work order from a BoM copies the required quantities snapshot, so later BoM edits do not change an open work order.
- [ ] Creating from a confirmed sales order line pre-fills product, quantity and the linking fields.
- [ ] `start` moves `planned` → `started` and stamps `started_at`; a second `start` fails with `work_order_wrong_state`.
- [ ] Consuming writes one inventory movement per line with reason `manufacturing_consume` and the work order as source, visible in the Movements tab.
- [ ] Producing writes one `manufacturing_produce` movement for the finished item and raises `quantity_produced` without exceeding `quantity_planned + tolerance` (default 0).
- [ ] Consumption beyond availability is refused with the missing quantity in the error; the item setting that allows negative stock lets it through and flags the line.
- [ ] Completing with unfulfilled consumption is blocked by default and allowed only through the confirm dialog, whose result is recorded in the audit trail.
- [ ] The shop-floor board lists today's and overdue work orders, and Start / +5 / Complete on a card updates both the card and the work-order detail within one refresh.
- [ ] Time entries sum to the actual minutes on the detail bar, and the costing report shows estimated vs actual variance per work order.
- [ ] Costing uses the unit cost at consumption time (snapshot), so a later price change does not rewrite history.
- [ ] Cancel asks for a reason, consumes nothing, and leaves the work order visible with `cancelled` status.
- [ ] `manufacturing.workorder.created` and `manufacturing.workorder.completed` reach a subscribed endpoint with redelivery working.
- [ ] Permissions hold: a user with only `manufacturing.read` cannot start or consume (403), and cannot open the BoM editor.
- [ ] All screens render at 390 px and 1024 px widths without horizontal scroll; the shop-floor board passes the walkthrough with zero high findings.

### QA plan

The walkthrough must visit `/manufacturing/boms`, open the seeded BoM, edit a quantity, add and remove a line, save, then create a work order from it; on `/manufacturing/work-orders` it must exercise every filter and the bulk Assign action; on the work-order detail it must click Start, Consume (with an over-quantity attempt to see the shortage error), Produce, log time, Complete through the confirm dialog, and then open the Movements tab to see the ledger rows; it must open `/manufacturing/shop-floor` and use the card buttons on a seeded started order; and it must open `/manufacturing/reports/costing` and export CSV. Visual check: the BoM editor shows a populated lines table with a cost roll-up footer and no clipped numeric fields; the shop-floor cards are large, legible and grouped into the three columns at tablet width; the costing report has a real totals row; empty states appear for a filtered-to-nothing list rather than a blank panel.

### Slices

1. **BoM depth.** Migration `0107_manufacturing.sql` (BoM tables); BoM list, editor with lines, waste, cost roll-up and validation; duplicate/archive. *Done when:* acceptance 1–2 pass and `/manufacturing/boms` is in the walkthrough inventory.
2. **Work orders and stock.** Work-order tables and CRUD, create from BoM and from a sales order, component snapshot, start/pause/consume/produce/complete, inventory ledger port with both movement reasons, shortage handling, audit rows. *Done when:* acceptance 3–9, 13, 15 pass.
3. **Shop floor and time.** Board endpoint and screen with the action buttons, time entries, auto-refresh and stale-state banner, mobile/tablet layouts. *Done when:* acceptance 10–11 pass.
4. **Costing and events.** Cost snapshot on consumption, costing report with variance and export, the six events on the bus with a verified delivery, and the `sales.order.confirmed` consumer that offers work-order creation. *Done when:* acceptance 12, 14 pass and the QA report for the wave lists zero high findings on the new screens.

### Risks / notes

- Stock correctness is the highest risk: every consumption and output must be a ledger row in the same transaction as the work-order update, and repeated clicks must not double-post (idempotency key per action).
- The inventory port is the seam with REQ-053; the stub keeps the module buildable, but it must implement the identical contract or the swap turns into a bug hunt.
- Numeric quantities use `numeric(14,3)` consistently — mixing floats here would corrupt stock arithmetic; conversions between units arrive with inventory depth and are explicitly out of scope now.
- The shop-floor board is used standing up: hit targets, contrast, and stale-data honesty matter more than density; never show a cached number without its timestamp.
- Costing is deliberately "lite": no overhead absorption, no work-in-progress accounting; the report labels it as direct material + labour only so nobody reads it as financial truth.
- Turkish example copy is limited to seeded demo data (for example a BoM named "Kutu Montajı" and a work-order note), never in API strings or log messages.
