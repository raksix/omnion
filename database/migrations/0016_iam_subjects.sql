-- Subjects, scopes and machine identities (REQ-006, slice 2).
--
-- The subject columns arrived with `0011_iam_advanced.sql` beside `user_id` (expand-then-contract);
-- this migration completes the expansion: `user_id` becomes optional so a binding can attach to a
-- group or a service account, the transitional trigger only backfills while `user_id` is present,
-- and the liveness rule is re-keyed on the subject (and the resource a binding names).
--
-- Additive and safe to apply on a populated database: every existing row keeps its `user_id`, the
-- backfill has already filled `subject_id`, and the replaced unique index accepts every row the
-- old one accepted.

-- A binding belongs to a person, a group or a machine identity (docs/07-IAM.md §9, §14).
alter table role_bindings alter column user_id drop not null;

-- The shim now completes a row only when the writer still speaks `user_id`; writers that set the
-- subject themselves are left alone (a group binding carries no `user_id`).
create or replace function role_bindings_fill_subject() returns trigger
language plpgsql as $$
begin
    if new.subject_id is null and new.user_id is not null then
        new.subject_id := new.user_id;
    end if;
    return new;
end $$;

-- Liveness is unique per subject, not per account: the same role may be bound to a user, to a
-- group they belong to and to a service account, and each binding stands on its own. Resource
-- bindings add the resource id to the key, so `/blog/*` and `/legal/*` can both carry it.
drop index role_bindings_active_key;
create unique index role_bindings_active_key on role_bindings (
    role_id,
    subject_type,
    subject_id,
    scope_type,
    coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid),
    coalesce(site_id, '00000000-0000-0000-0000-000000000000'::uuid),
    coalesce(resource_id, '')
) where revoked_at is null;

-- Listing bindings of one subject at one scope is the hot read of the guard, the members tab and
-- the simulator.
create index role_bindings_subject_live_idx on role_bindings (subject_type, subject_id, scope_type)
    where revoked_at is null;

-- Temporary bindings stop counting when they run out; the simulator and the overview read the
-- soonest ones.
create index role_bindings_expires_live_idx on role_bindings (expires_at)
    where revoked_at is null and expires_at is not null;
