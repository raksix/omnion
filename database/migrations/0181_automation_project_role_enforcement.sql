-- REQ-133 acceptance 6 — a project member's ROLE has to mean something, and it has to mean it on
-- the very next request, in both directions.
--
-- ## The defect this migration answers half of
--
-- `automation_project_members.role` has carried the matrix (`owner`, `editor`, `operator`,
-- `viewer`) since 0164, `ProjectRole::can_edit` / `can_run` / `can_manage_credentials` have been
-- unit-tested since slice 1, and the *members screen* renders all four roles. What did not exist
-- anywhere on this branch was a **write path that asked the role**: `PUT /workflows/{id}`,
-- `POST /workflows/{id}/run`, `DELETE /workflows/{id}` and `POST /automations` all reached the
-- store through `workflow_in_scope`, which answers one question — "may this caller SEE the
-- workflow" — and returns the row. So a `viewer` could rewrite a definition and an `operator`
-- could delete one, and no test could see it: every assertion in the suite was about membership
-- existing, never about what a role is allowed to do.
--
-- That is the tenth shape of the same defect this branch keeps meeting (the function answers the
-- right question and nothing calls it), except here the question was right and the CALLER was
-- missing — so the honest half of this file is the one that is not SQL.
--
-- ## What is SQL here, and why it is SQL
--
-- Two facts, and neither belongs in Rust:
--
-- 1. **`automation_projects.default_member_role`.** "Which role does a caller get in the
--    organization's default project when they are not a member of it?" was decided nowhere: it
--    was decided *implicitly*, by the absence of a check. A column makes the decision readable,
--    changeable by a deployment, and — the reason it is a column and not a constant — testable
--    against a project that is not the default. `viewer` is the value: the default project is
--    where an organization's automations accumulate, and granting run-or-edit to every account in
--    the tenant because they have not been given a role yet is exactly the delegation leak
--    REQ-133's own risk note warns about ("delegation must be narrower than it feels").
--
-- 2. **The `viewer` / `operator` grants are not inferred from membership.** A `select` on the
--    membership row is already the join; what was missing was the *capability* question, and
--    that one lives in Rust where the matrix is defined once (`ProjectRole`). This file
--    deliberately does NOT duplicate the matrix as a constraint — a check constraint and a Rust
--    constant are written twice and nothing keeps them in agreement, which is the same trap the
--    `automation_projects_default_is_active` constraint and the archive handler already sit on.
--
-- ## Backfill
--
-- Every organization that already had a default project gets the column's default for free
-- (`viewer`), so this migration cannot change the behaviour of an existing installation
-- silently: nobody who could not edit a project yesterday could not edit it this morning, because
-- nobody could edit it at all — the guard did not exist. What changes is the *new* rule's
-- visibility: a caller with no membership row in a project they can see now answers with the
-- project's default role rather than with "owner".

alter table automation_projects
    add column default_member_role text not null default 'viewer';

-- The same shape as the role column on the membership table: a check constraint so a bad role
-- can never reach the table, whatever writes to it. `ProjectRole::parse` is the other copy of
-- this list, and it is the one that reads it back — a value this constraint allows but
-- `ProjectRole` cannot parse would answer `None` for "what role is this", which is the silent
-- half of a permission bug.
alter table automation_projects
    add constraint automation_projects_default_member_role_valid
        check (default_member_role in ('owner', 'editor', 'operator', 'viewer'));

-- The read that every capability check now performs: one row, one organization, one project.
-- Nothing in the hot path can use an index for it — the hot path already holds the project row
-- it was called with — so this index is deliberately absent. Adding one to a two-column lookup
-- that is served from the same row is a write cost for a read that is already free.
--
-- ── deliberately NOT here ──
--
-- * A "membership revision" column. The REQ's risk note proposes one ("the cache key includes a
--   membership revision"), and the migration has no way to add it: **nothing on this branch
--   caches a project role.** `crates/permissions/src/groups.rs` says it outright — "resolution
--   never caches" — and `role_of` is a single indexed primary-key lookup
--   (`automation_project_members` pk `(project_id, user_id)`), which is the fastest possible
--   answer and is re-run per request by construction. A revision counter here would be a number
--   incremented by the membership writers and read by nobody, which is the greenest possible lie
--   the branch has not yet paid for. The gate (`scripts/qa/run-project-role-freshness.sh`) proves
--   the property that makes a revision unnecessary: **the next request re-reads the row**, in
--   both directions, with no cache to invalidate between them.
-- * An audit trigger. Membership changes already write an audit row from the route
--   (`automation.project.member.set` / `.removed`), and the audit trail is filtered by project —
--   adding a database trigger would write the same fact twice.
