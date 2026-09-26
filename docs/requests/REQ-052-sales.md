> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/sales`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Quote-to-order flow on top of CRM and the catalog.

- **Product catalog** (shared with inventory) with price lists, currency, tax class, units.
- **Quotes**: line items with quantity/price/discount, totals with tax, validity dates, versions, PDF output.
- **Approval rules** (e.g. discount > 15% needs a manager) integrated with the approvals module.
- **Quote → Order → Invoice** handoff with status history and stock reservation.
- **Customer portal-lite**: public quote view (tokenized link) with accept/decline.
- **Reports**: won/lost, average deal size, quote-to-order conversion, sales by period/owner.
- **Events**: `sales.quote.sent`, `sales.quote.accepted`, `sales.order.confirmed`.

## Implementation spec

> **Module:** `modules/sales` (crate `omnion-module-sales`, workspace member) · **Migration:** `database/migrations/0012_sales.sql` (next free slot at build time) · **Admin routes:** `/sales/*` · **Public route:** `/q/{token}` on the site renderer · **Permission family:** `sales.*` · **Depends on:** core crates + `modules/crm` (REQ-051) for the customer link, `modules/inventory` (REQ-053) for reservation, `modules/accounting` (REQ-054) for invoice handoff, `modules/approvals` (REQ-059) for discount gates.

### Scope (in / out)

**In**

- Sellable catalog: products (SKU, name, description, unit, tax class snapshot, category), price lists with currency and per-list prices, active/archived lifecycle.
- Quotes: lines with quantity, unit price, discount %, per-line tax; totals (subtotal, discount, tax, grand total); validity dates; versions (immutable snapshots per send); PDF output; tokenized public view with accept/decline.
- Quote → Order → Invoice handoff: status history, stock reservation on order confirmation, an invoice *draft* handoff to accounting, cancellation path that releases reservation.
- Approval gate: a quote whose largest discount exceeds the organization threshold (default 15%) goes to `pending_approval` and is decided through REQ-059; the requester and the assigned manager both get notifications.
- Reports: won/lost by period, average deal size, quote-to-order conversion, sales by owner/period; CSV export.
- Events for quote lifecycle and order confirmation, feeding automations and webhooks.

**Out (tracked elsewhere)**

- Tax rate *definitions* and the invoice document itself → REQ-054. Stock ledger, warehouses and availability math → REQ-053. Payment capture → REQ-054 (payments against invoices).
- PDF rendering engine → REQ-029 (sales registers templates "quote" and "order"). E-mail delivery channel → REQ-021. CRM pipeline/kanban → REQ-051. Storefront checkout → REQ-008.

### Screens (UI)

Module nav: **Overview · Quotes · Orders · Catalog · Price lists · Reports · Settings**.

| Route | Screen |
|---|---|
| `/sales` | Overview: open quotes, expiring this week, pending approvals, conversion % (30 days), recent orders |
| `/sales/quotes` | Quote list (table) with status tabs (All / Draft / Sent / Accepted / Declined / Expired) |
| `/sales/quotes/new` | Quote builder (customer → lines → totals → validity) |
| `/sales/quotes/{id}` | Quote detail: builder (editable while draft/pending), version history, activity, public link, PDF, actions |
| `/sales/orders` | Order list with status filters |
| `/sales/orders/new`, `/sales/orders/{id}` | Create order from an accepted quote or manually / order detail with reservation + invoice state |
| `/sales/catalog`, `/sales/catalog/new`, `/sales/catalog/{id}` | Product list / create / detail (prices across lists, stock hint from inventory) |
| `/sales/pricelists`, `/sales/pricelists/{id}` | Price list list / editor with per-product price rows |
| `/sales/reports` | Report screen with period, owner and status filters, chart + table, CSV export |
| `/sales/settings` | Currency default, discount approval threshold, quote validity default, document numbering prefix |

**Quote list** — columns: `Number` (`Q-2026-0001`), `Customer` (CRM link), `Owner`, `Amount` (grand total + currency), `Status` (badge), `Valid until` (amber when ≤7 days, red when expired), `Version`, `Updated`. Filters: search (number/customer/title), status (multi), owner, date range, amount range, expiring-in-N-days. Bulk: assign owner, send, duplicate, export, archive draft. Row actions: open, duplicate, download PDF, copy public link (only when sent), cancel. Shortcuts: `n` new quote, `/` search, `enter` open, `d` duplicate, `shift+p` PDF, `?` help. States: skeleton table, empty state ("Create your first quote" + "From a CRM deal" button), error state with retry.

**Quote builder** (`/sales/quotes/new`, editable while `draft`/`pending_approval`) — Customer (combobox searching CRM companies/contacts, create-inline via CRM), Title, Currency (default from org), Valid until (date, must be today or later, default +30 days), Payment terms, Notes, Reference (customer PO). Lines grid: `#`, Product (combobox, prefilled price from the selected price list), Description, Qty (numeric, 3 decimals, > 0), Unit, Unit price (≥ 0), Discount % (0–100), Tax % (snapshot), Line total (computed, right-aligned). Line mechanics: add row (`alt+enter` from the last field), delete (`ctrl+backspace`), reorder (drag handle + `alt+↑/↓`), footer with subtotal/discount/tax/total; a discount above the threshold shows an inline amber banner "Discount over 15% needs manager approval" before submit. Submit paths: `Save draft`, `Request approval` (enabled only when over threshold), `Send` (requires a customer e-mail or explicit confirm to skip e-mail). Validation: customer required, ≥1 line, each line product or description required, qty > 0, discount ≤ 100, valid-until ≥ today; totals recomputed server-side and echoed back (client totals are display only).

**Quote detail** — header with number, status badge, actions (`Send`, `Request approval`, `Duplicate`, `PDF`, `Copy public link`, `New order`, `Cancel`), left: lines (read-only once sent) + totals; right: customer card (CRM link), owner, validity, version list (v1, v2 … each with sent-at, totals, and a *restore as new draft* action), timeline. Sent quotes are immutable: editing opens a new version with a confirmation dialog.

**Public quote view** (`/q/{token}`, no sign-in) — server-rendered on the site renderer: organization logo/name, quote number, customer name, line table, totals, validity, notes; buttons `Accept` and `Decline` (decline asks for an optional reason) plus `Download PDF`. A consumed or expired token shows a clear expired state. All amounts are formatted with the quote's currency; the page carries `noindex`.

**Orders** — list columns: `Number`, `Customer`, `Source quote`, `Amount`, `Reservation` (none/total/partial), `Invoice` (draft/issued/none), `Status`, `Created`. Detail: lines (read-only), reservations per warehouse from inventory, invoice link or `Create invoice draft` action, status history. Cancel releases reservations behind a confirm.

**Catalog & price lists** — product list columns: `SKU`, `Name`, `Category`, `Unit`, `Tax %`, `Default price`, `Currency`, `Active`; filters search/category/active/price range; bulk activate/archive/export. Product form: SKU (required, unique per organization, `^[A-Za-z0-9._-]{2,32}$`), Name (required ≤160), Description (rich-lite, ≤4000), Category, Unit (select: piece/hour/kg/m/day + custom), Tax % (0–100, snapshot default), Default price (≥ 0), Currency, Active. Price list editor: name, currency, active window, per-product rows (product, price, min qty) with inline edit; a product with no row falls back to its default price.

**Mobile behaviour:** lists become card rows (number + customer + amount + status), the builder stacks (customer → lines as cards → totals sticky footer), the public quote page is a single column. Board-less module, so no drag-only controls exist; every action is a button in an action sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/sales/products` | Catalog list / create | `sales.products.read` / `.manage` |
| GET/PATCH/DELETE | `/api/v1/sales/products/{id}` | Product detail / update / archive | `sales.products.read` / `.manage` |
| GET/POST | `/api/v1/sales/pricelists` | Price lists list / create | `sales.pricelists.read` / `.manage` |
| PUT | `/api/v1/sales/pricelists/{id}/items` | Replace price rows | `sales.pricelists.manage` |
| GET/POST | `/api/v1/sales/quotes` | Quote list / create (draft) | `sales.quotes.read` / `.create` |
| GET/PATCH | `/api/v1/sales/quotes/{id}` | Quote detail / update while draft | `sales.quotes.read` / `.update` |
| POST | `/api/v1/sales/quotes/{id}/send` | Snapshot a version, mark sent, e-mail the link | `sales.quotes.send` |
| POST | `/api/v1/sales/quotes/{id}/request-approval` | Start the REQ-059 approval chain | `sales.quotes.send` |
| POST | `/api/v1/sales/quotes/{id}/duplicate` | Duplicate as a new draft | `sales.quotes.create` |
| POST | `/api/v1/sales/quotes/{id}/cancel` | Cancel with reason | `sales.quotes.update` |
| GET | `/api/v1/sales/quotes/{id}/pdf` | PDF (queued through REQ-029) | `sales.quotes.read` |
| GET | `/api/v1/sales/quotes/{id}/link` | Public token URL (regenerates on demand) | `sales.quotes.send` |
| POST | `/api/v1/sales/public/quotes/{token}/accept`, `/decline` | Public accept / decline (no session, token-scoped, rate-limited) | — |
| GET | `/api/v1/sales/public/quotes/{token}` | Public quote payload for the renderer | — |
| GET/POST | `/api/v1/sales/orders` | Order list / create (from `quote_id` or manual) | `sales.orders.read` / `.create` |
| GET | `/api/v1/sales/orders/{id}` | Order detail with reservation state | `sales.orders.read` |
| POST | `/api/v1/sales/orders/{id}/confirm` | Confirm: reserve stock, notify warehouse | `sales.orders.confirm` |
| POST | `/api/v1/sales/orders/{id}/cancel` | Cancel and release reservations | `sales.orders.confirm` |
| POST | `/api/v1/sales/orders/{id}/invoice-draft` | Create a draft invoice (REQ-054) | `sales.orders.confirm` |
| GET | `/api/v1/sales/reports/summary` | Won/lost, conversion, average size, by owner/period | `sales.reports.read` |
| GET | `/api/v1/sales/reports/export` | CSV of the same filter set | `sales.reports.read` |

Numbers are assigned server-side from a per-organization sequence and are immutable once sent. Totals are always recomputed in SQL from the lines.

### Data model

```text
sales_products(id uuid pk, organization_id uuid not null, sku text not null, name text not null,
  description text not null default '', category text, unit text not null default 'piece',
  tax_percent numeric(5,2) not null default 0, default_price numeric(14,2) not null default 0,
  currency char(3) not null default 'USD', active boolean not null default true, archived_at timestamptz)
sales_pricelists(id uuid pk, organization_id uuid not null, name text not null, currency char(3) not null,
  valid_from date, valid_to date, active boolean not null default true)
sales_pricelist_items(id uuid pk, pricelist_id uuid not null references sales_pricelists(id) on delete cascade,
  product_id uuid not null references sales_products(id) on delete cascade, price numeric(14,2) not null,
  min_qty numeric(14,3) not null default 1)
sales_quotes(id uuid pk, organization_id uuid not null, number text not null, title text not null default '',
  company_id uuid, contact_id uuid, owner_user_id uuid, quote_status text not null default 'draft',
  currency char(3) not null, valid_until date, payment_terms text, reference text,
  subtotal numeric(14,2) not null default 0, discount_total numeric(14,2) not null default 0,
  tax_total numeric(14,2) not null default 0, grand_total numeric(14,2) not null default 0,
  max_discount_percent numeric(5,2) not null default 0, current_version integer not null default 0,
  approval_request_id uuid, sent_at timestamptz, decided_at timestamptz, lost_reason text,
  public_token_hash text, public_token_expires_at timestamptz, cancelled_at timestamptz)
sales_quote_lines(id uuid pk, quote_id uuid not null references sales_quotes(id) on delete cascade,
  position integer not null, product_id uuid, description text not null, qty numeric(14,3) not null,
  unit text not null, unit_price numeric(14,2) not null, discount_percent numeric(5,2) not null default 0,
  tax_percent numeric(5,2) not null default 0, line_total numeric(14,2) not null)
sales_quote_versions(id uuid pk, quote_id uuid not null, version integer not null, snapshot jsonb not null,
  totals jsonb not null, created_by uuid, created_at timestamptz not null default now())
sales_orders(id uuid pk, organization_id uuid not null, number text not null, quote_id uuid, company_id uuid,
  contact_id uuid, owner_user_id uuid, order_status text not null default 'draft', currency char(3) not null,
  subtotal numeric(14,2) not null default 0, tax_total numeric(14,2) not null default 0,
  grand_total numeric(14,2) not null default 0, reservation_state text not null default 'none',
  invoice_id uuid, confirmed_at timestamptz, cancelled_at timestamptz, cancel_reason text)
sales_order_lines(id uuid pk, order_id uuid not null references sales_orders(id) on delete cascade,
  position integer not null, product_id uuid, description text not null, qty numeric(14,3) not null,
  unit_price numeric(14,2) not null, tax_percent numeric(5,2) not null default 0, line_total numeric(14,2) not null)
sales_status_history(id uuid pk, organization_id uuid not null, document_kind text not null, document_id uuid not null,
  from_status text, to_status text not null, note text, actor_user_id uuid, created_at timestamptz not null default now())
```

Checks: `quote_status in ('draft','pending_approval','approved','sent','accepted','declined','expired','cancelled','converted')`; `order_status in ('draft','confirmed','partially_invoiced','invoiced','cancelled')`; `qty > 0`; `discount_percent between 0 and 100`; `tax_percent between 0 and 100`; `unit_price >= 0`; `valid_to >= valid_from`; unique `(organization_id, number)`; unique `(organization_id, lower(sku))` where `archived_at is null`.

Indexes: `sales_quotes_org_status_idx (organization_id, quote_status, valid_until)`, `sales_quotes_org_updated_idx (organization_id, updated_at desc)`, `sales_orders_org_status_idx (organization_id, order_status, created_at desc)`, `sales_quote_lines_quote_idx (quote_id, position)`, `sales_status_history_doc_idx (document_kind, document_id, created_at desc)`, partial index on `sales_quotes (public_token_hash) where public_token_hash is not null`.

Migration: `database/migrations/0012_sales.sql`, additive; seeds no rows. The public token is stored as a SHA-256 hash — the URL carries the random token, the database never stores it in clear.

### Events

Emitted: `sales.quote.created`, `sales.quote.sent`, `sales.quote.accepted`, `sales.quote.declined`, `sales.quote.expired`, `sales.order.created`, `sales.order.confirmed`, `sales.order.cancelled`. Payloads carry ids, currency, totals, owner and — for `sales.order.confirmed` — the reserved line quantities. Consumed: `approvals.request.decided` (REQ-059) moves a quote from `pending_approval` to `approved` (or back to `draft` with the reason), `crm.deal.stage_changed` to `won` can pre-fill a quote from the deal, `inventory.stock.low` (REQ-053) warns in the order detail when a reserved product is under threshold, `accounting.payment.recorded` (REQ-054) marks a converted order paid in the report.

Webhook relevance: all eight names are subscribable; `sales.quote.accepted` and `sales.order.confirmed` are the integration hooks a partner system would listen on, so both are stable and versioned with the payload documented in the API reference.

### Acceptance criteria

- [ ] Migration `0012_sales.sql` applies cleanly on a populated database; `cargo test -p omnion-module-sales` is green.
- [ ] Every `/api/v1/sales/*` route is permission-guarded (401 / 403 / 200 verified per key); cross-organization ids answer 404.
- [ ] Quote, order, product, price-list and approval mutations write audit entries with before/after diffs.
- [ ] Totals are computed in SQL: changing a line's qty/price/discount/tax updates subtotal, discount, tax and grand total on the persisted row (a hand-computed test fixture matches to the cent).
- [ ] Quote numbering is per-organization, gap-free under concurrent creates, and immutable after send.
- [ ] `Send` snapshots an immutable version with its totals; editing after send creates a new version and the older ones stay readable and restorable.
- [ ] A 20% line discount blocks `Send`, offers `Request approval`, and creates a REQ-059 request routed to a manager; approval (or rejection with a comment) is reflected on the quote with the reason shown.
- [ ] The public quote page renders without a session, accepts once, declines with a reason, shows an expired/consumed state afterwards, and never leaks internal notes or other quotes.
- [ ] PDF export of a quote and an order downloads a real document with the lines, totals and organization header (verified by opening it).
- [ ] Quote → order conversion copies lines and totals, links both documents, and writes status history rows on both sides.
- [ ] Confirming an order reserves stock in inventory; the reservation is visible in the order detail and releasing it on cancel returns the stock (a second confirm of the same order is a no-op).
- [ ] Order → invoice draft hands off to accounting and returns a link; the invoice number and the order totals agree.
- [ ] Expiry: a quote past `valid_until` flips to `expired` on the next read sweep and shows the expired badge in the list and public page.
- [ ] Reports return won/lost counts, conversion %, average deal size and per-owner breakdown for the filter set; CSV export contains the same rows as the table.
- [ ] Price lists: selecting a list in the builder fills unit prices, and a missing row falls back to the product default price.
- [ ] Global search finds quotes and orders by number and customer; ⌘K offers "New quote" gated by `sales.quotes.create`.
- [ ] Empty, loading and error states exist on every screen; no dead buttons, no "coming soon" placeholders.
- [ ] Mobile 390×844: list, builder and public page are usable; totals footer stays visible while scrolling lines.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/sales`, `/sales/quotes`, `/sales/quotes/new`, `/sales/orders`, `/sales/catalog`, `/sales/pricelists`, `/sales/reports`, `/sales/settings` (desktop) plus `/sales/quotes` (mobile) and the public quote page on the site host. The walkthrough must: create a product → create a price-list row → create a CRM company (reuse the CRM sample) → build a quote with three lines → set one line to 20% discount → hit `Request approval` and see the banner → send the quote → open the public link on the site host, accept it → convert to an order → confirm (reservation visible) → open the invoice draft → download the PDF. It clicks every button/select/input on each screen, including the line-grid add/remove/reorder controls.

Visual check: the builder shows a three-line grid with a right-aligned totals block and the amber approval banner; the quote list shows status badges and the amber/red validity hints; the public page shows a clean single-column document with logo, totals and two buttons. Screenshots: `page-sales-quotes`, `page-sales-quote-builder`, `page-sales-orders`, `web-public-quote`, `mobile-sales-quotes`. Zero high findings, no horizontal overflow in the line grid at 1440 px, AA contrast on badges.

### Slices

1. **Catalog + price lists (data, API, screens).** Migration, products and price-list CRUD, permission keys, audit, tests. Done when a product and a price-list row are created through the panel and the price appears prefilled in a scratch quote-line request.
2. **Quotes end to end.** Builder, totals in SQL, versioning, send, PDF, public token page with accept/decline, expiry sweep. Done when QA builds a quote, sends it, accepts it through the public link and the PDF opens with correct totals.
3. **Approval gate + notifications.** Threshold check, REQ-059 request creation, approve/reject handling, notify requester and manager. Done when a 20% discount quote cannot be sent before approval and can be after it, with both sides seeing the decision.
4. **Orders, reservation, invoice handoff, reports.** Quote → order → confirm (reserve) → invoice draft, cancel/release, status history, report screen + CSV. Done when the full chain runs in one walkthrough and `sales.order.confirmed` reaches a subscribed webhook.

### Risks / notes

- **Tax rates live in accounting (REQ-054).** Lines store a `tax_percent` snapshot and an optional `tax_rate_id` without a FK, so sales can ship before accounting and historical documents never change when a rate is edited. When accounting is installed, the settings screen links the default rate.
- **Inventory coupling:** reservation calls the inventory service module when installed and degrades to `reservation_state = 'none'` with a visible note when it is not — never a silent failure.
- **Public tokens are credentials-shaped:** store only the hash, expire on send-date + validity, rate-limit by IP, and never expose the customer's other documents. Token regeneration invalidates the previous link.
- **Money rounding:** compute line totals in `numeric`, round half-up once per line, and sum the rounded values so the printed PDF, the panel and the invoice agree.
- **Immutability:** any code path that allows editing a sent quote must go through versioning; a PATCH on a sent quote returns 409 with a hint to duplicate.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
