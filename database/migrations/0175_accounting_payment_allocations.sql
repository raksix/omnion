-- Payments: one document, many allocations (REQ-054, slice 3).
--
-- Slice 1 wrote `accounting_payments` with a single `invoice_id`, which is true of the first
-- payment somebody records and false of the second: a customer who pays three invoices with one
-- transfer has made one payment, and a table that can only point at one invoice either invents
-- three payments (losing the fact that the money arrived once, on one date, with one reference)
-- or keeps the money and loses the invoices. The column stays, nullable, because a payment that
-- arrives before the invoice it settles — a deposit, a prepayment — is a real row that has
-- nothing to point at yet.
--
-- The allocation table is the fix, and it is additive: no existing row changes shape, no number
-- is consumed and the unique (organization_id, payment_number) still holds.
--
-- What is NEW here beyond the allocation table:
--
-- * `number` (the `PAY-000007` a person reads) alongside the existing `payment_number`. The
--   integer was the sequence; the text is the document, and the invoice already made that split.
-- * `currency` and `company_id`, so the list can answer "whose money was this" without joining
--   the invoice the payment may not have been allocated to yet.
-- * `reversed_at` and the reversal's own entry id. A reversal is a second document, not an
--   update: the original keeps its date, its method and its reference, because "we undid the
--   March receipt" is a fact about April, not an edit of March.
-- * account 2300 Customer Advances, seeded here and for existing organizations. A payment may
--   arrive for more than the invoices it is applied to, and the remainder is money the customer
--   is owed back. Without a liability account to hold it, the entry either does not balance or
--   silently drops the surplus, and the second is the kind of bug that only shows up in a
--   quarter's books.

-- ---------------------------------------------------------------------------
-- 1. The columns the screen and the journal need
-- ---------------------------------------------------------------------------

alter table accounting_payments
    -- Nullable now, on purpose: a prepayment has no invoice. The name stays so the old rows and
    -- the old index keep meaning what they meant.
    alter column invoice_id drop not null,
    add column if not exists number text,
    add column if not exists company_id uuid references organizations (id) on delete set null,
    -- The customer name as the receipt spells it, for the same reason the invoice owns one
    -- (migration 0171): the CRM holds the *current* name, a payment is a document received on a
    -- day, and a receipt printed next month must show who was told what.
    add column if not exists customer_name text not null default '',
    add column if not exists customer_id uuid,
    add column if not exists currency char(3) not null default 'USD',
    add column if not exists note text not null default '',
    add column if not exists reversed_at timestamptz,
    add column if not exists reversal_entry_id uuid references accounting_journal_entries (id) on delete set null,
    add column if not exists reversed_by uuid,
    add column if not exists reversal_reason text not null default '';

-- The rows that exist now predate the display number. They are numbered from their own
-- `payment_number`, which is the same integer, so PAY-000007 is a document that was already
-- sequence position 7 rather than a number invented at upgrade time.
update accounting_payments
set number = 'PAY-' || lpad(payment_number::text, 6, '0')
where number is null;

-- A NOT NULL constraint is added only after the backfill, so a row that somehow missed the
-- update cannot make the migration fail halfway and leave the table half-migrated.
alter table accounting_payments
    alter column number set not null,
    add constraint accounting_payments_number_key unique (organization_id, number);

comment on column accounting_payments.number is
  'The per-organization display number, e.g. PAY-000007. Distinct from payment_number, which is '
  'the sequence position it is formatted from.';

comment on column accounting_payments.invoice_id is
  'The invoice this payment was recorded against, when the caller named one. Nullable because a '
  'prepayment arrives before the invoice it settles; the allocations are the truth about what it '
  'was applied to.';

comment on column accounting_payments.reversed_at is
  'Set by the reversal route. The row is never edited or deleted: the original document stands, '
  'and the counter entry dated the day of the reversal is what unwinds it.';

