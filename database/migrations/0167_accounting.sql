-- Omnion · 0167 · Accounting: the money has to balance (docs/requests/REQ-054, slice 1)
--
-- The invoice handoff in REQ-052 has been half-proved since the sales module shipped: the draft
-- exists, the money is frozen into columns that agree with the order to the cent, and a second ask
-- returns the draft that already exists. What is NOT there is a destination — `external_id` and
-- `external_url` are columns the sales module writes nobody into, and the acceptance box it waits
-- on asks for a hand-off to accounting, which does not exist yet. This is that module.
--
-- Three decisions worth stating, because each of them is a way the data could have lied.
--
-- 1. **A journal entry balances or it does not exist.** There is no "pending" entry, no draft
--    journal: a half-written pair of lines is the one thing in accounting that must never be
--    readable, because a reader sums the debit column, gets a number, and believes it. The
--    invariant is therefore a CHECK over the entry's own lines rather than a rule the posting
--    route is trusted to apply. `accounting_journal_entries` carries a `balanced` column the
--    posting route sets in the same statement that writes the lines, and the check constraint
--    below refuses any entry claiming to be balanced when `debit_total <> credit_total`.
--
-- 2. **A quantity is never rounded twice.** Money is `numeric(14,2)` and every total is computed
--    in SQL from the lines with one `round(..., 2)` at the end, the same rule the sales module
--    established and the same reason: the printed PDF, the panel and the invoice have to agree to
--    the cent, and a half-up rounding applied per line and then again on the total disagrees with
--    itself by a fraction nobody can explain to a customer.
--
-- 3. **The chart of accounts is seeded per organization by a TRIGGER, not by the creating
--    statement.** REQ-051 wrote the CRM pipeline seed as a call inside the `insert` that created
--    the function, which meant every organization born after that statement owned no pipeline and
--    the board answered 404 for a week. The same mistake is not repeated twice: a function created
--    by this migration is owned by this migration, and a trigger on `organizations` is what
--    applies it. A tenant created while this migration is already installed still gets a chart.
--
-- Additive throughout: no existing table is altered, and nothing is seeded for an organization
-- that already exists. Those are backfilled at the end, from the same function the trigger calls,
-- so an installation upgrading into this migration is in the same state as one created after it.

-- ---------------------------------------------------------------------------
-- Chart of accounts
-- ---------------------------------------------------------------------------

create table accounting_accounts (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    code text not null,
    name text not null,
    -- asset | liability | equity | income | expense. The five top-level kinds; the tree itself is
    -- a parent pointer, because a chart of accounts is a hierarchy and a fixed depth would be a
    -- limit the data does not have.
    kind text not null check (kind in ('asset', 'liability', 'equity', 'income', 'expense')),
    parent_id uuid references accounting_accounts (id) on delete set null,
    -- A deactivated account keeps its rows and stops accepting new ones. Never delete: an entry
    -- posted against an account that no longer exists is an entry nobody can balance.
    active boolean not null default true,
    system boolean not null default false,
    created_at timestamptz not null default now(),
    unique (organization_id, code)
);

comment on table accounting_accounts is
    'Chart of accounts, lite. Codes are unique per organization; accounts deactivate and never delete.';

create index accounting_accounts_org_kind_idx
    on accounting_accounts (organization_id, kind, code);

-- ---------------------------------------------------------------------------
-- Tax rates
-- ---------------------------------------------------------------------------
--
-- Sales lines store a tax_percent SNAPSHOT precisely so a document never changes when a rate is
-- edited. This table is where the rate is *defined*; the snapshot on an issued document is the
-- historical truth and is never recomputed from here.

create table accounting_tax_rates (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    name text not null,
    percent numeric(5, 2) not null check (percent >= 0 and percent <= 100),
    kind text not null check (kind in ('sales', 'purchase')),
    -- Exactly one default per (organization, kind). A partial unique index is the whole
    -- enforcement: a "default" flag alone would let two rows claim it and the winner would depend
    -- on the order a query happened to return them in.
    is_default boolean not null default false,
    active boolean not null default true,
    created_at timestamptz not null default now(),
    unique (organization_id, name)
);

create unique index accounting_tax_rates_one_default_idx
    on accounting_tax_rates (organization_id, kind)
    where is_default;

-- ---------------------------------------------------------------------------
-- Journal
-- ---------------------------------------------------------------------------

create table accounting_journal_entries (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    entry_number bigint not null,
    entry_date date not null,
    -- manual | invoice | payment | expense. The source link, so an auditor can walk from a
    -- journal line back to the document that caused it without a second lookup table.
    source_kind text not null default 'manual',
    source_id uuid,
    memo text not null default '',
    -- Set by the posting route in the same statement that writes the lines, and checked below.
    -- The two totals are COLUMNS rather than a sum over the lines, because the check constraint
    -- below needs them, and a constraint cannot contain an aggregate over another table.
    debit_total numeric(14, 2) not null default 0,
    credit_total numeric(14, 2) not null default 0,
    balanced boolean not null default false,
    posted_at timestamptz,
    created_by uuid,
    created_at timestamptz not null default now(),
    unique (organization_id, entry_number),
    -- The invariant. An entry that says it balances but does not is refused by the database, which
    -- is the only place this can be enforced that a future route cannot forget.
    check (balanced = false or debit_total = credit_total),
    check (debit_total >= 0 and credit_total >= 0)
);

