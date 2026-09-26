# REQ-118 — Storefront & Checkout

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** modules/ecommerce + apps/web
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Selling from the public site.

- Product storefront: catalog pages, categories, filters, search, product detail with variants and gallery.
- Cart (persistent + guest cart merge), checkout with address book, shipping options, tax display.
- Payment provider adapter (starting with Stripe-style hosted flow) and order confirmation e-mail.
- Account area: orders, invoices (PDF), addresses, wishlist.
- Inventory and pricing wired to REQ-053 (stock) and REQ-052 (pricing); promotions/discount codes.

## Implementation spec

> **Module:** `modules/ecommerce` (storefront slice of the crate) · **Renderer:** `apps/web` routes rendered through the active theme · **Migrations:** `0122_storefront.sql`, `0123_storefront_accounts.sql` (reserved band 0116–0129 for the content-and-commerce wave; the ledger is append-only — take the next free number if taken) · **Admin routes:** `/commerce/storefront/*` · **Public routes:** `/api/v1/public/store/*` and `/api/v1/public/account/*` · **Depends on:** REQ-008 (products, variants, orders, coupons, taxes, shipping, providers, invoices — this request is the customer-facing surface and the visitor account layer, not a second commerce engine) · **Bridges:** REQ-053 stock when installed, REQ-052 price lists when installed, REQ-029 for invoice PDFs, REQ-064/REQ-084 for theme page types, REQ-115 for product structured data, REQ-060 for cart-abandonment campaigns.

### Scope (in / out)

**In**

