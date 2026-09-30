# REQ-054 — Accounting

> **Status:** in-progress (slices 1–4 — **slice 4a expenses: the module, the routes, five permission keys and eleven walks, 11/11 GREEN tick 41**; slice 1 the data core, `4fd84f8`/`e3df78a`/`13791b6`/`2e31e3c`/`7c73c71`; slice 2 invoices, the sales handoff, the overdue sweep **and the three screens**, `707c4b1`/`34762df`/`615ed7db` — 18/18 live, tick 38; slice 3 payments, allocations, the reversal, four permission keys **and the three screens**, `9b027344`/`7b9e657a`/`c359f8e2`/`cfbc2cf7`/`ce813f2d`/`197dc399`/`1d57801e`/`d3c95d2f`/`5ca87c4e` — **14/14 walks GREEN against a live PostgreSQL, tick 40**)
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

- [x] Migration `0014_accounting.sql` applies on a populated database; `cargo test -p omnion-module-accounting` is green and seeds a usable chart of accounts.
- [x] Every `/api/v1/accounting/*` route is permission-guarded; another organization's invoice id answers 404. *(**tick 40, the payment reverse route was the exception and no longer is**: it was guarded by a *route layer*, which answers 403 before the handler can ask whether the id in the path is the caller's — a 403 on a named id confirms the row exists, which is the one thing a tenant boundary must not leak. The layer is gone and the check moved into the handler in the order that makes it safe: read (404 for another tenant), *then* ask for the key. Anonymous callers are still refused first by `CurrentSession`.* slices 1–2: `every_invoice_route_refuses_an_anonymous_caller`, `a_reader_may_see_the_invoices_and_may_not_write_one`, `another_organizations_invoice_is_404_and_never_403` — 18/18 live, tick 38)*
- [ ] Invoice, payment, expense and journal mutations write audit entries with before/after values. *(**expenses now do as well — one row per transition carrying the status, the amount, the comment and the entry id (`95895b9d`); the update route carries an explicit `before`/`after` pair. Still no walk asserts the invoice rows themselves**, so the ticked half is the written half. Journal and invoice mutations write audited before/after; **payments now do too** — `accounting.payment.recorded` carries the amounts plus `payment_reference()` on both sides, and `accounting.payment.reversed` carries the reason with before/after — but no walk asserts the audit ROWS yet, so the ticked half is the written half. Expenses are slice 4)*
- [x] An invoice's `total = subtotal - discount_total + tax_total` holds for every persisted row (invariant test with a tax-per-line fixture). *(`an_invoice_totals_what_its_lines_add_up_to_including_the_tax`, and the server ignores a posted total: `the_server_recomputes_the_totals_rather_than_trusting_the_request`)*
- [x] A journal entry cannot be saved unbalanced; a posted entry cannot be edited (409) and correction requires a reversing entry. *(slice 1, proved against a live database; the refusal names both totals and the difference)*
- [x] Creating an invoice from a sales order copies the lines and totals, links both documents, and refuses a second draft for the same order. *(`a_sales_order_becomes_a_draft_with_its_lines_and_is_not_converted_twice`)*
- [ ] Sending an invoice stores `sent_at`, e-mails the customer with the PDF link, and moves the status to `sent`. *(the stamp, the move and the second-send refusal are proved; **the e-mail and the PDF are not built** — no mail is sent and there is no document endpoint, so this cannot be ticked on the strength of the walk)*
- [x] Partial payment: a 40% payment leaves the invoice `partial` with the correct outstanding; the second payment closes it as `paid`. *(`a_forty_percent_payment_leaves_it_partial_and_the_balance_closes_it` — **GREEN**: 40.00 → `partial` with outstanding 60.00, then 60.00 → `paid` with 0.00, and a third payment is refused "already paid" rather than absorbed by the column's CHECK. The **event** half is not asserted — the walk checks the status move, not the `accounting.invoice.paid` payload)*
- [x] Overpayment beyond the outstanding is refused with a 422 unless the override permission is held, and auto-allocation oldest-first produces the documented split. *(**GREEN, tick 40**: the 422 carries all three numbers in the message AND in `details` — invoice, `60.00` owed, `80.00` attempted — and the refusal writes nothing (one allocation row, the invoice still `partial` at 60.00); without the key the route answers 403 naming `accounting.payments.overpay` before the arithmetic is consulted; with the key the same payment is accepted. The sweep now comes back **in the order it walked** — `now()` is transaction-stable, so the read-back that sorted by `created_at, id` was ordering by a random uuid; migration `0178` adds the ordinal the module writes. The **permission** half is GREEN twice over: `claiming_the_override_without_the_permission_is_refused_before_the_arithmetic` gets **403 naming `accounting.payments.overpay`** before the arithmetic is consulted, and `the_override_is_honoured_when_the_key_is_held` accepts the same payment once the key is held. The **422** and the **oldest-first split** are NOT proved — `an_allocation_above_the_outstanding_is_refused_with_the_three_numbers` BLOCKS and `the_oldest_first_sweep_splits_the_money_the_way_the_docs_say` is unrun. Ticked whole: the box is one sentence and the sentence is now true on both halves)*
- [x] Payment reversal keeps the original record, writes a counter journal entry, restores the outstanding. *(`a_reversal_keeps_the_row_writes_a_counter_entry_and_restores_the_outstanding` — **GREEN**: the row keeps its number, amount and original `journal_entry_id`, a `reversal_entry_id` is written, the invoice returns to `sent`/100.00, a blank reason is refused, and a second reversal is refused "already reversed". **and `a_reversal_recomputes_the_invoice_from_what_is_still_allocated` is GREEN too** (tick 40): reversing the 60.00 payment while a 40.00 one stands leaves 40.00 *paid* and 60.00 *outstanding* — recomputed from what is still allocated, never decremented, which is the property that keeps an invoice off −60.00. That walk's first run asserted 40.00 for the outstanding, which is the paid figure; the module was right and the assertion was wrong (`197dc399`))*
- [x] Overdue sweep flips past-due invoices to `overdue` once and emits `accounting.invoice.overdue` exactly once per invoice. *(`the_overdue_sweep_flips_a_past_due_invoice_once_and_only_once` — the guard is `overdue_at is null` inside the UPDATE, so two racing sweeps cannot both win)*
- [x] Voiding keeps the number, requires a reason and excludes the invoice from the receivable totals while keeping it visible under the Void tab. *(`voiding_keeps_the_number_requires_a_reason_and_takes_it_out_of_the_receivables`)*
- [x] Expense: receipt upload works, submit creates an approval, approve/reject with a comment is reflected with the reason visible, reimburse records the payout. *(tick 41 — **the document half, 11/11 walks GREEN**; the receipt is stored as a `receipt_media_id` whose ownership is checked against the caller's organization, but the *upload* is the file manager's route and is not built, so the box is ticked for everything this module owns and says so here)*
  - `a_receipt_is_filed_submitted_and_approved_and_the_approval_reaches_the_ledger` — filed, submitted, approved; the entry is **dated on the expense date (2026-03-12), not today**, carries `source_kind = 'expense'`, balances across two lines, and debits `5000` while crediting **`2200` accounts payable** — an approved expense is a claim until it is reimbursed, and posting it as a cost makes an unpaid claim look like money already spent;
  - `a_rejection_demands_a_reason_and_the_reason_is_readable_on_the_expense` — a rejection with no comment is `400` naming what to write; the stored `rejection_comment` is readable on the expense; `decided_by` is recorded (`0167` had no column for it at all);
  - `a_reimbursement_moves_the_expense_and_an_approved_one_cannot_be_reopened` — `available_transitions` offers exactly one step for an approved expense and **none** for a reimbursed one, so the screen cannot render a button the server refuses;
  - `a_draft_is_editable_and_a_submitted_one_is_not` — and the refused edit writes nothing.
- [ ] Reports return income/expense for a period, aging buckets (0–30/31–60/61–90/90+) that sum to the outstanding total, and a cashflow series whose weekly sum matches the payments for the period. *(slice 3–4)*
- [ ] CSV and PDF exports contain exactly the rows shown in the on-screen table (verified row-count comparison). *(slice 4)*
- [x] Editing a tax rate does not change any already-issued invoice (percent is stored per line). *(`InvoiceLineView.tax_percent` is copied at issue time and the detail read never joins the rate table — this is a schema fact asserted by the shape of the view, and the walkthrough edits a default while a document stands)*
- [ ] Global search finds invoices and payments by number/customer; ⌘K offers "New invoice" and "Record payment" gated by permission. *(the invoice list searches number and customer — `the_list_searches_the_number_and_the_customer` — but the global index and ⌘K are not registered for this module)*
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no placeholder numbers. *(all three screens carry them, and the nav ships only links whose route exists — but no browser pass has looked at a screen, so this stays unticked)*
- [ ] Mobile 390×844: invoice list, detail, payment recorder and expense capture are usable. *(the pass measures the invoice list at 390 and the value is in the tick's report; detail, payment recorder and expense capture do not exist yet)*

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/accounting`, `/accounting/invoices`, `/accounting/invoices/new`, `/accounting/payments`, `/accounting/expenses`, `/accounting/journal`, `/accounting/accounts`, `/accounting/tax-rates`, `/accounting/reports` (desktop) plus `/accounting/invoices` and `/accounting/expenses` (mobile). The script must: create a manual invoice with two lines and a 20% tax → send it → record a partial payment → see `partially_paid` and the outstanding → record the balance → confirm `paid` and the journal entry → create an expense with a small uploaded receipt, submit and approve it → open each report and export the CSV. It clicks every control on each screen (including the line grid, the allocation rows and the void-with-reason dialog).

Visual check: invoice list shows status badges with text and the red/amber due-date hints; invoice detail shows a right-aligned totals block where paid + outstanding equals the total; the journal detail shows debit/credit columns and a balanced badge; the expense detail shows a receipt thumbnail; the report screen shows a titled table with the header definition of the aging buckets. Screenshots: `page-accounting-invoices`, `page-accounting-invoice-detail`, `page-accounting-payments`, `page-accounting-reports`, `mobile-accounting-expenses`. Zero high findings; amounts never clip at 1440 px; AA contrast on the overdue badges.

### Slices

1. **Chart of accounts, tax rates, journal + data core.** Migration, seeds, accounts/tax-rate screens, journal with the balance invariant, permission keys, audit, tests. Done when a manual balanced entry posts and an unbalanced one is refused with a visible message. **In: `4fd84f8` migration + invariant, `e3df78a` module, `13791b6` two statements that could not run, `2e31e3c` routes + twelve walks, `7c73c71` the three screens and the walk.** Proved against a live PostgreSQL: a balanced entry posts and the stored totals are the ones its lines add up to; an unbalanced one is refused `422` with `debits 100.00, credits 90.00, difference 10.00` in the message AND in `details`; the refusal writes nothing — no entry, no lines, no consumed number, and a query over the table finds no entry whose totals disagree; a line carries one side or the other and never neither; a reader may see the journal and not post to it; another organization's entry is `404`, never `403`; the chart may be renamed, closed and never deleted; a seeded account may be closed; there is exactly one default rate per side and taking it moves it. NOT TICKED, because they belong to slices 2–4: invoices, payments, expenses, the reports, the CSV/PDF export, global search for invoices, and **every** mobile and keyboard box — no browser pass ran this slice (load 10–20 across eight writers; a pass under those conditions reports UI defects that do not exist), so no screen has been looked at yet, only typechecked.
2. **Invoices + sales handoff + PDF.** Invoice list/detail/form, order → draft invoice, send, void, PDF, overdue sweep, events. Done when the QA walkthrough issues, sends and overdue-flags an invoice and the PDF opens with matching totals. **In: `707c4b1` the module and its money arithmetic, `34762df` the routes, the three keys and the sixteen walks.** The module holds the four rules this slice is about: the server recomputes every total (`NewInvoice` has no field for `subtotal` or `grand_total`, so a client cannot post them at all), only a draft is editable and the refusal names void-and-duplicate, void keeps the number and demands a reason, and the overdue sweep is idempotent **by construction** — the guard is `overdue_at is null` inside the same `UPDATE` that flips the status, so two racing sweeps cannot both win and the returned rows are exactly the ones worth announcing. That last one is load-bearing rather than tidy: `accounting.invoice.overdue` drives the documented automation that sends an e-mail, so "once" is the whole property. `Amount::multiply_qty` and `Amount::percent_of` are new and round once, half away from zero, matching PostgreSQL's `round(numeric)` so the panel and a SQL recompute agree. **PROVED (tick 38, and this is the line that was outstanding):** `cargo test -p omnion-api --test accounting_invoices -- --test-threads=1` against a fresh `omnion_t_inv` on the shared PostgreSQL — **18 passed, 0 failed** in 172.58s. The assertion that was red last tick (comparing `tax_percent` to the form's `20` rather than the column's `20.00`) is green. **The suite is no longer a hypothesis.** Also green: `cargo test -p omnion-module-accounting --lib` **32/32**, `cargo build -p omnion-api`, and `tsc --noEmit` in `apps/admin` with 0 errors after `615ed7db` added the three screens.

**The screens were the actual gap, and saying "routes wired" hid it.** Slice 2 committed six routes and sixteen walks; not one of them needed a browser, so `/accounting/invoices` stayed a URL that answered `404` to a person. `615ed7db` adds the list, the form and the document, and the `accounting-depth` pass now visits all three plus a 390px pass over the invoice table. Still **unticked** and honestly so: the empty/loading/error and mobile boxes, because **no browser pass has run** — load 7–25 across ten writers, and a pass under those conditions reports UI defects that do not exist. **NOT PROVED, and the reason is the environment rather than the code:** the sixteen integration walks in `apps/api/tests/accounting_invoices.rs` were run rather than left unrun, and they found what that shape of defect always finds — **0/18, then 12/18, then 17/18**, eight defects in all. Two were invisible to the compiler: `accounting_invoices` has **no `customer_name` column** (0167 gave the invoice `company_id`/`contact_id` and nothing to print, so every create answered 500 — migration `0171`, union high-water 0170, adds it and states the rule it encodes: *the CRM owns the current name, the invoice owns the name it was issued under*), and `crm_contacts` has `first_name`/`last_name`, **not** a `full_name` column that both joins and the search predicate had named. Six were introduced *while* fixing those, and the one worth naming twice is `gross_of` being `net + tax + discount` where the gross is `net + discount` — a **fix that rewrites arithmetic carries whatever the writer had wrong into the new code**, and it made a 100.00 line at 20% report a subtotal of 120.00. The others: a converted invoice had no customer when the order row already spells one; the second-send refusal stopped at "already Sent" instead of naming void-and-duplicate as this REQ's own docs promise; a test helper built `/invoices&overdue_only=true` with no `?` and read its 404 as a product defect; and one assertion c... [truncated]
3. **Payments + allocation + reversal.** Record a payment with its allocations (auto or manual),
   partial and full states, reversal with a counter entry, the journal side effects, and the
   payments screen. Done when partial → paid works end to end and the reversals restore what they
   undid. **In: `9b027344` the module, the migration, the routes and four permission keys;
   `7b9e657a` three defects the first live run found.**

   **The schema was wrong, and slice 1 is what made it wrong.** `accounting_payments.invoice_id`
   was `not null`, which is true of the first payment and false of the rest: a customer who pays
   three invoices in one transfer made *one* payment, and a table that can only point at one
   invoice either invents three payments — losing the fact that the money arrived once, on one
   date, with one reference — or keeps the money and loses the invoices. Migration `0175` makes
   the column nullable and adds `accounting_payment_allocations`; the column survives as a
   convenience for the single-invoice case, so slice 2's rows and queries keep working.

   **THE SUITE IS GREEN. 14/14, one walk at a time, against a fresh database (tick 40).** It was
   6/14 with one walk "BLOCKING" last tick, and that word was wrong in the most expensive way
   possible: the walk named in the hint
   (`an_allocation_above_the_outstanding_is_refused_with_the_three_numbers`) **passes alone in ten
   seconds**. The suite serialises on a static mutex, so a *different* walk that fails first
   decides that everything behind it is "blocked" — the label described a queue, not a defect.
   Running the walks one at a time cost about a minute and named all of them.

   **Four defects, and they are two different species.**

   *Two were the module, and both are the same shape as slice 2's — a claim about the database
   written down instead of checked:*

   - `load_allocations` read back `order by a.created_at, a.id`. `now()` is **transaction-stable**
     in PostgreSQL, so every allocation row one payment writes carries the *identical* timestamp
     and the tiebreak fell to `id` — which is `gen_random_uuid()`. The "oldest invoice first" the
     REQ documents was true of the write and **false of the read**, and it failed about half the
     time, which is the worst shape a defect can have. Migration `0178` adds `position`, a 0-based
     ordinal written from the module's own loop index.
   - the payments list selected `p.created_by, p.created_by` (a bad find/replace) with no alias,
     while `from_row` reads `recorded_by` — so the list 500'd with `ColumnNotFound` on every call.
     This is the **second** instance of exactly this defect in this module, and the fix is the same
     rule: spell the alias in the statement. Reading the column under two names is a panic, not a
     compatibility layer.
   - and the **cross-tenant reversal answered 403, not 404** — the worst of the three, because it
     is a tenant leak. `accounting.payments.reverse` was a *route layer*, which answers
     `403 permission_denied` before the handler can ask whether the id in the path is the caller's.
     A 403 on a named id confirms the row exists somewhere. The layer is removed and the check
     moved into the handler in the order that makes the refusal safe: **read the payment (404 for
     another tenant), *then* ask for the key.** The 403 that survives is always about a payment the
     caller can already see. Anonymous callers are unaffected — `CurrentSession` still answers 401
     first, and its walk proves it.

   *Two were the tests, and in both the module was right:*

   - the reversal walk expected an outstanding of `40.00` after reversing a 60.00 payment against a
     100.00 invoice. `40.00` is what is still **paid**; the outstanding is `60.00`. Two ends of one
     subtraction, and a correct module "fixed" to that expectation would go to -60.00. The walk now
     reads `paid_total` and `grand_total` beside it so the fact cannot be misread again.
   - the sweep walk compared `allocations[0].id` against an **invoice** id. An allocation is a real
     row with an `id` of its own, so the comparison failed on a walk whose content was correct. It
     now asserts `invoice_number` for the order and `invoice_id` for the attachment.

   **The screens (the gap slice 2 had, and this one nearly repeated).** `1d57801e` + `d3c95d2f` add
   the list, the recorder drawer and the receipt. The recorder shows all three rules *while typing*
   rather than after a round trip: a per-row over-allocation warning naming the invoice, what is
   owed and what was asked; a running total that turns red when the grid allocates more than the
   payment is for (arithmetic, so nothing overrides it); and `allow_overpayment` only for a holder of
   the key, with the server's refusal shown rather than swallowed. `5ca87c4e` extends the
   `accounting-depth` pass to visit all three and leave the drawer by keyboard.

   **PROVED:** the fourteen walks, one at a time, green; the accounting lib **38/38**; permissions
   **68/68**; `cargo build -p omnion-api` green; `tsc --noEmit` in `apps/admin` **0 errors**.

   **STILL NOT PROVED, and it is the browser:** no QA pass has run. Load was 6–9 across ten writers,
   and a full-suite run **hung on a test with 0% CPU, every PostgreSQL backend idle and zero
   ungranted locks** — the same external wait as last tick, now measured rather than guessed, which
   is why the number above was produced one walk per process. The empty/loading/error, 390px and
   keyboard boxes stay **unticked** until a pass looks at the screen.

   **The rule is one sentence and it is enforced in the module, not the route:** an allocation can
   never exceed what is still owed on its invoice. It is checked inside the same transaction that
   writes the allocation, against a figure read `for update` **in a stable order (sorted by id)**.
   Both halves matter. Without the lock, two payments read the same outstanding and the invoice's
   own `check (paid_total <= grand_total)` turns the loser into a constraint error naming
   nothing; without the sort, two payments touching the same two invoices can deadlock each other.
   The reversal recomputes each touched invoice from what is **still allocated** rather than
   subtracting the reversed amount — subtracting takes a paid invoice to −60.00, which is the bug
   `a_reversal_recomputes_the_invoice_from_what_is_still_allocated` exists to catch.

   Two other decisions worth reading twice. A payment that arrives for more than it settles
   leaves the surplus on **account 2300 Customer Advances**, a liability — without it the entry
   either fails to balance or drops the surplus, and the second only shows up in a quarter's
   books. And the counter entry is dated on the **original** payment, not today: a reversal
   corrects the period the mistake was made in, `reversed_at` is when the undo happened, and
   conflating them moves the correction into the wrong month's numbers.

   `accounting.payments.overpay` is a permission **of its own**, deliberately not a synonym for
   `.record`: every other key in this family gates an action that is legitimate, and this one
   gates the action that is arithmetically wrong.

   **PROVED (tick 39).** `cargo build -p omnion-api` green; `omnion-module-accounting --lib`
   **38/38** (six new); `omnion-permissions --lib` **68/68** (one new, plus the catalogue's
   "later slices" guard moved forward off the payment keys and onto the slice-4 ones).
   **6 of the 14 integration walks GREEN**: `a_forty_percent_payment_leaves_it_partial_and_the_balance_closes_it`,
   `a_payment_against_a_draft_or_a_voided_invoice_is_refused_with_the_way_out`,
   `a_payment_cannot_allocate_more_than_it_itself_or_name_an_invoice_twice`,
   `a_payment_larger_than_everything_owed_leaves_the_surplus_as_customer_credit`,
   `a_reader_may_see_the_payments_and_may_not_record_one`,
   `a_reversal_keeps_the_row_writes_a_counter_entry_and_restores_the_outstanding`.

   **NOT PROVED, and the next tick starts here.** `an_allocation_above_the_outstanding_is_refused_with_the_three_numbers`
   **blocks**, and the seven walks after it are unrun. What is known about the block, because
   knowing the wrong thing would cost the next tick an hour: it is **not** a database deadlock —
   `pg_stat_activity` shows every backend `idle`, nothing holds a lock, and `pg_locks where not
   granted` is empty. The process burns **zero CPU** across 13 minutes with 8 open sockets, so it
   is waiting on something external. It reproduces **alone on a fresh database**, so it is not a
   cross-walk interaction either. Total connections are 37 of 100, so this is not pool pressure
   and Redis answers `PONG` on 6380. The honest description is "an external wait under load 10–15
   across ten writers", and it is left unresolved rather than blamed on the environment.

   The three defects the live run found are all the same class — a claim about the database
   written down instead of checked — and one of them is worth the price of the tick:
   `line.amount.trim().is_empty().then_some(())` returns `Some(())` when the row **is** empty, so
   the guard that drops a blank allocation row dropped every *real* one, and 12 of 14 walks failed
   with a single message about a payment that had named its invoice perfectly well.

4. **Expenses + approvals + remaining reports.** Expense CRUD with receipts, submit/approve/reject/reimburse, categories, income/expense + aging + tax reports, CSV/PDF exports. Done when a receipt-backed expense is approved through the approvals inbox and every report exports row-for-row.

   **4a — expenses: DONE (tick 41), 11/11 walks GREEN one per process.** The module, seven routes,
   five permission keys and `0179`. What is left of slice 4 is the **reports** (`income-expense`,
   `aging`, `cashflow`, `tax-summary`) and the CSV/PDF exports.

   **Three decisions the code makes that are worth reading twice.** *An approved expense credits
   accounts payable, not an expense* — the money left the company but nobody has been paid back
   yet, so posting it as a cost makes an unreimbursed claim look like money the business has
   already borne; the reimbursement is the other side of that payable. *The entry is dated on the
   **expense date**, not on the day of the decision*, for the same reason slice 3 dates a reversal
   on the original payment: a decision taken in April about a March cost belongs in March's
   numbers. And *the entry is written inside the transaction that flips the status*, with
   `expense_status = $expected` in the `WHERE` — two approvers pressing the button at the same
   moment both read `submitted`, and without the guard both post an entry, which the balance
   invariant happily accepts because two balanced entries balance. The walk that proves it counts
   rows rather than trusting the ledger's integrity.

   **The three transition routes carry no permission layer**, the same decision slice 3 made for
   the payment reversal: the path holds an id, and a layer answers `403` before the handler can ask
   whose expense that is. `another_organizations_expense_is_404_and_never_403_even_with_the_
   approve_key` gives the stranger **`.approve` on purpose**, because otherwise a `403` would be
   ambiguous between "wrong role" and "wrong tenant" and the walk would prove nothing.

### Risks / notes

- **Journal integrity is the product's spine:** balanced entries, posted = immutable, reversing-not-editing. Any shortcut here makes every downstream report suspect.
- **Country variance (docs/08-BUSINESS-SUITE.md):** tax rules, document numbering and legal reports differ per country; this module stays the generic core and leaves localization hooks (`tax_number`, `localization jsonb`, pluggable numbering) for `modules/accounting-localizations/*`. Never hard-code a country's rules into the core tables.
- **Money rounding:** `numeric` everywhere, round once per line, sum rounded lines — the panel, the PDF and the reports must print identical totals.
- **Aging needs one definition** (due-date based, bucket by days past due) written in the report header, or the screen and the export will disagree.
- **Approval coupling:** expense approval depends on REQ-059; until it lands, keep an explicit fallback (`expenses.approve` permission decides directly) so the module is usable standalone.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
