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

## Implementation spec

> **Module:** `modules/accounting` (crate `omnion-module-accounting`, workspace member) · **Migration:** `database/migrations/0014_accounting.sql` (next free slot at build time) · **Admin routes:** `/accounting/*` · **Permission family:** `accounting.*` · **Depends on:** core crates + `modules/sales` (REQ-052) for the order handoff + `modules/approvals` (REQ-059) for expense approval + `crates/media` for receipts.

### Scope (in / out)

**In**

- Chart of accounts, lite: seeded account tree per organization (asset / liability / equity / income / expense), editable names and codes, deactivate instead of delete.
- Journal: entries with lines (debit/credit), balanced invariant, source link (`invoice`, `payment`, `expense`, `manual`), post/unpost with audit.
- Tax rates per organization (name, percent, kind `sales` / `purchase`, default flag) plus a default per product.
- Invoices: manual or from a sales order, lines with qty/price/tax, due dates, status machine, PDF (REQ-029 template `invoice`), e-mail send, void with reason.
- Payments: full and partial against invoices with allocations, methods (bank transfer, card, cash, other), reference number, auto-created journal entry; unallocated credit on a customer.
- Expenses: category, vendor, date, amount, tax, receipt attachment (media), submit → approve → reimbursed flow through REQ-059, reject with reason.
- Reports: income/expense summary by period, receivable aging (0–30 / 31–60 / 61–90 / 90+), cashflow-lite (money in vs out per week), tax summary; CSV export and PDF export.
- Overdue sweep: invoices past due flip to `overdue` and emit an event an automation can chase.

**Out (tracked elsewhere)**

- Country localizations (e-invoice / e-archive / waybill / KDV rules and local reports) → `modules/accounting-localizations/*` per docs/08-BUSINESS-SUITE.md; this REQ leaves the hooks (`tax_number`, `localization jsonb`, pluggable document numbering) but ships none.
- Bank feeds, reconciliation imports and payroll → out of scope (no bank integration in this wave). Purchase orders/bills → `modules/purchases` (future). Timesheet → invoice conversion → REQ-056.
- Report *engine* charts → REQ-028; the numbers here are plain aggregates with a CSV/PDF door.

### Screens (UI)

Module nav: **Overview · Invoices · Payments · Expenses · Journal · Accounts · Tax rates · Reports**.

| Route | Screen |
|---|---|
| `/accounting` | Overview: outstanding receivable, overdue count and value, this month's income/expense, cash in/out, recent invoices and payments |
| `/accounting/invoices` | Invoice list (table) with status tabs (All / Draft / Sent / Partially paid / Paid / Overdue / Void) |
| `/accounting/invoices/new`, `/accounting/invoices/{id}` | Invoice create (manual or from an order) / detail with lines, payments, journal link, PDF, actions |
| `/accounting/payments` | Payment list with method and date filters |
| `/accounting/expenses`, `/accounting/expenses/new`, `/accounting/expenses/{id}` | Expense list / create / detail with receipt preview and approval state |
| `/accounting/journal` | Journal entries list + entry detail (lines, debit/credit, balanced badge) |
| `/accounting/accounts` | Chart of accounts tree editor |
| `/accounting/tax-rates` | Tax rate list + editor (name, percent, kind, default) |
| `/accounting/reports` | Report screen (type selector, period, filters, table + chart, export) |
| `/accounting/settings` | Currency, invoice numbering, payment terms default, expense categories, approval thresholds |

**Invoice list** — columns: `Number`, `Customer` (CRM link), `Issue date`, `Due date` (red when overdue, amber ≤7 days), `Status` (badge), `Total`, `Paid`, `Outstanding`, `Source` (`order O-2026-0012` / `manual`), `Updated`. Filters: search (number/customer), status (multi), customer, issue/due range, overdue-only toggle, amount range. Bulk: send, void (with reason, requires `accounting.invoices.void`), export, record payment. Row actions: open, PDF, e-mail, record payment, duplicate as draft, void. Shortcuts: `n` new invoice, `/` search, `enter` open, `p` record payment, `shift+p` PDF, `?` help. States: skeleton table, empty state ("Create an invoice" + "Import from an order"), error state with retry.

