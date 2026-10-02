-- Omnion · 0226 · AI app builder: decisions carry their reason
--
-- 0224 wrote the plan store; this migration writes what a *person* does to a plan, and the gap
-- between the two is exactly what the review screen (docs/requests/REQ-045, slice 2) renders.
--
-- **Why a second migration rather than an edit to 0224.** Released migrations are append-only
-- (docs/05-VERSIONING.md) and 0224 is already in every database that ran it — editing its
-- checks would leave those databases believing they were migrated when they were not.
--
-- Two columns and one shared rule. The rule is deliberately ONE-DIRECTIONAL, and getting it
-- backwards breaks every populated database:
--
--     a reason exists  ->  the row was rejected
--     a row was rejected  ->  a reason exists          <-- NOT this, and not because it
--                                                             would be untidy
--
-- `supersede_artifact` retires the artifact a regeneration replaced by setting it to
-- `rejected`, and it can honestly say why: "superseded by a regeneration". `supersede_plan`
-- does the same to a whole plan. So a `rejected` row with no reason is a **real state** — a
-- machine retirement rather than a reviewer's decision — and the equality form of the check
-- refuses exactly those rows. It would also have failed to apply at all on any database that
-- had ever regenerated anything, which is this repository's own lesson twice over (a migration
-- proven only on a fresh database cannot fail the way a migration in production fails).
--
-- What "a reviewer must give a reason" means is therefore a **store** rule, not a column
-- rule: `reject_artifact` and `reject_plan` refuse an empty reason, and the machine paths
-- write `null`. The column records what happened; the store decides who owes an explanation.

alter table app_builder_artifacts
    add column rejected_reason text;

alter table app_builder_artifacts
    add constraint app_builder_artifacts_rejection_check
    check (rejected_reason is null or status = 'rejected');

alter table app_builder_plans
    add column decision_reason text;

alter table app_builder_plans
    add constraint app_builder_plans_decision_check
    check (decision_reason is null or status = 'rejected');

comment on column app_builder_artifacts.rejected_reason is
    'Why this artifact was refused or retired. Written by a reviewer''s rejection and null when a regeneration superseded it; never set on a row that is not rejected.';
comment on column app_builder_plans.decision_reason is
    'Why the whole plan was rejected. Null when a fresh attempt superseded it, never set on a plan that is not rejected.';