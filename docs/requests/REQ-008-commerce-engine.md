# REQ-008 — Commerce Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/ecommerce`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Commerce features to move closer to the Odoo-side of the market:

- Products
- Categories
- Inventory
- Orders
- Customers
- Coupons
- Taxes
- Shipping
- Payment providers
- Invoices
- Subscriptions
- Digital products

**Not embedded in the Core** — built as a module:

```text
modules/
├── ecommerce/
├── crm/
├── inventory/
├── accounting/
├── subscriptions/
└── support/
```

## Notes

- Part of the broader business suite — [`docs/08-BUSINESS-SUITE.md`](../08-BUSINESS-SUITE.md)
  (CRM, Sales, Inventory, Accounting, HR, Projects, Helpdesk, Marketing, Documents, Knowledge,
  Approvals).

## Implementation spec

> **Module:** `modules/ecommerce` (crate `omnion-module-ecommerce`, workspace member) · **Migration:** `database/migrations/0013_commerce.sql` (next free slot at build time) · **Admin routes:** `/commerce/*` · **Public routes:** `/api/v1/public/store/*` · **Permission family:** `commerce.*` · **Depends on:** core crates (`identity`, `permissions`, `audit`, `media`, `events`, `workflows`) · **Bridges:** `modules/inventory` (REQ-053) owns stock when installed, otherwise this module keeps its own ledger; invoices render through REQ-029; payments arrive as signed provider webhooks; CRM (REQ-051) links orders to contacts.

### Scope (in / out)

**In**

- **Catalogue:** products (physical / digital / service) with categories, variants (option → value combinations), media gallery from the file manager (REQ-010), SEO fields, per-product tax class and shipping profile, draft/active/archived status, duplicate.
- **Inventory:** per-location stock with an append-only movement ledger, manual adjustments with a reason, reservations on order confirmation, low-stock thresholds and a low-stock panel.
- **Orders:** manual and storefront orders, line editing with per-line discounts, totals computed by one server-side function (subtotal → discounts → shipping → tax → total), status flow `pending → paid → fulfilled → completed` plus `cancelled`/`refunded`, shipments with carrier + tracking, a per-order timeline, internal notes, and partial or full refunds with a restock toggle.
- **Customers:** records created from checkout, manually or from a CRM contact, with addresses, tags, notes, order history and a lifetime-value rollup; guest checkout keeps an e-mail-only customer row.
- **Money:** coupons (percent / fixed / free shipping, usage limits, windows, product or category scope, per-customer limits, stacking rules), tax classes + rates (country/region, priority, compound, inclusive display), shipping zones + methods (flat / free-over / weight / price tier), payment providers (generic redirect + webhook, test and live mode, secrets by reference), invoices (number sequence, PDF via REQ-029, send, mark paid, void, credit note).
- **Subscriptions and digital delivery:** plans (interval, price, trial, features) and subscriptions (`trialing`/`active`/`past_due`/`cancelled`, renewal attempts with dunning, pause/resume/cancel, MRR rollup); digital products carry a file from the file manager, expiring download links, per-order limits and optional license keys with activation counters.
- **Storefront surface and settings:** product listing, product detail, cart, checkout and order-status endpoints under `/api/v1/public/store/*` (tenant-scoped by site, presentation owned by the theme), plus currency, tax-inclusive display, order number format, guest checkout toggle, default low-stock threshold, return window and invoice footer text.

**Out (tracked elsewhere)**

- Warehouse transfers, receipts, multi-warehouse documents and barcode workflows → REQ-053; general ledger and reconciliation → REQ-054; quotes and price lists → REQ-052.
- E-mail/abandoned-cart campaigns → REQ-060 (this module emits events only); carrier rate APIs → REQ-015; e-invoice localizations beyond country/region rates → REQ-054.

### Screens (UI)

Nav: **Overview · Products · Categories · Inventory · Orders · Customers · Coupons · Taxes · Shipping · Payments · Invoices · Subscriptions · Settings**.