**Invoice detail** — header: number, status badge, customer, dates, actions (`Send`, `Record payment`, `PDF`, `Duplicate`, `Void`); body: lines table (read-only once sent), totals block (subtotal, discount, tax per rate, total, paid, outstanding), payments list with allocation amounts, journal entry link, timeline. Editing a sent invoice is refused (409) — the flow is void + duplicate.

**Invoice form** — Customer (required, CRM combobox), Issue date (required, default today), Due date (required, ≥ issue date, default issue + payment terms), Currency (default org), Payment terms, Reference/PO, Tax number (free text, ≤32, used by localizations), Notes, Lines grid: Product (optional combobox), Description (required when no product), Qty (> 0), Unit price (≥ 0), Tax rate (select from the organization's rates, default = product/org default), Line total (computed). Server recomputes all totals; the client copy is display only. Creating from an order pre-fills lines and locks the source.

**Payments** — list columns: `Date`, `Customer`, `Method`, `Amount`, `Currency`, `Reference`, `Allocated` (sum), `Unallocated`, `Journal` (link), `Recorded by`. Record drawer: Customer, Date, Method, Amount (>, ≤ outstanding unless explicitly allowed), Reference, Note, allocation rows (invoice, outstanding, amount to apply; "auto-allocate oldest first" button). Validation: allocation total must equal the payment amount (or less, leaving credit), no double-allocation, cannot overpay an invoice beyond its outstanding without an explicit override permission. A payment writes a journal entry (debit cash/bank, credit receivable).

**Expenses** — list columns: `Date`, `Category`, `Vendor`, `Employee`, `Amount`, `Tax`, `Receipt` (thumbnail/icon), `Status` (badge: Draft / Submitted / Approved / Reimbursed / Rejected), `Updated`. Copy Turkish examples in the UI only where helpful (e.g. category "Yol ve konaklama"). Form: Date (required, not in the future), Category (required, from settings; create-inline), Vendor, Amount (> 0), Tax rate (optional), Currency, Payment method, Employee (default caller), Receipt (upload to media, images + PDF, ≤20 MB, required when the org setting says so), Note. Actions: Submit, Approve/Reject (with comment, per permission), Mark reimbursed, Reopen (audited).

**Journal** — entries list columns: `Number`, `Date`, `Memo`, `Source`, `Lines`, `Debit total`, `Credit total`, `Balanced` (check badge), `Status` (draft/posted). Entry detail: line table (account, memo, debit, credit) with a live balance indicator that blocks saving an unbalanced entry; post/unpost actions are permission-guarded and audited. Manual entries are allowed only with `accounting.journal.manage`.

**Accounts** — tree by type with code, name, active toggle, "used by N lines" guard that refuses deletion of an account with postings. **Tax rates** — table with name, percent, kind, default flag; editing a rate never changes already-issued invoices (they store their own percent).

**Reports** — one screen, report type selector, period picker, filters (customer, owner, category); renders a totals table plus a simple bar/line chart, a "figures agree" note (totals equal the sum of the underlying rows), and `Export CSV` / `Export PDF`. Aging buckets are defined once and printed in the header so the definition is visible.

**Mobile:** lists are card rows (number → customer → total → status), invoice detail stacks with the totals block first, the payment recorder is a full-screen sheet, expenses allow camera capture of the receipt on mobile browsers.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/accounting/invoices` | Invoice list / create (manual or `order_id`) | `accounting.invoices.read` / `.create` |
| GET/PATCH | `/api/v1/accounting/invoices/{id}` | Detail / update while draft | `accounting.invoices.read` / `.update` |
| POST | `/api/v1/accounting/invoices/{id}/send` | Mark sent + e-mail the PDF | `accounting.invoices.send` |
| POST | `/api/v1/accounting/invoices/{id}/void` | Void with reason (keeps the number) | `accounting.invoices.void` |
| GET | `/api/v1/accounting/invoices/{id}/pdf` | PDF through REQ-029 | `accounting.invoices.read` |
| GET/POST | `/api/v1/accounting/payments` | Payment list / record with allocations | `accounting.payments.read` / `.record` |
| POST | `/api/v1/accounting/payments/{id}/reverse` | Reverse a payment (audited, creates a counter entry) | `accounting.payments.reverse` |
| GET/POST | `/api/v1/accounting/expenses` | Expense list / create | `accounting.expenses.read` / `.create` |
| GET/PATCH | `/api/v1/accounting/expenses/{id}` | Detail / update while draft | `accounting.expenses.read` / `.update` |
| POST | `/api/v1/accounting/expenses/{id}/submit` | Submit for approval (REQ-059) | `accounting.expenses.create` |
| POST | `/api/v1/accounting/expenses/{id}/decision` | Approve / reject with comment | `accounting.expenses.approve` |
| POST | `/api/v1/accounting/expenses/{id}/reimburse` | Mark reimbursed (payment recorded) | `accounting.expenses.approve` |
| GET/POST | `/api/v1/accounting/accounts` | Chart of accounts read / create-edit | `accounting.accounts.read` / `.manage` |
| GET/POST | `/api/v1/accounting/journal` | Journal list / create manual entry | `accounting.journal.read` / `.manage` |
| POST | `/api/v1/accounting/journal/{id}/post`, `/unpost` | Post / unpost an entry | `accounting.journal.manage` |
| GET/POST | `/api/v1/accounting/tax-rates` | Tax rates read / manage | `accounting.taxrates.read` / `.manage` |
| GET | `/api/v1/accounting/reports/{report}` | `income-expense`, `aging`, `cashflow`, `tax-summary` | `accounting.reports.read` |
| GET | `/api/v1/accounting/reports/{report}/export` | CSV / PDF of the same filter set | `accounting.reports.read` |

All list endpoints accept `?status=&customer=&from=&to=&cursor=&limit=`; mutations return the fresh record.

### Data model

```text
accounting_accounts(id uuid pk, organization_id uuid not null, code text not null, name text not null,
  kind text not null, parent_id uuid, active boolean not null default true)
accounting_tax_rates(id uuid pk, organization_id uuid not null, name text not null,
  percent numeric(5,2) not null, kind text not null, is_default boolean not null default false)
accounting_invoices(id uuid pk, organization_id uuid not null, number text not null, company_id uuid,
  contact_id uuid, order_id uuid, invoice_status text not null default 'draft', currency char(3) not null,
  issue_date date not null, due_date date not null, payment_terms text, reference text, tax_number text,
  subtotal numeric(14,2) not null default 0, discount_total numeric(14,2) not null default 0,
  tax_total numeric(14,2) not null default 0, total numeric(14,2) not null default 0,
  amount_paid numeric(14,2) not null default 0, sent_at timestamptz, paid_at timestamptz,
  voided_at timestamptz, void_reason text, localization jsonb not null default '{}')
accounting_invoice_lines(id uuid pk, invoice_id uuid not null references accounting_invoices(id) on delete cascade,
  position integer not null, product_id uuid, description text not null, qty numeric(14,3) not null,
  unit_price numeric(14,2) not null, discount_percent numeric(5,2) not null default 0,
  tax_rate_id uuid, tax_percent numeric(5,2) not null default 0, line_total numeric(14,2) not null)
accounting_payments(id uuid pk, organization_id uuid not null, number text not null, company_id uuid,
  payment_date date not null, method text not null, amount numeric(14,2) not null, currency char(3) not null,
  reference text, note text, journal_entry_id uuid, reversed_at timestamptz, recorded_by uuid, created_at timestamptz)
accounting_payment_allocations(id uuid pk, payment_id uuid not null references accounting_payments(id) on delete cascade,
  invoice_id uuid not null references accounting_invoices(id) on delete restrict, amount numeric(14,2) not null)
accounting_expense_categories(id uuid pk, organization_id uuid not null, name text not null, active boolean not null default true)
accounting_expenses(id uuid pk, organization_id uuid not null, category_id uuid not null, vendor text,
  employee_user_id uuid, expense_date date not null, amount numeric(14,2) not null, tax_percent numeric(5,2) not null default 0,
  currency char(3) not null, method text, receipt_media_id uuid, note text not null default '',
  expense_status text not null default 'draft', approval_request_id uuid, decided_by uuid, decided_at timestamptz,
  decision_comment text, reimbursed_at timestamptz)
accounting_journal_entries(id uuid pk, organization_id uuid not null, number text not null, entry_date date not null,
  memo text not null default '', source_kind text not null, source_id uuid, entry_status text not null default 'posted',
  posted_at timestamptz, posted_by uuid, created_at timestamptz not null default now())
accounting_journal_lines(id uuid pk, entry_id uuid not null references accounting_journal_entries(id) on delete cascade,
  position integer not null, account_id uuid not null, memo text not null default '',
  debit numeric(14,2) not null default 0, credit numeric(14,2) not null default 0)
```

Checks: `invoice_status in ('draft','sent','partially_paid','paid','overdue','void')`; `expense_status in ('draft','submitted','approved','rejected','reimbursed')`; `kind in ('asset','liability','equity','income','expense')` on accounts; `percent between 0 and 100`; `qty > 0`; `unit_price >= 0`; `amount > 0` on payments and expenses; `due_date >= issue_date`; `total = subtotal - discount_total + tax_total` (invariant check); `debit >= 0 and credit >= 0 and (debit = 0) <> (credit = 0)`; unique `(organization_id, code)` on accounts, unique `(organization_id, lower(name))` on tax rates, unique `(organization_id, number)` on invoices/payments/entries; `refund`-safe: an allocation can never exceed the invoice's outstanding (service rule + test).

Indexes: `accounting_invoices_org_status_idx (organization_id, invoice_status, due_date)`, `accounting_invoices_due_idx (organization_id, due_date) where invoice_status in ('sent','partially_paid','overdue')`, `accounting_payments_org_date_idx (organization_id, payment_date desc)`, `accounting_payment_allocations_invoice_idx (invoice_id)`, `accounting_journal_entries_source_idx (source_kind, source_id)`, `accounting_journal_lines_account_idx (account_id)`, `accounting_expenses_org_status_idx (organization_id, expense_status, expense_date desc)`.

Migration: `database/migrations/0014_accounting.sql`, additive; seeds the lite chart of accounts (1000 Cash, 1200 Accounts receivable, 2000 Accounts payable, 2100 VAT payable, 3000 Equity, 4000 Sales revenue, 5000 Operating expenses + children) and one default tax rate (0% "Exempt", plus a 20% "Standard VAT" example) for existing organizations, and the same set for new ones. Journal entries are immutable once posted: correction is a reversing entry, never an edit.

### Events

Emitted: `accounting.invoice.issued`, `accounting.invoice.sent`, `accounting.invoice.paid`, `accounting.invoice.partially_paid`, `accounting.invoice.overdue`, `accounting.invoice.voided`, `accounting.payment.recorded`, `accounting.payment.reversed`, `accounting.expense.submitted`, `accounting.expense.approved`, `accounting.expense.rejected`, `accounting.journal.posted`. Payloads carry ids, currency, amounts and (for `invoice.overdue`) the days past due. Consumed: `sales.order.confirmed`/`sales.orders/{id}/invoice-draft` (REQ-052) creates a draft invoice; `approvals.request.decided` (REQ-059) settles expense approvals; `projects.timesheet.approved` (REQ-056) can be turned into a draft invoice by rule.

Webhook relevance: `accounting.invoice.issued`, `accounting.invoice.overdue` and `accounting.payment.recorded` are the names a finance system would subscribe to; the overdue event is the trigger for the documented automation (`wait 3 days → send e-mail → create task → notify finance`, docs/08).

### Acceptance criteria

- [ ] Migration `0014_accounting.sql` applies on a populated database; `cargo test -p omnion-module-accounting` is green and seeds a usable chart of accounts.
- [ ] Every `/api/v1/accounting/*` route is permission-guarded; another organization's invoice id answers 404.
- [ ] Invoice, payment, expense and journal mutations write audit entries with before/after values.
- [ ] An invoice's `total = subtotal - discount_total + tax_total` holds for every persisted row (invariant test with a tax-per-line fixture).
- [ ] A journal entry cannot be saved unbalanced; a posted entry cannot be edited (409) and correction requires a reversing entry.
- [ ] Creating an invoice from a sales order copies the lines and totals, links both documents, and refuses a second draft for the same order.
- [ ] Sending an invoice stores `sent_at`, e-mails the customer with the PDF link, and moves the status to `sent`.
- [ ] Partial payment: a 40% payment leaves the invoice `partially_paid` with the correct outstanding; the second payment closes it as `paid` and writes a `accounting.invoice.paid` event.
- [ ] Overpayment beyond the outstanding is refused with a 422 unless the override permission is held, and auto-allocation oldest-first produces the documented split.
- [ ] Payment reversal keeps the original record, writes a counter journal entry, restores the outstanding and is audited.
- [ ] Overdue sweep flips past-due invoices to `overdue` once and emits `accounting.invoice.overdue` exactly once per invoice.
- [ ] Voiding keeps the number, requires a reason and excludes the invoice from the receivable totals while keeping it visible under the Void tab.
- [ ] Expense: receipt upload works, submit creates an approval, approve/reject with a comment is reflected with the reason visible, reimburse records the payout.
- [ ] Reports return income/expense for a period, aging buckets (0–30/31–60/61–90/90+) that sum to the outstanding total, and a cashflow series whose weekly sum matches the payments for the period.
- [ ] CSV and PDF exports contain exactly the rows shown in the on-screen table (verified row-count comparison).
- [ ] Editing a tax rate does not change any already-issued invoice (percent is stored per line).
- [ ] Global search finds invoices and payments by number/customer; ⌘K offers "New invoice" and "Record payment" gated by permission.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no placeholder numbers.
- [ ] Mobile 390×844: invoice list, detail, payment recorder and expense capture are usable.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/accounting`, `/accounting/invoices`, `/accounting/invoices/new`, `/accounting/payments`, `/accounting/expenses`, `/accounting/journal`, `/accounting/accounts`, `/accounting/tax-rates`, `/accounting/reports` (desktop) plus `/accounting/invoices` and `/accounting/expenses` (mobile). The script must: create a manual invoice with two lines and a 20% tax → send it → record a partial payment → see `partially_paid` and the outstanding → record the balance → confirm `paid` and the journal entry → create an expense with a small uploaded receipt, submit and approve it → open each report and export the CSV. It clicks every control on each screen (including the line grid, the allocation rows and the void-with-reason dialog).

Visual check: invoice list shows status badges with text and the red/amber due-date hints; invoice detail shows a right-aligned totals block where paid + outstanding equals the total; the journal detail shows debit/credit columns and a balanced badge; the expense detail shows a receipt thumbnail; the report screen shows a titled table with the header definition of the aging buckets. Screenshots: `page-accounting-invoices`, `page-accounting-invoice-detail`, `page-accounting-payments`, `page-accounting-reports`, `mobile-accounting-expenses`. Zero high findings; amounts never clip at 1440 px; AA contrast on the overdue badges.

### Slices

1. **Chart of accounts, tax rates, journal + data core.** Migration, seeds, accounts/tax-rate screens, journal with the balance invariant, permission keys, audit, tests. Done when a manual balanced entry posts and an unbalanced one is refused with a visible message.
2. **Invoices + sales handoff + PDF.** Invoice list/detail/form, order → draft invoice, send, void, PDF, overdue sweep, events. Done when the QA walkthrough issues, sends and overdue-flags an invoice and the PDF opens with matching totals.
3. **Payments + allocation + cashflow.** Record payment with allocation (auto/manual), partial and full states, reversal, journal side effects, payments screen, cashflow report. Done when partial → paid works end to end and the cashflow sums match the payments.
4. **Expenses + approvals + remaining reports.** Expense CRUD with receipts, submit/approve/reject/reimburse, categories, income/expense + aging + tax reports, CSV/PDF exports. Done when a receipt-backed expense is approved through the approvals inbox and every report exports row-for-row.

### Risks / notes

- **Journal integrity is the product's spine:** balanced entries, posted = immutable, reversing-not-editing. Any shortcut here makes every downstream report suspect.
- **Country variance (docs/08-BUSINESS-SUITE.md):** tax rules, document numbering and legal reports differ per country; this module stays the generic core and leaves localization hooks (`tax_number`, `localization jsonb`, pluggable numbering) for `modules/accounting-localizations/*`. Never hard-code a country's rules into the core tables.
- **Money rounding:** `numeric` everywhere, round once per line, sum rounded lines — the panel, the PDF and the reports must print identical totals.
- **Aging needs one definition** (due-date based, bucket by days past due) written in the report header, or the screen and the export will disagree.
- **Approval coupling:** expense approval depends on REQ-059; until it lands, keep an explicit fallback (`expenses.approve` permission decides directly) so the module is usable standalone.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