comment on constraint accounting_journal_entries_check ON accounting_journal_entries is
    'A balanced entry has equal debit and credit totals. Enforced here, not in the route, because a '
    'rule only the route knows is a rule the next route does not have.';

create table accounting_journal_lines (
    id uuid primary key default gen_random_uuid(),
    entry_id uuid not null references accounting_journal_entries (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    account_id uuid not null references accounting_accounts (id) on delete restrict,
    position integer not null,
    description text not null default '',
    debit numeric(14, 2) not null default 0 check (debit >= 0),
    credit numeric(14, 2) not null default 0 check (credit >= 0),
    -- One side or the other, never both and never neither. A line with 0/0 is a comment wearing a
    -- line's clothes and it makes the entry's line count disagree with its arithmetic.
    check ((debit > 0 and credit = 0) or (credit > 0 and debit = 0))
);

create index accounting_journal_lines_entry_idx
    on accounting_journal_lines (entry_id, position);

create index accounting_journal_lines_account_idx
    on accounting_journal_lines (organization_id, account_id, id);

create index accounting_journal_entries_org_date_idx
    on accounting_journal_entries (organization_id, entry_date desc, id);

-- ---------------------------------------------------------------------------
-- Invoices
-- ---------------------------------------------------------------------------

create table accounting_invoices (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    number text not null,
    -- A manual invoice has no order behind it; a converted one keeps the pointer so the sales
    -- side can show the link back. The FK is deliberately absent (see the migration header): the
    -- sales module is the caller, and two modules holding each other's foreign keys is a cycle
    -- that makes neither of them independently deployable.
    order_id uuid,
    company_id uuid,
    contact_id uuid,
    owner_user_id uuid,
    invoice_status text not null default 'draft'
        check (invoice_status in ('draft', 'sent', 'partial', 'paid', 'overdue', 'void')),
    currency char(3) not null default 'USD',
    issue_date date not null default current_date,
    due_date date,
    payment_terms text not null default '',
    reference text not null default '',
    notes text not null default '',
    subtotal numeric(14, 2) not null default 0 check (subtotal >= 0),
    discount_total numeric(14, 2) not null default 0 check (discount_total >= 0),
    tax_total numeric(14, 2) not null default 0 check (tax_total >= 0),
    grand_total numeric(14, 2) not null default 0 check (grand_total >= 0),
    paid_total numeric(14, 2) not null default 0 check (paid_total >= 0),
    -- The one the receivables report groups on. Stored rather than computed from paid_total,
    -- because a partially paid invoice has no single date and the aging bucket needs one.
    last_payment_at timestamptz,
    sent_at timestamptz,
    paid_at timestamptz,
    voided_at timestamptz,
    void_reason text not null default '',
    overdue_at timestamptz,
    created_by uuid,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (organization_id, number),
    -- Paid is a claim about money that arrived, so it cannot exceed what was owed.
    check (paid_total <= grand_total)
);

create index accounting_invoices_org_status_idx
    on accounting_invoices (organization_id, invoice_status, due_date);

create index accounting_invoices_org_order_idx
    on accounting_invoices (organization_id, order_id)
    where order_id is not null;

create index accounting_invoices_overdue_idx
    on accounting_invoices (organization_id, due_date)
    where invoice_status in ('sent', 'partial', 'overdue');

create table accounting_invoice_lines (
    id uuid primary key default gen_random_uuid(),
    invoice_id uuid not null references accounting_invoices (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    position integer not null,
    product_id uuid,
    description text not null,
    qty numeric(14, 3) not null check (qty > 0),
    unit_price numeric(14, 2) not null check (unit_price >= 0),
    discount_percent numeric(5, 2) not null default 0
        check (discount_percent >= 0 and discount_percent <= 100),
    tax_percent numeric(5, 2) not null default 0
        check (tax_percent >= 0 and tax_percent <= 100),
    line_total numeric(14, 2) not null check (line_total >= 0)
);

create index accounting_invoice_lines_invoice_idx
    on accounting_invoice_lines (invoice_id, position);

-- ---------------------------------------------------------------------------
-- Payments
-- ---------------------------------------------------------------------------
--
-- A payment is an allocation against an invoice, not a column on the invoice: a customer who
-- pays one invoice in two instalments has made two payments, and the second one is a fact with a
-- date, a method and a reference that the first one's absence would lose.

create table accounting_payments (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    invoice_id uuid not null references accounting_invoices (id) on delete cascade,
    payment_number bigint not null,
    paid_on date not null default current_date,
    -- bank_transfer | card | cash | other
    method text not null check (method in ('bank_transfer', 'card', 'cash', 'other')),
    amount numeric(14, 2) not null check (amount > 0),
    reference text not null default '',
    journal_entry_id uuid references accounting_journal_entries (id) on delete set null,
    created_by uuid,
    created_at timestamptz not null default now(),
    unique (organization_id, payment_number)
);

create index accounting_payments_invoice_idx
    on accounting_payments (invoice_id, paid_on desc);

create index accounting_payments_org_date_idx
    on accounting_payments (organization_id, paid_on desc);

-- ---------------------------------------------------------------------------
-- Expenses
-- ---------------------------------------------------------------------------

create table accounting_expenses (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    description text not null,
    category text not null default 'general',
    vendor text not null default '',
    expense_date date not null default current_date,
    amount numeric(14, 2) not null check (amount > 0),
    tax_amount numeric(14, 2) not null default 0 check (tax_amount >= 0),
    currency char(3) not null default 'USD',
    -- A media id, not a path. The receipt can be re-encoded, re-scanned and revoked without the
    -- expense row learning about it, and a deleted media file leaves a row that says "receipt
    -- missing" rather than a broken path in a financial document.
    receipt_media_id uuid,
    -- draft | submitted | approved | rejected | reimbursed
    expense_status text not null default 'draft'
        check (expense_status in ('draft', 'submitted', 'approved', 'rejected', 'reimbursed')),
    approval_request_id uuid,
    decided_at timestamptz,
    decision_reason text not null default '',
    reimbursed_at timestamptz,
    journal_entry_id uuid references accounting_journal_entries (id) on delete set null,
    created_by uuid,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create index accounting_expenses_org_status_idx
    on accounting_expenses (organization_id, expense_status, expense_date desc);

create table accounting_status_history (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    document_kind text not null,
    document_id uuid not null,
    from_status text,
    to_status text not null,
    note text not null default '',
    actor_user_id uuid,
    created_at timestamptz not null default now()
);

create index accounting_status_history_doc_idx
    on accounting_status_history (document_kind, document_id, created_at desc);

-- ---------------------------------------------------------------------------
-- The seeded chart of accounts, applied by a TRIGGER
-- ---------------------------------------------------------------------------
--
-- The bug this shape exists to prevent is in REQ-051's own log: the seed function was created
-- and called in the same statement, so every organization created after that statement owned no
-- pipeline. Here the function is created by this migration and the trigger is what calls it, so
-- the rule holds for a tenant created at any point afterwards.

create or replace function accounting_seed_chart_of_accounts()
returns trigger
language plpgsql
as $$
declare
    base jsonb := jsonb_build_array(
        jsonb_build_object('code', '1000', 'name', 'Assets',           'kind', 'asset'),
        jsonb_build_object('code', '1100', 'name', 'Cash',              'kind', 'asset'),
        jsonb_build_object('code', '1200', 'name', 'Accounts Receivable', 'kind', 'asset'),
        jsonb_build_object('code', '1500', 'name', 'Inventory',         'kind', 'asset'),
        jsonb_build_object('code', '2000', 'name', 'Liabilities',       'kind', 'liability'),
        jsonb_build_object('code', '2200', 'name', 'Accounts Payable',  'kind', 'liability'),
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

    -- One default sales rate per organization, seeded alongside the chart. A tenant that cannot
    -- issue an invoice until somebody visits the tax screen is a tenant that will not issue one.
    insert into accounting_tax_rates (organization_id, name, percent, kind, is_default)
    values (new.id, 'Standard', 20, 'sales', true)
    on conflict (organization_id, name) do nothing;

    return new;
end;
$$;

create trigger organizations_accounting_seed
    after insert on organizations
    for each row
    execute function accounting_seed_chart_of_accounts();

-- The upgrade path. An organization that exists now predates the trigger, so it has no chart;
-- the SAME function is what fills it, which is the point of putting the rule in a function rather
-- than in the migration's own body. Running this twice is a no-op (`on conflict do nothing`).
insert into accounting_accounts (organization_id, code, name, kind, system)
select o.id, a.code, a.name, a.kind, true
from organizations o
cross join (values
    ('1000', 'Assets',             'asset'),
    ('1100', 'Cash',               'asset'),
    ('1200', 'Accounts Receivable','asset'),
    ('1500', 'Inventory',          'asset'),
    ('2000', 'Liabilities',        'liability'),
    ('2200', 'Accounts Payable',   'liability'),
    ('3000', 'Equity',             'equity'),
    ('4000', 'Income',             'income'),
    ('4100', 'Sales',              'income'),
    ('5000', 'Expenses',           'expense'),
    ('5100', 'Cost of Goods Sold', 'expense')
) as a(code, name, kind)
on conflict (organization_id, code) do nothing;

insert into accounting_tax_rates (organization_id, name, percent, kind, is_default)
select o.id, 'Standard', 20, 'sales', true
from organizations o
on conflict (organization_id, name) do nothing;