| Route | Screen |
|---|---|
| `/commerce` | Overview: orders today/30d, revenue, average order value, low-stock list, unpaid invoices, expiring subscriptions |
| `/commerce/products` · `/new` · `/{id}` | Product list / create / detail (tabs Overview, Variants, Inventory, Media, SEO, History) |
| `/commerce/categories` | Category tree with drag reorder |
| `/commerce/inventory` | Stock levels, movement ledger, adjustments |
| `/commerce/orders` · `/new` · `/{id}` | Order list / manual order / detail |
| `/commerce/customers` · `/{id}` | Customer list / detail (Overview, Orders, Subscriptions, Notes, Audit) |
| `/commerce/coupons` · `/new` | Coupon list / editor |
| `/commerce/taxes` · `/commerce/shipping` | Tax classes and rates / zones and methods |
| `/commerce/payments` | Provider connections, modes, webhook endpoints |
| `/commerce/invoices` · `/{id}` | Invoice list / detail |
| `/commerce/subscriptions` · `/plans` | Subscription list / plan editor |
| `/commerce/settings` | Store settings |

**Product list** — columns `Product` (thumbnail + name), `SKU`, `Type`, `Price`, `Stock` (value plus `Low`/`Out` badge), `Status`, `Categories`, `Updated`; filters: search (name/SKU), category, status, type, stock state, price range, tag; bulk actions: publish, archive, delete, assign category, adjust price by percent, export CSV; sort by any column and save the view per user.

**Product form** — General (name required ≤160, slug auto with `^[a-z0-9-]{2,80}$`, description, short description, status, tags ≤10), Pricing (price ≥0, compare-at ≥ price or empty, cost, tax class), Inventory (SKU unique per organization, track-stock toggle, opening quantity, low-stock threshold, backorder toggle), Shipping (weight in grams, dimensions, profile), Variants (option name + values with a generated combination table), Digital (file picker, download limit 1–100, link lifetime 1–720 h, license keys one per line), SEO (title ≤60, description ≤160, canonical). Messages render under the field and focus moves to the first invalid input.

**Inventory** — levels table `Product`, `Variant`, `Location`, `On hand`, `Reserved`, `Available`, `Reorder point`, `Status`; the ledger below shows `Date`, `Product`, `Change`, `Reason`, `Reference`, `User` with a date filter; the adjustment dialog (set / add / remove + reason + note) confirms any decrease larger than 10% of stock.

**Orders** — list columns `Order`, `Date`, `Customer`, `Items`, `Total`, `Payment`, `Fulfilment`, `Channel`; filters on every status plus date, customer, channel and coupon; bulk actions mark fulfilled (with shipment details), print packing slips, export, cancel. Detail: header with status selectors and a timeline, line editor (product search, quantity, unit price, discount %), totals panel (editable shipping and order discount), payments panel (capture, refund), shipments panel (carrier, tracking, mark delivered), internal notes, and an invoice block with `Create invoice`.

**Customers, coupons, taxes, shipping** — customers: `Customer`, `E-mail`, `Orders`, `Lifetime value`, `Last order`, `Tags`, `Status`, with a merge action. Coupons: `Code` (`^[A-Z0-9-]{3,32}$`, unique, uppercase), type, value (percent 1–90 or fixed ≥0), minimum order, usage limits total and per customer, window, scope, stacking policy, active toggle — an end before a start is refused. Tax rates: `Class`, `Country`, `Region`, `Rate %`, `Priority`, `Compound`, `Active` (seeded example row `TR` / 20% / class `standard`). Shipping: a zone country/region picker plus method rows `Name`, `Kind`, `Rate`, `Free over`, `Estimate`, `Carrier`.

**Payments, invoices, subscriptions** — provider cards show `Name`, `Mode` (test/live), `Status`, `Last event`, a copyable webhook URL and `Test connection`; the connection drawer stores key material by reference and never echoes it back (a stored value renders `••••` with `Replace`), and the signing secret is shown once at creation. Invoices: `Invoice`, `Order`, `Customer`, `Issued`, `Due`, `Status`, `Total`, `Tax` with send, mark paid, void (reason required) and PDF. Subscriptions: `Customer`, `Plan`, `Status`, `Amount`, `Renews on`, `MRR` with pause/resume/cancel (cancel-now names the amount at risk); plans carry interval count and unit, price, trial days, features and an active toggle.