- **Catalog surface.** Category pages with nested navigation, product listing with filters (category, price range, availability, tags, and variant options as attributes), sort (relevance, newest, price asc/desc, best-selling from order history), text search routed through the search index with a keyword fallback, and facet counts computed from the filtered set. Page size, listing variant and pagination mode (`pagination`, `load_more`, `infinite`) come from the theme's declared variants plus per-site settings, and every page of an infinite list is also reachable as a real URL.
- **Product detail.** Gallery from the media library with zoom, variant selector rendered from the option matrix (disabled combinations marked unavailable rather than hidden), price with compare-at, tax-aware display, stock state (`in_stock`, `low_stock`, `backorder`, `out_of_stock`) with the low-stock threshold from settings, quantity stepper with the per-order maximum, add-to-cart, digital-product note (delivery after payment through REQ-008's expiring links), shipping estimate hint, related products by category and tags, and the product's structured data read from REQ-115 when that module is installed.
- **Cart.** Server-side cart owned by a signed, http-only cookie token (the token is stored hashed), usable signed-out and merged into the account's cart on sign-in: items are unioned by variant with quantities added, capped at the per-order maximum, then revalidated (price, availability, coupon) with a visible "your cart changed" summary naming each adjustment. The cart shows subtotal, discounts, estimated shipping and tax display, and honours a coupon code with a full explanation when refused.
- **Checkout.** Steps: contact → shipping address (or the account's address book) → shipping method → payment → review. Before payment the server recomputes the order through REQ-008's single totals function, re-checks price and stock, re-validates the coupon and stores an immutable quote snapshot on the checkout session; submitting creates the order with an idempotency key and redirects to the payment provider's hosted page (a Stripe-style hosted redirect adapter with a generic interface so a second provider is configuration, not a fork). Returning from payment renders an order status page keyed by a public order token; the provider webhook (REQ-008's signed route) is what actually marks the order paid. Tax display follows the site setting (inclusive or exclusive) with the amount and rate named on the review step.
- **Promotions and discount codes.** Coupons and their rules are REQ-008's; the storefront applies and displays them (code field, automatic-eligible coupons shown as "applied", refusal reasons surfaced verbatim from the totals function) and renders promotional banner sections from the theme when a site configures them. No second discount engine.
- **Visitor accounts.** Accounts are per site and strictly separate from panel users: sign-up, e-mail verification, sign-in, sign-out, password reset, profile (name, phone, marketing consent), address book (multiple addresses, default shipping and billing, country/region validated per the shipping zone list), orders list and detail, invoice PDF download, and a wishlist (products or variants, move to cart, remove). Tokens (verify, reset), sessions and the cart token are stored hashed, single-use where applicable and time-limited.
- **Admin storefront surface.** Per-site settings (guest checkout, tax display, listing variant and page size, pagination mode, per-order item maximum, wishlist toggle, abandonment window, order confirmation template reference, low-stock badge threshold), the visitor account list (verify, block, send reset) and an abandoned-cart view for support and campaign targeting.

**Out**

- Product, variant, category, order, coupon, tax, shipping, provider and invoice **data and rules** → REQ-008; this request never reimplements the totals function, coupon validation or the payment webhook.
- Warehouse transfers, stock documents and barcode flows → REQ-053; price lists, quotations and sales orders → REQ-052; general ledger and reconciliation → REQ-054; campaign sending and abandoned-cart e-mails → REQ-060.
- Card data handling of any kind — the platform stores only provider references and never sees an instrument; express wallets (Apple/Google Pay style), one-click stored cards and BNPL providers are later adapters behind the same interface.
- Marketplace multi-vendor checkout, subscriptions bought through the storefront (REQ-008's subscription plans remain admin-side in v1) and storefront-localized pricing per country beyond price lists.

### Screens (UI)

Public screens are theme-rendered page types declared in the theme manifest (`shop-index`, `shop-category`, `product`, `cart`, `checkout`, `account`); the renderer resolves the active theme exactly as it does for pages (docs/03-FRONTEND.md, `themes/README.md`).

| Route | Screen |
|---|---|
| `/shop` · `/shop/{category}` | Catalogue index · category listing with filters and facets |
| `/product/{slug}` | Product detail with gallery, variants, stock state, related |
| `/cart` | Cart with line editing, coupon field, totals |
| `/checkout` · `/checkout/return/{token}` | Checkout steps · payment return and order status |
| `/account` · `/orders` · `/orders/{number}` · `/addresses` · `/wishlist` | Account dashboard, orders, addresses, wishlist |
| `/account/signin` · `/signup` · `/reset/{token}` | Visitor authentication |
| `/commerce/storefront` · `/commerce/storefront/accounts` · `/commerce/storefront/abandoned` | Admin: settings · visitor accounts · abandoned carts |

- **Listing.** Top bar: active filters as removable chips, result count, sort select, view variant switch (when the theme ships more than one). Facets in a left rail on desktop and a sheet on mobile: categories (tree), price (range slider with numeric inputs), availability, tags, and one facet per variant option with value checkboxes and counts. Product cards show image, name, price with compare-at, stock badge (`Low` / `Out of stock`), and `Add to cart` (disabled for out-of-stock, with a backorder note when allowed). Empty state inside an over-filtered listing offers `Clear filters` and shows the two nearest categories; a zero-result search suggests removing one facet.
- **Product detail.** Gallery with thumbnails and a keyboard-navigable zoom; variant selector as option groups plus a "notify me when back in stock" hint (email capture is out of scope in v1, the control explains rather than doing nothing); price block with tax note ("incl. VAT" / "excl. VAT" from the site setting); stock line; quantity stepper; `Add to cart` with a success toast linking to the cart; digital note; accordion for description, specifications, shipping and returns; related products grid. Slug changes are the renderer's concern: a product whose slug moved answers a `301` to the current slug.
- **Cart.** Line table (image, name, variant summary, unit price, quantity stepper, line total, remove) with per-line stock warnings (`Only 2 left`), coupon field with inline refusal reasons, order summary (subtotal, discounts, shipping estimate, tax, total), `Continue shopping` and `Checkout`. A guest cart shows a note that signing in merges carts. Empty state explains the cart is empty and links to the catalogue. Mobile: lines become cards, the summary collapses into a sticky bar showing the total and `Checkout`.
- **Checkout.** A three-step layout with a progress indicator; the contact step validates e-mail (and offers sign-in for existing accounts), the address step validates required fields and country against the shipping zones with per-field errors, the shipping step lists methods with prices and the selected free-over state, and the review step shows the quoted order (immutable snapshot) with shipping and billing addresses, tax breakdown and the total. Failures are specific: price changed (with the old and new value), stock reduced (with the available quantity), coupon expired, payment declined or cancelled, or the session expired (with a link that restores the cart). Guest checkout is available when enabled; when disabled, the sign-in step explains and links to sign-up with the cart preserved.
- **Account.** Dashboard with recent orders and a wishlist teaser; orders table (`Order`, `Date`, `Items`, `Total`, `Payment`, `Fulfilment`, `Actions` with `View`, `Invoice PDF`, `Reorder` when all items are still available); order detail with lines, addresses, totals, shipment tracking and the invoice download; addresses as cards with `Add`, `Edit`, `Delete`, `Set default` (a delete of the default asks for a replacement); wishlist grid with `Move to cart` and `Remove`. Sign-in and sign-up validate inline and never leak whether an e-mail exists (verify message is neutral).
- **States and mobile.** Skeleton product cards, shimmering price blocks, empty and error states everywhere (a failed catalogue load shows retry with a request id), a payment-return page that is honest about pending/failed/cancelled (`Pending confirmation — this page refreshes automatically` while the webhook lands). On mobile (<768 px) the filters become a sheet, the product gallery a swipeable carousel, checkout a single column with a sticky total, and the quantity steppers use numeric keyboards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/public/store/products` | Catalogue with facets, filters, sort, paging | public, site-scoped, rate limited |
| GET | `/api/v1/public/store/products/{slug}` | Detail with variants, gallery, availability, related | public, site-scoped |
| GET | `/api/v1/public/store/categories` | Category tree with counts | public, site-scoped |
| GET · POST | `/api/v1/public/store/cart` | Read the cart from the cookie token · create if absent | public (token) |
| POST · PATCH · DELETE | `/api/v1/public/store/cart/items` (+ `/{id}`) | Add, change quantity, remove | public (token) |
| POST · DELETE | `/api/v1/public/store/cart/coupon` | Apply · remove a coupon code | public (token) |
| POST | `/api/v1/public/store/cart/merge` | Merge the guest cart into the signed-in account cart | public (token + session) |
| POST · GET · PATCH | `/api/v1/public/store/checkout` (+ `/{id}`) | Start · read · update contact, address and shipping method | public (token) |
| POST | `/api/v1/public/store/checkout/{id}/submit` | Quote, create the order, return the payment redirect | public (token), idempotency key |
| GET | `/api/v1/public/store/orders/{token}` | Order status page data (guests included) | public (order token) |
| GET | `/api/v1/public/store/downloads/{token}` | Digital delivery (REQ-008) | public (token) |
| POST | `/api/v1/public/account/signup` · `/signin` · `/signout` | Visitor authentication | public, rate limited |
| POST | `/api/v1/public/account/verify` · `/password-reset` · `/password-reset/confirm` | Verification and reset flows | public (token) |
| GET · PATCH | `/api/v1/public/account` | Profile and marketing consent | account session |
| GET · POST · PATCH · DELETE | `/api/v1/public/account/addresses` (+ `/{id}`) | Address book | account session |
| GET | `/api/v1/public/account/orders` · `/{number}` · `/{number}/invoice.pdf` | Orders, detail, invoice PDF (REQ-029) | account session |
| GET · POST · DELETE | `/api/v1/public/account/wishlist` (+ `/{product_id}`) | Wishlist read, add, move to cart, remove | account session |
| GET · PUT | `/api/v1/commerce/storefront/settings` | Per-site storefront settings | `commerce.storefront.manage` |
| GET · PATCH | `/api/v1/commerce/storefront/accounts` (+ `/{id}/verify`, `/block`, `/reset`) | Visitor accounts | `commerce.storefront.accounts.manage` |
| GET | `/api/v1/commerce/storefront/abandoned` | Abandoned carts with value and contact e-mail | `commerce.storefront.accounts.read` |

Public routes are rate-limited per IP and per token, scoped by site, and never leak another organization's catalogue (a foreign slug answers `404`). Account routes require the visitor session cookie and answer `404` for another account's order number; the order token route is separate and rotates per order.

### Data model

Migrations `0122_storefront.sql` (settings, carts, checkout sessions) and `0123_storefront_accounts.sql` (accounts, tokens, sessions, wishlist) — additive, commented in the `0009` style; every existing site gets one `storefront_settings` row.

```sql
storefront_settings (site_id uuid pk -> sites on delete cascade, guest_checkout bool not null default true,
  tax_display text not null default 'inclusive' check (tax_display in ('inclusive','exclusive')),
  listing_variant text not null default 'grid', page_size int not null default 24 check (page_size between 4 and 96),
  pagination text not null default 'pagination' check (pagination in ('pagination','load_more','infinite')),
  per_order_item_max int not null default 20 check (per_order_item_max between 1 and 100),
  wishlist_enabled bool not null default true, low_stock_badge_threshold int not null default 5,
  abandonment_hours int not null default 24 check (abandonment_hours between 1 and 720),
  confirmation_template text not null default 'order_confirmation', updated_by uuid -> users, updated_at)
storefront_carts (id uuid pk, organization_id uuid not null, site_id uuid not null, token_hash text not null,
  account_id uuid null -> storefront_accounts on delete set null, customer_id uuid null -> commerce_customers on delete set null,
  currency char(3) not null, coupon_code text, status text not null default 'active'
  check (status in ('active','converted','abandoned','merged')), item_count int not null default 0,
  subtotal numeric(14,2) not null default 0, last_activity_at timestamptz not null default now(),
  created_at/updated_at)  unique (token_hash), index (site_id, status, last_activity_at desc)
storefront_cart_items (id uuid pk, cart_id uuid not null -> storefront_carts on delete cascade,
  product_id uuid not null -> commerce_products on delete cascade, variant_id uuid null -> commerce_variants on delete cascade,
  quantity int not null check (quantity between 1 and 100), name_snapshot text not null,
  options_snapshot jsonb not null default '{}', unit_price_snapshot numeric(14,2) not null,
  added_at/updated_at)  unique (cart_id, product_id, coalesce(variant_id,'00000000-…'::uuid))
storefront_checkout_sessions (id uuid pk, organization_id uuid not null, site_id uuid not null, cart_id uuid not null -> storefront_carts,
  account_id uuid null, customer_id uuid null, email text not null, phone text, shipping_address jsonb not null default '{}',
  billing_address jsonb not null default '{}', shipping_method_id uuid -> commerce_shipping_methods,
  totals_snapshot jsonb not null default '{}', currency char(3) not null, tax_display text not null,
  status text not null default 'started' check (status in ('started','awaiting_payment','paid','failed','cancelled','expired')),
  provider_id uuid null -> commerce_payment_providers, provider_reference text, order_id uuid null -> commerce_orders on delete set null,
  idempotency_key text not null, expires_at timestamptz not null, created_at/updated_at)
  unique (idempotency_key)
storefront_accounts (id uuid pk, organization_id uuid not null, site_id uuid not null, email citext not null,
  password_hash text not null, name text, phone text, marketing_consent bool not null default false,
  status text not null default 'pending' check (status in ('pending','verified','blocked')),
  commerce_customer_id uuid null -> commerce_customers on delete set null, verified_at timestamptz,
  last_signin_at timestamptz, failed_attempts smallint not null default 0, locked_until timestamptz,
  created_at/updated_at)  unique (site_id, lower(email::text))
storefront_account_tokens (id uuid pk, account_id uuid not null -> storefront_accounts on delete cascade,
  kind text not null check (kind in ('verify','reset')), token_hash text not null unique,
  expires_at timestamptz not null, used_at timestamptz, created_at)
storefront_account_sessions (id uuid pk, account_id uuid not null -> storefront_accounts on delete cascade,
  token_hash text not null unique, expires_at timestamptz not null, last_seen_at timestamptz not null default now(),
  ip_hash text, ua_hash text, created_at)
storefront_wishlist_items (id uuid pk, account_id uuid not null -> storefront_accounts on delete cascade,
  product_id uuid not null -> commerce_products on delete cascade, variant_id uuid null, created_at)
  unique (account_id, product_id, coalesce(variant_id,'00000000-…'::uuid))
```

Checks: `subtotal >= 0`, `item_count >= 0`, addresses carry `country` as ISO 3166-1 alpha-2, `expires_at > created_at`. Indexes: `storefront_carts (site_id, status, last_activity_at desc)`, `storefront_cart_items (cart_id)`, `storefront_checkout_sessions (status, expires_at)` for the expiry sweep and `(site_id, created_at desc)` for support, `storefront_accounts (site_id, status)` and `(organization_id, lower(email::text))`, `storefront_wishlist_items (account_id)`. The checkout session carries the quoted snapshot; the order it produces is REQ-008's `commerce_orders` with `channel = 'storefront'`, so reporting does not need a second table. A sweep expires sessions past `expires_at`, marks carts abandoned after the configured window (emitting the event once) and restores nothing automatically.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `storefront.cart.created` · `.merged` | A token cart is created · a guest cart merges into an account cart | `cart_id`, `site_id`, `items`, `merged_items` |
| `storefront.cart.abandoned` | Sweep passes the abandonment window on an active cart with items | `cart_id`, `site_id`, `subtotal`, `currency`, `email_present` |
| `storefront.checkout.started` · `.completed` · `.failed` | Session transitions (completed when the order is created, failed on decline/cancel/expiry) | `session_id`, `order_id`, `reason_code`, `total`, `currency` |
| `storefront.account.created` · `.verified` · `.blocked` | Visitor account lifecycle | `account_id`, `site_id`, `status` |
| `storefront.wishlist.updated` | Wishlist added, removed or moved to cart | `account_id`, `product_count` |
| `storefront.settings.updated` | Per-site storefront settings changed | `site_id`, `changed_keys` |

Consumed: `order.created`, `order.paid`, `order.fulfilled`, `order.cancelled` and `payment.failed` (REQ-008) drive the order status page and the confirmation state; `inventory.low_stock` (REQ-008) and `inventory.stock.low` (REQ-053 when installed) refresh availability badges and cache entries; `product.updated` revalidates listing caches and related grids; `coupon.redeemed` closes the loop on the cart's coupon. Webhook relevance: `storefront.checkout.completed` is the reliable "order placed" signal for export integrations, and `storefront.cart.abandoned` is the entry point REQ-060's abandoned-cart campaign subscribes to. Payloads carry ids, amounts and contact presence flags — never an address, an e-mail or line-level buyer data.

### Acceptance criteria

- [ ] `cargo test -p omnion-module-ecommerce` (storefront target) is green, covering cart merge arithmetic, quantity caps, coupon re-validation, session idempotency and address validation.
- [ ] Both migrations apply on a fresh and on a populated database and seed one settings row per existing site.
- [ ] Category listing filters (category, price, availability, tag, option facet) each narrow the result set, facet counts match the filtered rows, and clearing filters restores the full set.
- [ ] Pagination works in all three modes with the second page reachable as a URL, and an over-filtered listing shows the `Clear filters` empty state with two nearest categories.
- [ ] A product with two options renders every combination, unavailable combinations are marked rather than hidden, and adding to cart stores the correct variant with its option snapshot.
- [ ] Out-of-stock add-to-cart is refused with a readable message; a `backorder` product adds with the backorder note visible.
- [ ] A guest cart survives a browser restart (token cookie), and signing in merges it into the account cart with quantities summed and a visible change summary when a price or availability changed.
- [ ] Coupon behaviour matches REQ-008's engine exactly: a valid code applies and shows the discount, an expired or per-customer-exceeded code is refused with the engine's reason, and the storefront never applies a code the engine would reject.
- [ ] Checkout recomputes totals server-side and the quoted snapshot equals the created order to the cent (fixture: three lines, one line discount, one coupon, one tax rate).
- [ ] A stock change between the quote and payment blocks the order with the available quantity named, and a price change blocks with old and new values, in both cases without losing the cart.
- [ ] Submitting the same checkout session twice with the same idempotency key creates exactly one order and returns the same redirect.
- [ ] The hosted payment redirect works end to end in the provider's test mode, returning to a status page that shows `Pending confirmation` until the signed webhook lands and then shows `Paid`; a cancelled payment shows `Cancelled` with a link back to the cart.
- [ ] The order confirmation e-mail is sent once per paid order through the mail path with the configured template and contains the order number, total and status link.
- [ ] Account flows: sign-up sends a verification mail, an unverified account cannot sign in, reset works once and expires, and five failed attempts lock the account with a neutral message.
- [ ] An account sees only its own orders, downloads its invoice PDF (issued through REQ-008 with the REQ-029 renderer), edits addresses (a default delete asks for a replacement) and manages a wishlist that moves items into the cart and out again.
- [ ] Admin storefront settings persist per site and the client honours them: disabling guest checkout forces the sign-in step, switching tax display changes the labels and amounts presentation, and raising the per-order maximum changes the stepper cap.
- [ ] Abandoned carts appear in the admin view with value and contact presence after the configured window, and `storefront.cart.abandoned` fires exactly once per cart.
- [ ] Every mutation writes an audit entry (account changes, settings, blocks), public routes are rate-limited, and catalogues, carts and orders of another organization answer `404`.
- [ ] All public screens and the three admin screens have empty, loading and error states with zero high findings at desktop and 390 px, and the mobile checkout is completable start to finish.

### QA plan

The walkthrough extends `scripts/qa/walkthrough.cjs` with a storefront script: seed a category with two products (one two-variant with stock, one out-of-stock), open `/shop` and a category (apply two filters, clear them, switch sort, page to 2), open the product (select variants, add to cart, see the badge), open `/cart` (change quantity, apply a valid coupon, then an expired one and see the reason), start `/checkout` as a guest (address validation error, then a valid address, choose a shipping method, review the totals), pay in the provider's test mode and verify the status page flips to `Paid` after the webhook. Then sign up as a visitor (verify through the mail sink), add an address, place a second order signed in after adding an item to a guest cart first (merge proof), download the invoice PDF, reorder, and manage the wishlist. Admin: `/commerce/storefront` (flip guest checkout off and confirm the checkout step changes), `/commerce/storefront/accounts` (block one account and see the sign-in refusal) and `/commerce/storefront/abandoned` (after shortening the window in the seed, see one cart with its value). The visual check must see: product cards with real images and price formatting matching the site currency, a variant selector with visibly disabled combinations, a cart summary whose total aligns with the lines, a checkout stepper with completed steps, an order status page with a readable pending state, and no clipped amounts, no placeholder images and no control that does nothing. Screenshots: `site-shop-index`, `site-product-detail`, `site-cart`, `site-checkout`, `page-storefront-settings`, `mobile-checkout`.

### Slices

1. **Catalogue and theme contract.** `0122` settings and cart tables, the public catalogue and detail endpoints with facets and search, the theme page types (`shop-index`, `shop-category`, `product`) with listing variants and pagination modes, availability from REQ-053 or REQ-008's ledger, related products and the slug redirect. *Done when:* acceptance 1–5 and 16 pass and a real catalogue renders through a theme on desktop and mobile.
2. **Cart, checkout and payments.** Cart lifecycle with the token cookie, merge, quantity caps, coupon application and change summaries; checkout sessions with server-side requote, address validation, shipping methods, tax display, idempotent order creation and the hosted payment redirect; the return page and the confirmation e-mail. *Done when:* acceptance 6–13 pass, a test-mode payment produces exactly one paid order, and the totals match REQ-008's engine to the cent.
3. **Accounts, orders and wishlist.** `0123` accounts, tokens, sessions and wishlist; sign-up, verification, sign-in, reset, lockout; the account area with orders, invoice PDF, address book and wishlist; the admin account list with verify/block/reset. *Done when:* acceptance 14–15 pass and the full visitor journey from sign-up to reorder works in the walkthrough.
4. **Admin depth, abandonment and promotions.** Storefront settings enforcement, the abandoned-cart sweep and view, the withdrawal window for sessions and carts, promotional banner wiring, the `storefront.*` event set, and the closed-loop consumers of REQ-008's order events. *Done when:* acceptance 17–18 pass and the abandoned-cart event reaches a subscribed campaign entry point once.

### Risks / notes

- **One totals function.** The storefront calls REQ-008's totals function for every displayed and charged figure; a client-side or second server-side calculation is how carts and invoices start disagreeing.
- **Revalidate before charging.** Price, stock, coupon and shipping are re-checked at submit; a change is surfaced as a specific message with the old and new values, never as a generic failure, and the cart survives.
- **Idempotent payment.** Order creation is keyed by the checkout session's idempotency key, and the order is marked paid only by the signed provider webhook; a refresh of the return page must never create a second order or a second confirmation e-mail.
- **Guest cart tokens are bearer credentials.** Stored hashed, single-purpose, rotated on merge, cleared on conversion, and never logged; a cart cookie alone can never read an account's orders.
- **Visitor accounts stay separate from panel users.** Separate tables, separate cookies, separate sessions, no promotion path — the same boundary REQ-064 draws for members.
- **No card data, ever.** The hosted flow keeps the platform out of PCI scope; the adapter interface carries a redirect URL, a return URL and a webhook reference only, and provider credentials remain references (REQ-008's rules apply unchanged).
- **Tax display is presentation.** Inclusive and exclusive display never changes the stored amounts or the invoice, and the wording on the review step names the rate so the customer sees why the number is what it is.
- **Overselling is prevented at confirmation, not at add-to-cart.** Cart quantities are a wish, stock is reserved when the order is confirmed (REQ-053's reservation path), and scarcity is communicated with truthful stock states rather than fake urgency.
- **Faceted URLs and SEO.** Filter combinations are crawlable but canonicalised to the category page with `noindex` on deep facet combinations, so a catalogue does not generate thousands of near-duplicate URLs; structured data for products comes from REQ-115 when installed.
- **Cache discipline.** Listing and detail responses are cached per site, currency and locale, and invalidated by `product.updated`, stock events and settings changes; a stale price on a public page is a support incident, so the validator key includes the product's `updated_at`.
- **Mobile checkout is the primary path.** The full flow is exercised at 390 px in QA, including the address sheet, the shipping method list and the sticky total; a desktop-only pass does not satisfy this request.
