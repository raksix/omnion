-- Omnion · 0127 · Stock adjustment approval requests (docs/requests/REQ-053, slice 2)
--
-- Slice 1 shipped `inventory_settings.adjustment_approval_threshold` and an
-- `InventoryError::ApprovalNotGranted` variant with **no route behind either**: a threshold nothing
-- reads is a setting a person can change and watch nothing happen, and an error variant nothing
-- raises is a comment. This migration is the table, and `modules/inventory/src/approvals.rs` is
-- the code that writes to it.
--
-- The rule being enforced: **an absolute adjustment whose size is over the organization's
-- threshold changes nothing until somebody who did not ask for it says yes.** "Absolute" is the
-- operative word and it is why the column is `numeric(14,3)` and not a percent — a stocktake
-- variance of two units out of ten thousand is not a bigger mistake than a variance of two out of
-- ten, and a rule measured in percent would wave the first through and stop the second, which is
-- exactly backwards. The number a person is being asked to approve is therefore the same number
-- they typed.
--
-- ## The pending request holds the whole write, not a reference to it
--
-- `inventory_adjustment_approvals` carries `item_id`, `location_id`, `kind`, `quantity`,
-- `mode`, `reason`, `note` and `source_*` — that is, **the request is the movement, un-applied**.
-- The obvious cheaper design is a `pending` flag on `inventory_movements`, and it is wrong for a
-- reason worth stating: the ledger is append-only and replayable, and a row in it that may or may
-- not count depending on a second table is a row whose meaning changes after the fact. Keeping
-- the un-applied write in its own table means:
--
-- * the ledger still contains only movements that actually happened, so `replay` stays a proof
--   rather than a puzzle;
-- * the request is a *document* with a life of its own — pending, approved, rejected, cancelled —
--   that can be listed, filtered and audited, exactly like `sales_quote_approvals` (0055);
-- * approving replays the request through the **same** `record_movement` the drawer's save button
--   calls, so the two paths cannot compute the stock differently. This is the important one: two
--   write paths for one business rule is how a module ends up with two answers.
--
-- ## One open request per item × location
--
-- A partial unique index over the open state, for the same reason `0055` has one per quote: an
-- operator who clicks save twice must not leave a manager with two rows for one physical
-- recount, and the second request is refused with the first one in the error rather than silently
-- deduplicated. Decided rows stay forever — the audit trail has to answer "who asked, who
-- decided and what did they say" long after the stock has moved on.
--
-- ## A rejection carries its reason
--
-- `decision = 'rejected'` requires a non-blank comment, enforced by a check constraint, because
-- the operator whose adjustment was refused is the one who has to fix it and cannot guess why.
--
-- ## The threshold is a snapshot, not a reference
--
-- `threshold` is copied onto the request. Lowering the threshold afterwards must not rewrite what
-- the approver was actually shown — the same rule `sales_quote_approvals.threshold_percent`
-- follows, for the same reason.
--
-- Nothing here deletes anything, and nothing here writes to `inventory_stock` or
-- `inventory_movements`: approving calls the module's single write path.

create table inventory_adjustment_approvals (
    id                uuid           primary key default gen_random_uuid(),
    organization_id   uuid           not null references organizations (id) on delete cascade,
    item_id           uuid           not null references inventory_items (id) on delete cascade,
    location_id       uuid           not null references inventory_locations (id) on delete cascade,
    requested_by      uuid           not null references users (id) on delete cascade,
    -- The write being asked for, held whole. See the note above: this table IS the un-applied
    -- movement, so approving replays these exact values rather than re-deriving them from a
    -- form the requester has since edited.
    kind              text           not null,
    -- `delta` (the number is the change) or `counted` (the number is the new on-hand). The mode
    -- travels with the request because a counted adjustment cannot be turned into a delta after
    -- the fact: the two are different arithmetic against a different starting number.
    mode              text           not null default 'delta',
    quantity          numeric(14,3)  not null,
    reason            text           not null,
    note              text           not null default '',
    source_kind       text,
    source_id         uuid,
    -- The number the approver is being asked to allow, and the line it was measured against.
    -- Both are snapshots; see the note above.
    amount            numeric(14,3)  not null,
    threshold         numeric(14,3)  not null default 0,
    -- The on-hand the request was measured from, so an approver can see whether the shelf has
    -- moved since the request was raised without opening the stock list.
    on_hand_at_request numeric(14,3) not null default 0,
    status            text           not null default 'pending',
    decision          text,
    decided_by        uuid           references users (id) on delete set null,
    decided_at        timestamptz,
    comment           text,
    -- The movement that the approval produced. Kept so the inbox can link to the ledger row
    -- without replaying the request, and null while the request is still waiting.
    movement_id       bigint,
    cancelled_at      timestamptz,
    created_at        timestamptz    not null default now(),
    updated_at        timestamptz    not null default now(),

    constraint inventory_adjustment_approvals_status_check check (
        status in ('pending', 'approved', 'rejected', 'cancelled')
    ),
    constraint inventory_adjustment_approvals_decision_check check (
        decision is null or decision in ('approved', 'rejected', 'cancelled')
    ),
    -- A decision is a decision, not an edit: the row can never say "approved" without saying
    -- who approved it and when.
    constraint inventory_adjustment_approvals_decided_check check (
        (decision is null and decided_by is null and decided_at is null)
        or (decision is not null and decided_by is not null and decided_at is not null)
    ),
    -- The rejection carries its reason. A manager who says no without saying why makes the
    -- operator guess, and the operator is the one who has to redo the count.
    constraint inventory_adjustment_approvals_rejection_comment check (
        decision <> 'rejected' or (comment is not null and length(btrim(comment)) > 0)
    ),
    -- The whole point of the table: a request over the line it was measured against. An approval
    -- raised by somebody who already holds `inventory.adjustment.approve` is a different route
    -- and does not come through here, so nothing under the line can land in this table.
    constraint inventory_adjustment_approvals_over_threshold check (amount > threshold),
    constraint inventory_adjustment_approvals_amount_positive check (amount > 0),
    constraint inventory_adjustment_approvals_mode_check check (mode in ('delta', 'counted')),
    constraint inventory_adjustment_approvals_quantity_nonzero check (quantity <> 0),
    constraint inventory_adjustment_approvals_note_length check (length(note) <= 2000),
    constraint inventory_adjustment_approvals_comment_length check (comment is null or length(comment) <= 2000)
);

-- One open request per item × location: see the note in the header. `cancelled_at` is excluded
-- alongside the decided states so a withdrawn request does not block a fresh one.
create unique index inventory_adjustment_approvals_open_idx
    on inventory_adjustment_approvals (organization_id, item_id, location_id)
    where status = 'pending';

-- The inbox's two access patterns: everything still waiting, and the history of one item.
create index inventory_adjustment_approvals_org_created_idx
    on inventory_adjustment_approvals (organization_id, created_at desc);
create index inventory_adjustment_approvals_item_idx
    on inventory_adjustment_approvals (item_id, created_at desc);
