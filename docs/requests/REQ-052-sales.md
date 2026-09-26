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
