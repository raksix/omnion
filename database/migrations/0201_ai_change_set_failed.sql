-- 0201_ai_change_set_failed.sql — REQ-101 slice 3b: the `failed` status and the editor's stamp.
--
-- Two changes, both of them things migration 0189 could not have known:
--
-- 1. **`failed`.** 0189 shipped the table with a closed status check of six values and the
--    `ai.changeset.*` events with three names. Slice 3a made the apply all-or-nothing but left
--    a rolled-back set reading `confirmed`, which is the one status a person cannot act on and
--    cannot tell apart from a set that is about to apply. A terminal `failed` is what makes
--    "nothing happened" observable — the acceptance criterion asks for it by name, together
--    with the failing operation and an `ai.changeset.failed` event.
--
--    The reason is stored in `discarded_reason`, which 0189 already declares. Renaming that
--    column to something honest for both uses would touch a released migration's dependents
--    for no gain: the row's `status` is what every screen filters on, and the column only has
--    to be legible next to it. The check below is what makes the reuse safe — a `failed` row
--    must say why, exactly as a `discarded` one must.
--
-- 2. **`updated_by`.** "Editing a change set records the editor and time" is a box in the
--    request, and 0189 has `created_by` and no `updated_by`, so the row cannot answer "who
--    last touched this". It is a column rather than a table because there is exactly one
--    writer (`PATCH /ai/change-sets/{id}`) and the answer is read on the same row. `set null`
--    on a deleted user, matching `created_by`: a change set outlives the person who edited it,
--    and the operations it carries are the record, not the name.
--
-- `drop constraint … add constraint` rather than `drop … if exists … add`: the constraint has
-- to be replaced on every installation, and `add constraint` on a name that still exists
-- fails loudly rather than silently keeping the old one. The whole file is wrapped in a
-- transaction, so a failure here leaves the six-value check in place.

begin;

alter table ai_change_sets
    drop constraint ai_change_sets_status_known;

alter table ai_change_sets
    add constraint ai_change_sets_status_known
    check (status in ('draft', 'pending', 'confirmed', 'applied', 'discarded', 'expired', 'failed'));

-- A `failed` set says why, on the same grounds a discarded one does: an unexplained failure is
-- indistinguishable from a set that is still waiting, and the reviewer cannot tell the two
-- apart from the list alone.
alter table ai_change_sets
    add constraint ai_change_sets_failed_has_reason
    check (status <> 'failed' or (discarded_reason is not null and char_length(btrim(discarded_reason)) > 0));

alter table ai_change_sets
    add column if not exists updated_by uuid references users (id) on delete set null;

-- The list filters on `status`, and `failed` is a status a person looks for, so the existing
-- live index does not cover it: `ai_change_sets_live_idx` is a partial index over
-- `('draft','pending')` only, which is right for "what needs attention" and wrong for "what
-- broke". This one is the second, smaller index and it is what the status filter's third tab
-- reads.
create index if not exists ai_change_sets_failed_idx
    on ai_change_sets (organization_id, updated_at desc)
    where status = 'failed';

commit;
