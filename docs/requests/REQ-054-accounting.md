> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/accounting`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Light accounting so orders and expenses are traceable without a full ERP.

- **Chart of accounts** (lite, seeded per organization), journal entries.
- **Invoices** (from sales orders or manual) with lines, tax rates, due dates, statuses (draft/sent/paid/overdue).
- **Payments**: record against invoices, partial payments, payment methods.
- **Expenses**: categories, receipts (media), reimbursement state.
- **Tax rates** per organization with default per product.
- **Reports**: income/expense summary, receivable aging, cashflow-lite; export to CSV/PDF.
- **Events**: `accounting.invoice.issued`, `accounting.payment.recorded`.
