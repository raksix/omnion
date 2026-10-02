-- Expenses get their decision trail (REQ-054, slice 4).
--
-- `0167_accounting.sql` created `accounting_expenses` with `approval_request_id`, `decided_at` and
-- `journal_entry_id`, and with **no column for who decided it**. That is the one column an auditor
-- asks for first and the module cannot answer: "who approved this?" resolves to "the column does
-- not exist". Two further gaps the same table has, and both are properties of the *document*:
--
-- 1. **A number.** Every other financial document in this module carries a per-organization
--    number (`INV-000007`, `PAY-000007`, `JE-42`) so a person can say "the third expense of the
--    quarter" and an export can be reconciled. The expense has only a uuid, which is unusable in
--    both directions.
-- 2. **A position in the approval order.** `expense_date` is the day the money was spent, not the
--    day the request entered the queue; `created_at` is transaction-stable, so two expenses
--    submitted in one request sort by a random uuid. The exact defect slice 3 paid for on
--    allocations: the read claims an order the database does not guarantee.
--
-- The decision comment lives next to the row rather than in `accounting_status_history` because
-- `decision_reason` already exists on this table and this REQ's own acceptance criteria say the
-- reason is *visible on the expense*. Splitting one decision across two tables is how a reason
-- goes missing.
--
-- Additive: every column is nullable or has a default, so existing rows are untouched and a row
-- written before this migration reads with an empty reason and no decider.

alter table accounting_expenses
    add column if not exists expense_number bigint,
    add column if not exists decided_by uuid,
    add column if not exists rejection_comment text not null default '',
    -- `0167` never created these two, although its own spec lists them. `note` is the filer's free
    -- text; `decision_reason` is what the *approver* wrote, and one column cannot be both — the
    -- filer does not know the decision when they write, and overwriting one with the other loses
    -- whichever was there first. Every write that named `note` failed with
    -- `column "note" of relation "accounting_expenses" does not exist`, which is a 500 on the
    -- create route and therefore on the whole module.
    add column if not exists note text not null default '',
    -- Who spent it, for an organization that reimburses its staff. Nullable: an expense paid
    -- directly to a supplier has no employee behind it, and a NOT NULL here would force a
    -- placeholder row that reads like a person.
    add column if not exists employee_user_id uuid references users (id) on delete set null;

comment on column accounting_expenses.expense_number is
  'Per-organization sequence number, allocated the same way as a journal entry (max + 1 for '
  'update). Nullable for rows written before this column existed; the read falls back to the row id.';

comment on column accounting_expenses.decided_by is
  'The user who approved or rejected this expense. The one column the table was missing: without '
  'it "who signed this off" has no answer at all, and an approval with no decider is not an audit '
  'trail, it is a state.';

comment on column accounting_expenses.rejection_comment is
  'Why the approver said no. Separate from decision_reason (0167) because that column is written '
  'for both decisions while the REQ names only the rejection comment as something an operator has '
  'to be able to read back on the expense itself.';

comment on column accounting_expenses.expense_date is
  'The day the money was spent. NOT the queue order: two expenses spent on one day but submitted '
  'in one request share this value because now() is transaction-stable, so the list orders by '
  'expense_date desc, expense_number desc — the second key is what actually breaks the tie.';

-- The list's default sort is "newest first", which reads this index when the filter names no
-- status. The existing `accounting_expenses_org_status_idx` covers the filtered case; this one
-- covers the unfiltered case, which is what the screen loads on open.
create index if not exists accounting_expenses_org_date_idx
    on accounting_expenses (organization_id, expense_date desc, expense_number desc);

create index if not exists accounting_expenses_org_category_idx
    on accounting_expenses (organization_id, category);

-- The backfill for rows that predate the number. `row_number()` per organization in the arbitrary
-- but stable order `created_at, id` produces, so a re-run on the same data numbers the same rows
-- the same way rather than producing a different arbitrary sequence each time.
--
-- The unique index below is what makes "the same number twice in one organization" impossible, and
-- it is **partial** (`where expense_number is not null`) because a unique *constraint* over a
-- nullable column lets unlimited NULLs through by definition — and NULL is exactly the state every
-- pre-existing row is in after this migration runs.
with numbered as (
    select
        e.id,
        row_number() over (
            partition by e.organization_id order by e.created_at, e.id
        ) as ordinal
    from accounting_expenses e
    where e.expense_number is null
)
update accounting_expenses e
set expense_number = n.ordinal
from numbered n
where e.id = n.id;

create unique index if not exists accounting_expenses_number_idx
    on accounting_expenses (organization_id, expense_number)
    where expense_number is not null;
