-- Omnion · 0057 · Order reservations and the invoice handoff (docs/requests/REQ-052, slice 4)
--
-- Migration 0053 already created `sales_orders`, `sales_order_lines` and `sales_status_history`,
-- and nothing has written them yet. This migration adds the two things 0053 deliberately left
-- out, because both of them are **another module's table to adopt later** rather than columns the
-- sales desk should own:
--
-- * `sales_order_reservations` — the stock a confirmed order holds. REQ-053 (inventory) owns
--   warehouses, the stock ledger and the availability arithmetic; this table is the sales side of
--   that conversation. It exists **even when inventory is not installed**, so `reservation_state`
--   can say `total` or `partial` honestly instead of collapsing to `none` the moment REQ-053 is
--   absent. The spec asks for "a visible note rather than a silent failure", and a state column
--   that can only ever say `none` cannot be honest about it.
-- * `sales_invoice_handoffs` — the draft invoice this order owes accounting (REQ-054), shaped the
--   way that module's spec describes a document (`subject_type`/`subject_id`/`subject_url` plus a
--   label and a frozen payload) so REQ-054 adopts these rows rather than migrating them. It
--   mirrors `0055_quote_approvals.sql` deliberately: slice 3 set the precedent that a module may
--   hand a document forward without pretending the receiving module exists.
--
-- The rules the schema encodes:
--
-- * **A reservation is per line, not per order.** A five-line order holds five rows, so `partial`
--   is a countable fact rather than a guess and a release can be audited line by line.
-- * **One reservation per line, ever.** The unique index makes a second confirm — the
--   double-click, the retried request — a no-op at the database rather than a second hold of the
--   same stock, which is what the acceptance criteria mean by "a second confirm is a no-op".
-- * **A release is recorded, not deleted.** `state` moves to `released` with the timestamp and
--   the reason, because "when did this order stop holding that stock?" has to be answerable
--   afterwards; deleting the row would answer it with silence.
-- * **One live draft invoice per order.** Nobody may be handed two documents for one delivery.
--   The index covers the live states only, so a voided handoff does not block a fresh one.
-- * **The handoff freezes the money.** Currency and the three totals are columns rather than only
--   entries inside the payload, so the order detail prints the figure the accounting document was
--   raised for without re-deriving it from rows that may since have moved.

create table sales_order_reservations (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    order_id          uuid           not null references sales_orders (id) on delete cascade,
    line_id           uuid           not null references sales_order_lines (id) on delete cascade,
    product_id        uuid           references sales_products (id) on delete set null,
    description       text           not null default '',
    -- The quantity held, as numeric: the row never carries a float. A free-text line with no
    -- product is held too, because it is still something the warehouse has to know about.
    quantity          numeric(14,3)  not null default 0,
    unit              text           not null default 'piece',
    state             text           not null default 'held',
    held_at           timestamptz    not null default now(),
    released_at       timestamptz,
    -- Why the hold went away, so a release is a sentence rather than a null.
    released_reason   text           not null default '',

    constraint sales_order_reservations_state_check check (state in ('held', 'released')),
    constraint sales_order_reservations_quantity_positive check (quantity > 0),
    constraint sales_order_reservations_description_length check (length(description) <= 4000),
    constraint sales_order_reservations_reason_length check (length(released_reason) <= 2000),
    -- A release is complete or it did not happen: `state` and `released_at` cannot disagree.
    constraint sales_order_reservations_release_is_complete check (
        (state = 'held' and released_at is null)
        or (state = 'released' and released_at is not null)
    )
);

-- A second confirm of the same order is a no-op rather than a second hold. This constraint makes
-- that a property of the data and not of the handler's read-then-write.
create unique index sales_order_reservations_one_per_line
    on sales_order_reservations (order_id, line_id);

-- The order detail reads the holds of one order; the reports screen aggregates held quantity per
-- product, which is the same table read the other way round.
create index sales_order_reservations_by_order
    on sales_order_reservations (organization_id, order_id, held_at);

create index sales_order_reservations_by_product
    on sales_order_reservations (organization_id, product_id)
    where state = 'held';

create table sales_invoice_handoffs (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    order_id          uuid           not null references sales_orders (id) on delete cascade,
    -- The frozen money, taken from the order at the moment the draft was raised. Accounting
    -- computes its own figures from its own rules, but "did the two agree?" is a question the
    -- sales desk has to be able to answer, and that needs both numbers.
    currency          char(3)        not null default 'TRY',
    subtotal          numeric(14,2)  not null default 0,
    tax_total         numeric(14,2)  not null default 0,
    grand_total       numeric(14,2)  not null default 0,
    -- The line grid as the order held it, frozen. The handoff is a document, and a document that
    -- reads whatever the order says later is not a record of what was invoiced.
    payload           jsonb          not null default '{}'::jsonb,
    -- `draft` until REQ-054 issues the document, then `issued`; `void` if the order was cancelled
    -- before accounting turned it into anything.
    state             text           not null default 'draft',
    external_id       uuid,
    external_url      text,
    raised_by         uuid           references users (id) on delete set null,
    raised_at         timestamptz    not null default now(),
    settled_at        timestamptz,
    void_reason       text           not null default '',

    constraint sales_invoice_handoffs_state_check check (state in ('draft', 'issued', 'void')),
    constraint sales_invoice_handoffs_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint sales_invoice_handoffs_totals_non_negative check (
        subtotal >= 0 and tax_total >= 0 and grand_total >= 0
    ),
    constraint sales_invoice_handoffs_reason_length check (length(void_reason) <= 2000),
    -- A settled handoff names the document it became. `settled_at` without `external_id` would
    -- be a timestamp on a fact nobody can follow.
    constraint sales_invoice_handoffs_settled_names_a_document check (
        state <> 'issued' or external_id is not null
    )
);

-- One live draft per order: nobody is handed two invoices for one delivery. Voided and issued
-- rows leave the index, so a fresh draft is still possible.
create unique index sales_invoice_handoffs_one_live_per_order
    on sales_invoice_handoffs (order_id)
    where state in ('draft', 'issued');

create index sales_invoice_handoffs_by_state
    on sales_invoice_handoffs (organization_id, state, raised_at desc);
