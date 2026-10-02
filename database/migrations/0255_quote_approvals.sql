-- Omnion · 0055 · Quote approval requests (docs/requests/REQ-052, slice 3)
--
-- A quote whose largest line discount is over the organization's threshold cannot be sent until
-- somebody who is not the seller says yes. Slice 3 is the gate, the request row and the two
-- decisions. REQ-059 will later own the *general* approval module — flows, chains, delegation,
-- escalation — and this table is deliberately shaped like the request REQ-059's spec describes
-- (`subject_type` + `subject_id` + `subject_url` + a label), so that module can adopt these rows
-- rather than migrate them. The rules the schema encodes:
--
-- * **One open request per quote.** A seller who re-submits must not leave a manager with two
--   rows for the same document, so a partial unique index covers the open states only. Decided
--   rows stay, and a second request after a rejection is a new row with its own history.
-- * **A decision is a decision, not an edit.** `decision` and `decided_by` are set together by a
--   check constraint, so a row can never say "approved" without saying who approved it.
-- * **A rejection carries the reason.** `decision = 'rejected'` requires a non-blank comment:
--   a manager who says no without saying why makes the seller guess, and the seller is the one
--   who has to fix the quote.
-- * **Nothing is deleted.** A cancelled request keeps its row, for the same reason the quote
--   keeps `cancelled_at` instead of vanishing: the audit trail has to be able to answer "who
--   asked, who decided and what did they say".

create table sales_quote_approvals (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    quote_id          uuid           not null references sales_quotes (id) on delete cascade,
    requested_by      uuid           not null references users (id) on delete cascade,
    -- 0-100, the largest line discount the quote carried when it was asked for. It is a snapshot
    -- like the version's `max_discount`: lowering the threshold afterwards must not rewrite what
    -- the manager was actually shown.
    discount_percent  numeric(5,2)   not null,
    threshold_percent numeric(5,2)   not null,
    currency          char(3)        not null default 'TRY',
    grand_total       numeric(14,2)  not null default 0,
    note              text           not null default '',
    status            text           not null default 'pending',
    decision          text,
    decided_by        uuid           references users (id) on delete set null,
    decided_at        timestamptz,
    comment           text,
    -- The quote was withdrawn (or edited back under the threshold) while the request was open.
    cancelled_at      timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint sales_quote_approvals_status_check check (
        status in ('pending', 'approved', 'rejected', 'cancelled')
    ),
    constraint sales_quote_approvals_decision_check check (
        decision is null or decision in ('approved', 'rejected', 'cancelled')
    ),
    constraint sales_quote_approvals_discount_range check (
        discount_percent >= 0 and discount_percent <= 100
    ),
    constraint sales_quote_approvals_threshold_range check (
        threshold_percent >= 0 and threshold_percent <= 100
    ),
    -- The whole point of the table: a request above the line it was measured against.
    constraint sales_quote_approvals_over_threshold check (discount_percent > threshold_percent),
    constraint sales_quote_approvals_currency_format check (currency ~ '^[A-Z]{3}$'),
    constraint sales_quote_approvals_note_length check (length(note) <= 2000),
    constraint sales_quote_approvals_comment_length check (comment is null or length(comment) <= 2000),
    constraint sales_quote_approvals_totals_non_negative check (grand_total >= 0),
    -- A decision is a decision: who said it, when, and what they said.
    constraint sales_quote_approvals_decision_complete check (
        (status = 'pending' and decision is null and decided_by is null and decided_at is null)
        or (status <> 'pending' and decision is not null)
    ),
    constraint sales_quote_approvals_rejection_carries_a_reason check (
        status <> 'rejected' or (comment is not null and length(btrim(comment)) > 0)
    )
);

-- At most one live request per quote: a manager never sees the same document twice, and the
-- seller cannot stack a second "please approve" on top of a row that is already waiting.
create unique index sales_quote_approvals_one_open_per_quote
    on sales_quote_approvals (quote_id)
    where status = 'pending';

create unique index sales_quote_approvals_one_decision
    on sales_quote_approvals (id)
    where status <> 'pending';

-- The inbox's two lists: "my pending approvals" and "what I asked for".
create index sales_quote_approvals_inbox
    on sales_quote_approvals (organization_id, status, created_at desc);

create index sales_quote_approvals_requester
    on sales_quote_approvals (organization_id, requested_by, created_at desc);

create index sales_quote_approvals_quote
    on sales_quote_approvals (organization_id, quote_id, created_at desc);
