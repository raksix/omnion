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