**States, keyboard, mobile** — skeletons, real empty states with the primary action, error states with retry; `/` focuses search, `n` creates in the current section, `j`/`k` move row focus, `enter` opens, `⌘+enter` saves, `Esc` closes the drawer; on mobile tables become cards (name, amount, status first), the order detail stacks panels with a sticky totals bar, and numeric inputs use the numeric keyboard.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/commerce/products` | Product list (filters, saved view) / create | `commerce.products.read` / `commerce.products.manage` |
| GET, PATCH, DELETE | `/api/v1/commerce/products/{id}` | Detail with variants and media / update / archive | `commerce.products.read` / `commerce.products.manage` |
| POST, PUT | `/api/v1/commerce/products/{id}/duplicate` · `/variants` | Duplicate with variants / replace the variant matrix | `commerce.products.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/commerce/categories` · `/{id}` · `/reorder` | Category tree, create, update, delete, reorder/re-parent | `commerce.products.read` / `commerce.products.manage` |
| GET, POST | `/api/v1/commerce/inventory` · `/inventory/movements` · `/inventory/adjust` | Stock levels, ledger, manual adjustment with reason | `commerce.inventory.read` / `commerce.inventory.manage` |
| GET, POST | `/api/v1/commerce/orders` | Order list / manual order | `commerce.orders.read` / `commerce.orders.manage` |
| GET, PATCH | `/api/v1/commerce/orders/{id}` | Detail (lines, payments, shipments, timeline) / update | `commerce.orders.read` / `commerce.orders.manage` |
| POST | `/api/v1/commerce/orders/{id}/fulfil` · `/cancel` · `/notes` · `/refunds` | Shipment, cancellation, note, refund (partial/full, restock optional) | `commerce.orders.manage` / `commerce.orders.refund` |
| GET, POST, PATCH | `/api/v1/commerce/customers` · `/{id}` · `/merge` | Customer list, create, update, merge duplicates | `commerce.customers.read` / `commerce.customers.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/commerce/coupons` · `/{id}` | Coupon list and CRUD | `commerce.products.read` / `commerce.discounts.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/commerce/tax-rates` · `/{id}` | Tax classes and rate CRUD | `commerce.products.read` / `commerce.taxes.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/commerce/shipping-zones` · `/{id}` | Zone and method CRUD | `commerce.products.read` / `commerce.shipping.manage` |
| GET, POST, PATCH | `/api/v1/commerce/payment-providers` · `/{id}` | Connections (no secrets returned), connect, mode switch, test | `commerce.payments.manage` |
| POST | `/api/v1/commerce/webhooks/payments/{provider}` | Provider callback, signature verified | public, signature required |
| GET, POST | `/api/v1/commerce/invoices` · `/{id}/pdf` · `/{id}/send` · `/mark-paid` · `/void` | Invoice list, PDF (REQ-029), lifecycle actions | `commerce.orders.read` / `commerce.invoices.manage` |
| GET, POST, PATCH | `/api/v1/commerce/plans` · `/{id}` · `/subscriptions` | Plans and subscription list with MRR | `commerce.subscriptions.manage` |
| POST | `/api/v1/commerce/subscriptions/{id}/pause` · `/resume` · `/cancel` | Lifecycle changes | `commerce.subscriptions.manage` |
| GET, PUT | `/api/v1/commerce/settings` | Store settings | `commerce.settings.manage` |
| GET | `/api/v1/public/store/products` · `/products/{slug}` | Storefront catalogue | public, site-scoped |
| POST, GET | `/api/v1/public/store/cart` · `/checkout` · `/orders/{token}` · `/downloads/{token}` | Cart, checkout, order status, expiring digital delivery | public, rate limited / public token |

### Data model

```text
commerce_products(id uuid pk, organization_id uuid not null, site_id uuid, name text not null, slug text not null, kind text not null default 'physical', status text not null default 'draft', description text, short_description text, price numeric(14,2) not null, compare_at_price numeric(14,2), cost numeric(14,2), currency char(3) not null, sku text, track_stock boolean not null default true, low_stock_threshold integer not null default 0, backorder boolean not null default false, weight_grams integer, tax_class_id uuid, shipping_profile text, seo jsonb not null default '{}', tags text[] not null default '{}', digital_media_id uuid, download_limit integer, download_ttl_hours integer, created_by uuid, created_at, updated_at, archived_at)
commerce_categories(id uuid pk, organization_id uuid not null, parent_id uuid, name text not null, slug text not null, position integer, created_at, updated_at) · commerce_variants(id uuid pk, product_id uuid not null, sku text, price numeric(14,2), option_values jsonb not null default '{}', position integer)
commerce_inventory(id uuid pk, variant_id uuid not null, location text not null default 'MAIN', on_hand integer not null default 0, reserved integer not null default 0, reorder_point integer not null default 0, unique (variant_id, location))
commerce_stock_movements(id bigint identity pk, variant_id uuid not null, location text not null, change integer not null, reason text not null, reference text, note text, on_hand_after integer not null, actor_user_id uuid, created_at)   -- append-only ledger
commerce_customers(id uuid pk, organization_id uuid not null, email text, name text, phone text, tags text[] not null default '{}', notes text, user_id uuid, crm_contact_id uuid, created_at, updated_at) · commerce_addresses(id uuid pk, customer_id uuid not null, kind text not null, line1 text not null, line2 text, city text not null, region text, postal_code text, country char(2) not null, phone text)
commerce_orders(id uuid pk, organization_id uuid not null, site_id uuid, number text not null, customer_id uuid not null, status text not null default 'pending', payment_status text not null default 'unpaid', fulfilment_status text not null default 'unfulfilled', currency char(3) not null, subtotal numeric(14,2) not null default 0, discount_total numeric(14,2) not null default 0, shipping_total numeric(14,2) not null default 0, tax_total numeric(14,2) not null default 0, total numeric(14,2) not null default 0, coupon_code text, channel text not null default 'admin', billing_address jsonb, shipping_address jsonb, placed_at timestamptz, updated_at)
commerce_order_lines(id uuid pk, order_id uuid not null, product_id uuid, variant_id uuid, name text not null, sku text, quantity integer not null, unit_price numeric(14,2) not null, discount_percent numeric(5,2) not null default 0, tax_rate numeric(5,2) not null default 0, line_total numeric(14,2) not null) · commerce_order_events(id bigint identity pk, order_id uuid not null, kind text not null, actor_user_id uuid, detail jsonb, created_at)
commerce_payments(id uuid pk, order_id uuid not null, provider_id uuid, kind text not null, status text not null, amount numeric(14,2) not null, currency char(3) not null, provider_reference text, captured_at, failure_reason text) · commerce_refunds(id uuid pk, order_id uuid not null, payment_id uuid, amount numeric(14,2) not null, reason text not null, restocked boolean, actor_user_id uuid, created_at) · commerce_shipments(id uuid pk, order_id uuid not null, carrier text, tracking_number text, status text not null default 'pending', shipped_at, delivered_at)
commerce_coupons(id uuid pk, organization_id uuid not null, code text not null, kind text not null, value numeric(14,2) not null default 0, min_order numeric(14,2) not null default 0, usage_limit integer, per_customer_limit integer, starts_at timestamptz, ends_at timestamptz, scope jsonb not null default '{}', stackable boolean not null default false, active boolean not null default true, created_at) · commerce_coupon_redemptions(coupon_id uuid not null, order_id uuid not null, customer_id uuid, amount numeric(14,2) not null, created_at, pk (coupon_id, order_id))
commerce_tax_classes(id uuid pk, organization_id uuid not null, name text not null, is_default boolean not null default false) · commerce_tax_rates(id uuid pk, tax_class_id uuid not null, country char(2) not null, region text, rate numeric(5,2) not null, priority integer not null default 0, compound boolean not null default false, active boolean not null default true)
commerce_shipping_zones(id uuid pk, organization_id uuid not null, name text not null, countries text[] not null default '{}', regions text[] not null default '{}') · commerce_shipping_methods(id uuid pk, zone_id uuid not null, name text not null, kind text not null, rate numeric(14,2) not null default 0, free_over numeric(14,2), estimate text, active boolean not null default true)
commerce_payment_providers(id uuid pk, organization_id uuid not null, name text not null, adapter text not null, mode text not null default 'test', enabled boolean not null default false, config jsonb not null default '{}', secret_ref text not null default '', webhook_prefix text not null, created_at, updated_at)
commerce_invoices(id uuid pk, order_id uuid not null, number text not null, status text not null default 'draft', issued_on date, due_on date, total numeric(14,2) not null, tax_total numeric(14,2) not null, currency char(3) not null, pdf_media_id uuid, sent_at, paid_at, voided_at, void_reason text)
commerce_plans(id uuid pk, organization_id uuid not null, name text not null, interval_count integer not null default 1, interval_unit text not null default 'month', price numeric(14,2) not null, currency char(3) not null, trial_days integer not null default 0, features text, active boolean not null default true) · commerce_subscriptions(id uuid pk, customer_id uuid not null, plan_id uuid not null, status text not null default 'trialing', current_period_start timestamptz not null, current_period_end timestamptz not null, renew_attempts integer not null default 0, cancel_at_period_end boolean not null default false, cancelled_at, created_at)
commerce_license_keys(id uuid pk, order_line_id uuid not null, key_ciphertext text not null, activations integer not null default 0, max_activations integer not null default 1, created_at)
```

Checks: `price >= 0`, `compare_at_price >= price`, `quantity > 0`, `discount_percent between 0 and 100`, `rate between 0 and 100`, `ends_at > starts_at`, `kind in ('physical','digital','service')`. Indexes: unique `(organization_id, lower(slug))` and `(organization_id, sku)` on products, unique `(organization_id, code)` on coupons, unique `(organization_id, number)` on orders, `commerce_orders_org_placed_idx (organization_id, placed_at desc)`, `commerce_stock_movements_variant_idx (variant_id, created_at desc)`, `commerce_subscriptions_renew_idx (current_period_end) where status in ('active','past_due')`.

Migration `database/migrations/0013_commerce.sql` — append-only and commented in the `0009` style; seeds a `standard` tax class, a `MAIN` location, a default shipping zone and the order-number sequence per existing organization.

### Events

- Emitted: `order.created`, `order.paid`, `order.fulfilled`, `order.cancelled`, `order.refunded`, `payment.failed`, `invoice.issued`, `invoice.paid`, `invoice.overdue`, `product.created`, `product.updated`, `inventory.low_stock`, `inventory.adjusted`, `coupon.redeemed`, `customer.created`, `subscription.started`, `subscription.renewed`, `subscription.past_due`, `subscription.cancelled`, `digital.delivered`.
- Payloads carry ids, amounts and the changed field list — never a payment credential and never an address beyond the customer id. Consumed: `approval.decided` (REQ-059) releases refunds above a configured threshold; a scheduled worker raises `invoice.overdue` from due dates. Webhook relevance: `order.paid` and `order.fulfilled` drive fulfilment and accounting integrations; `inventory.low_stock` feeds notifications (REQ-021); `subscription.past_due` starts a dunning workflow (REQ-003).

### Acceptance criteria

- [ ] `0013_commerce.sql` applies on a fresh and on a populated database; `cargo test -p omnion-module-ecommerce` is green.
- [ ] Product validation refuses a duplicate slug/SKU and a negative price with field-level messages; variants regenerate deterministically from their options.
- [ ] Category drag reorder persists order and parent and refuses a cycle.
- [ ] A manual adjustment writes a ledger row whose `on_hand_after` matches the level row; decreases over 10% ask for confirmation.
- [ ] Totals come from one server-side function and recompute identically on every mutation (fixture: three lines, one line discount, one coupon).
- [ ] Admin, storefront and line editing share the same validation (zero quantity, negative price and discount over 100% are all refused).
- [ ] Confirming an order reserves stock, cancelling restocks only when asked, and a refund with `restock` writes a compensating movement.
- [ ] The status flow is enforced: no `completed` without a shipment, no `paid` without a payment record, `cancelled` only before fulfilment.
- [ ] Coupons refuse a percent above 90, an end before a start, a duplicate code and a per-customer breach, naming the reason, and stacking rules hold in the totals function.
- [ ] A Turkish fixture rate (`TR`, 20%, class `standard`) taxes a fixture order correctly, and inclusive display changes presentation only.
- [ ] Shipping: the free-over threshold applies, a zone mismatch is refused, and the chosen method is stored on the order.
- [ ] A signed payment webhook marks the order paid exactly once, an unsigned or replayed event is rejected, and `payment.failed` fires on failure.
- [ ] A paid digital order yields an expiring link that respects the download limit and is refused after expiry.
- [ ] A subscription renews at period end, a failed renewal becomes `past_due` with an attempt count, and pause/resume/cancel show up in MRR.
- [ ] Invoice numbers are gap-free per organization, the PDF renders through REQ-029, `send` records the delivery event, and `void` requires a reason.
- [ ] Every mutation writes an audit entry with actor, before/after and request id, and all routes answer 401/403/200 as documented.
- [ ] All thirteen screens have empty, loading and error states with zero high findings; mobile orders render as cards with the sticky totals bar usable.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/commerce`, `/commerce/products`, `/commerce/categories`, `/commerce/inventory`, `/commerce/orders`, `/commerce/customers`, `/commerce/coupons`, `/commerce/taxes`, `/commerce/shipping`, `/commerce/payments`, `/commerce/invoices`, `/commerce/subscriptions` (desktop) and `/commerce/products`, `/commerce/orders` (mobile). The script must create a category, a product with two variants and stock, adjust stock once with a valid reason, create a coupon and a customer, place a manual order with two lines plus the coupon, mark it paid, fulfil it with a tracking number, refund one line with restock, open the invoice and download the PDF, then create a plan and check the subscription list empty state.