-- ---------------------------------------------------------------------------
-- 2. The allocations
-- ---------------------------------------------------------------------------
--
-- `on delete restrict` on the invoice, matching the REQ: a payment that is a fact about money
-- must not disappear because somebody deleted the document it was applied to, and an invoice with
-- money against it is one a finance person will not delete. The cascade from the payment is the
-- other direction and is right: reversing the payment removes the allocations with it.

create table accounting_payment_allocations (
    id uuid primary key default gen_random_uuid(),
    payment_id uuid not null references accounting_payments (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    invoice_id uuid not null references accounting_invoices (id) on delete restrict,
    amount numeric(14, 2) not null check (amount > 0),
    created_at timestamptz not null default now(),
    -- One row per invoice per payment. Without this a client that posts the same invoice twice in
    -- one request writes two rows, and the outstanding it computed from the first is the only
    -- thing standing between that and a double allocation.
    unique (payment_id, invoice_id)
);

-- The outstanding query: "what is still owed on this invoice" and "which invoices are open for
-- this organization, oldest first" are the same index read in two directions.
create index accounting_payment_allocations_invoice_idx
    on accounting_payment_allocations (organization_id, invoice_id);

create index accounting_payment_allocations_payment_idx
    on accounting_payment_allocations (payment_id);

-- The list filters on "unreversed payments of this organization, newest first".
create index accounting_payments_open_idx
    on accounting_payments (organization_id, paid_on desc, id)
    where reversed_at is null;

-- ---------------------------------------------------------------------------
-- 3. 2300 Customer Advances
-- ---------------------------------------------------------------------------
--
-- The seeded chart is a plpgsql function (REQ-054 slice 1) so that an organization born after
-- the migration owns the same chart as one that existed before it. `create or replace` is
-- additive: the function gains a row in its array, and the trigger that calls it is untouched.

create or replace function accounting_seed_chart_of_accounts()
returns trigger
language plpgsql
as $$
declare
    base jsonb := jsonb_build_array(
        jsonb_build_object('code', '1000', 'name', 'Assets',           'kind', 'asset'),
        jsonb_build_object('code', '1100', 'name', 'Cash',              'kind', 'asset'),
        jsonb_build_object('code', '1200', 'name', 'Accounts Receivable','kind', 'asset'),
        jsonb_build_object('code', '1500', 'name', 'Inventory',         'kind', 'asset'),
        jsonb_build_object('code', '2000', 'name', 'Liabilities',       'kind', 'liability'),
        jsonb_build_object('code', '2200', 'name', 'Accounts Payable',  'kind', 'liability'),
        -- 2300 exists for one reason: a payment that arrives for more than it is applied to. The
        -- surplus is a liability, not income, and putting it in income would overstate the period
        -- it landed in.
        jsonb_build_object('code', '2300', 'name', 'Customer Advances', 'kind', 'liability'),
        jsonb_build_object('code', '3000', 'name', 'Equity',            'kind', 'equity'),
        jsonb_build_object('code', '4000', 'name', 'Income',            'kind', 'income'),
        jsonb_build_object('code', '4100', 'name', 'Sales',             'kind', 'income'),
        jsonb_build_object('code', '5000', 'name', 'Expenses',          'kind', 'expense'),
        jsonb_build_object('code', '5100', 'name', 'Cost of Goods Sold','kind', 'expense')
    );
    row jsonb;
begin
    for row in select * from jsonb_array_elements(base) loop
        insert into accounting_accounts (organization_id, code, name, kind, system)
        values (new.id, row->>'code', row->>'name', row->>'kind', true)
        on conflict (organization_id, code) do nothing;
    end loop;

    insert into accounting_tax_rates (organization_id, name, percent, kind, is_default)
    values (new.id, 'Standard', 20, 'sales', true)
    on conflict (organization_id, name) do nothing;

    return new;
end;
$$;

-- The upgrade path, the same shape slice 1 used. `on conflict do nothing` makes it idempotent,
-- so a re-run of the whole migration file changes nothing.
insert into accounting_accounts (organization_id, code, name, kind, system)
select o.id, '2300', 'Customer Advances', 'liability', true
from organizations o
on conflict (organization_id, code) do nothing;
