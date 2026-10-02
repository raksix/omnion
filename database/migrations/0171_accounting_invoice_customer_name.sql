-- Customer names on accounting invoices (docs/requests/REQ-054, slice 2).
--
-- WHY THIS COLUMN EXISTS AT ALL, since the invoice already carries `company_id` and `contact_id`.
--
-- The document says a **name**, and the name it says is the one that was true when it was issued.
-- Without a column of its own, the only way to print "Northwind Ltd" on a 2026 invoice is to
-- join to `crm_companies` at render time — which means renaming the company in the CRM silently
-- rewrites a document the customer already received, and a document whose number says one thing
-- and whose header says another is a support ticket about the wrong figure.
--
-- The rule this column encodes: **the CRM owns the current name, the invoice owns the name it was
-- issued under.** The list and the detail prefer the live name and fall back to this, so a
-- corrected CRM record shows up on an unsent draft and never on an issued document — which is
-- exactly when a correction is legitimate.
--
-- The name is a copy of what was typed, bounded like every other free-text field in this module,
-- and it is NOT NULL default '' rather than NULL: a screen that has to tell "the customer is
-- unnamed" from "we forgot to join" is a screen with two empty states, and one of them is a bug.

alter table accounting_invoices
    add column if not exists customer_name text not null default '';

-- The tax and the net of each line, stored rather than derived.
--
-- The first version of the reader back-solved the tax as `line_total / (1 + tax%)`. That is a
-- SECOND definition of the arithmetic the writer already performed, and it agrees on a round
-- number while disagreeing by a cent on a discounted line — which is exactly the case the REQ's
-- "the panel, the PDF and the reports must print identical totals" exists to prevent. Storing
-- both figures means the reader has nothing left to disagree with.
--
-- The CHECK keeps the three consistent with each other at the storage layer, so a future import
-- or a person with a psql prompt cannot write a line whose total, tax and net do not add up.
-- One statement per action, not one statement with two: `ALTER TABLE` accepts exactly one
-- action before its semicolon, and the second `ADD COLUMN` is a syntax error that takes the whole
-- migration with it — so every suite in the workspace answers `migrations must apply` rather than
-- naming the line.
alter table accounting_invoice_lines add column if not exists tax_amount numeric(14, 2);
alter table accounting_invoice_lines add column if not exists net_amount numeric(14, 2);

update accounting_invoice_lines
   set tax_amount = coalesce(tax_amount, 0),
       net_amount = coalesce(net_amount, line_total - coalesce(tax_amount, 0))
 where tax_amount is null or net_amount is null;

alter table accounting_invoice_lines
    alter column tax_amount set default 0,
    alter column tax_amount set not null,
    alter column net_amount set default 0,
    alter column net_amount set not null;

-- The line's own arithmetic, checked where the data is rather than only where it is written.
alter table accounting_invoice_lines
    add constraint accounting_invoice_lines_totals_agree
        check (line_total = net_amount + tax_amount);

-- A list screen that searches the customer's name scans this column, and a manual invoice — one
-- with no company and no contact behind it — is exactly the row that has only this to search on.
-- The predicate is partial because the overwhelmingly common row has a live name in the CRM and
-- this column is only ever a fallback; indexing all of them would cost writes to serve a read
-- that almost never needs them.
create index if not exists accounting_invoices_customer_name_idx
    on accounting_invoices (organization_id, customer_name)
    where customer_name <> '';
