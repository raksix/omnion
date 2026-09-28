-- Omnion · 0031 · the invitation approval queue (REQ-005, slice 3)
--
-- `organization_settings.invite_policy` has been stored, validated and offered in the Settings
-- tab since migration 0030 — but the three *behaviours* it names were never enforced, so the
-- field decorated the panel and bounded nothing. This migration gives the policy teeth:
--
-- * `closed` — refused in the create-invitation path, no schema change needed.
-- * `self_serve` — the route's existing `organizations.manage` guard is the whole rule, which
--   is why it needs no column.
-- * `owner_approval` — the only policy that needs a new state. An invitation raised by somebody
--   who is not the tenant's owner is not handed a working link: it waits in a queue until an
--   owner releases it.
--
-- The queue is a *status*, not a second table. An invitation is a pending invitation that has not
-- been released yet, and keeping it in `organization_invitations` means the existing unique
-- index on `(organization_id, lower(email)) where status = 'pending'` still refuses a duplicate
-- live invitation — a queued one and a released one cannot both sit for the same address.
--
-- `is_usable()` already requires `status = 'pending'`, so a queued token answers the preview and
-- the acceptance path as unusable without either handler learning about the queue. The token is
-- still returned to the owner *when it is released*, and only then: before that there is no
-- working link to leak.
--
-- Additive by design (docs/05-VERSIONING.md): a constraint is widened (never narrowed) and two
-- nullable columns are added. Existing rows are untouched — every current invitation is
-- `pending`, `accepted`, `revoked` or `expired` and keeps its meaning.

alter table organization_invitations
    drop constraint organization_invitations_status_check;

alter table organization_invitations
    add constraint organization_invitations_status_check
        check (status in ('pending', 'awaiting_approval', 'accepted', 'revoked', 'expired'));

-- Who released or declined a queued invitation, and when. Both are null for every invitation
-- that never waited in the queue, so a `pending` invitation proves nothing was decided.
alter table organization_invitations
    add column decided_by uuid references users (id) on delete set null;

alter table organization_invitations
    add column decided_at timestamptz;

-- The Members tab's queue is a filtered listing of this one state, and the filter is the
-- organization's own so the index has to carry the status column.
create index organization_invitations_queue_idx
    on organization_invitations (organization_id, created_at desc)
    where status = 'awaiting_approval';