What the visual check should see: a product table with thumbnails, correctly formatted prices and readable status badges; a totals panel whose columns line up with a visually distinct total; a coupon editor showing validation after an invalid submit; `Low`/`Out` badges on stock; a provider card rendering stored secrets as `••••` and never a value; no clipped amounts or overflowing tables — screenshots `page-commerce-products`, `page-commerce-product-detail`, `page-commerce-orders`, `page-commerce-order-detail`, `page-commerce-inventory`, `mobile-commerce-orders`.

### Slices

1. **Catalogue + categories + media.** Migration, product/variant/category CRUD, media link, SEO fields, statuses, list and form screens with validation, permission keys, tests. Done when a two-variant product with stock round-trips through the API and both populated and empty product screens render in QA.
2. **Inventory + orders + customers.** Stock ledger with adjustments and reservations, the order engine with its single totals function, manual orders, line editing, fulfil/cancel, customers and merge, timeline events. Done when fixture totals match to the cent, reservation and restock prove out, and the order screens pass QA.
3. **Money: coupons, taxes, shipping, payments, invoices.** Coupon rules, tax classes/rates, zones/methods, provider connections with signed webhooks and test mode, invoice lifecycle with PDF via REQ-029 and the number sequence. Done when a signed webhook marks an order paid exactly once, a coupon + tax fixture computes correctly, and an invoice is issued, sent and voided.
4. **Subscriptions + digital delivery + storefront.** Plans, subscription lifecycle with the renewal worker and dunning, digital files with expiring links and license keys, public store endpoints. Done when a subscription renews at period end, a paid digital order delivers a working expiring link, and a storefront checkout creates the same order shape as a manual one.

### Risks / notes

- **Migration number** is “next free slot at build time”; the file must stay additive — commerce is the largest module schema so far.
- **One totals function.** Discounts, coupons, shipping, inclusive tax and rounding live in exactly one place used by admin, storefront and the invoice PDF; a second implementation will drift.
- **Inventory ownership:** with `modules/inventory` (REQ-053) installed it owns stock and this module calls it, the local ledger being the fallback — the seam is one trait, decided in slice 2, never in the UI.
- **Rounding and currency:** store currency is fixed per site, rounding happens at line level with the remainder assigned deterministically, and invoices must never disagree with the order total.
- **Credentials by reference only** (`secret_ref`), never echoed back after saving, with replay protection by provider event id; the platform stores references, never payment instrument data.
- **Restock on refund** writes a compensating ledger row — the ledger is append-only and is never edited.
- **Public store endpoints** are a public write surface: body limits, quotas, tenant scoping by site, and no catalogue leakage across organizations by slug.
