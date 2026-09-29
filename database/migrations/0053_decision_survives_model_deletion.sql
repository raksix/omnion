-- REQ-098 · slice 4 — let a decision survive the deletion of the model it names.
--
-- Slice 3 shipped two rules that cannot both hold, and nothing noticed because each was
-- individually reasonable and the tests never deleted a model that a decision named:
--
--   * `resolved_model_id` / `resolved_provider_id` are `on delete set null`. A decision is a
--     *historical* record: it must survive its model being removed, or the log loses the row that
--     explains a call the operator is still asking about.
--   * `ai_route_decisions_answer_agrees_with_rule` requires a non-`unresolved` row to carry a
--     non-null model. That is a good invariant for a row being *written*.
--
-- The pair is contradictory the moment a model is deleted: the foreign key nulls the column and
-- the check refuses the resulting row, so `PUT /ai/providers/{id}/models` — the ordinary way an
-- operator prunes a model — fails with
-- `new row for relation "ai_route_decisions" violates check constraint`, and **the whole model
-- replacement is rolled back**. The symptom is an unrelated-sounding 500 on a model edit, and it
-- is total: any installation that has ever routed a request cannot replace a provider's model
-- list at all.
--
-- The resolution is to narrow the check to what it can still know, and to keep the historical
-- answer in the column that is designed to hold it.
--
-- The rule becomes: a row that says `unresolved` carries no model, and **that is the only
-- contradiction worth forbidding**. A row whose model was deleted afterwards is no longer a
-- contradiction — it is history, and the `reason` / `walk` columns still say which model it was
-- (`walk[].model_id` is stored as text precisely so it outlives the row it points at).
--
-- The check is dropped and recreated rather than `not valid`: the replacement is satisfied by
-- every row that can already exist (an `unresolved` row has nulls; a resolved row has non-nulls,
-- and after a cascade has nulls, which the new rule allows), so validating is cheap and leaves
-- no unvalidated constraint behind.

alter table ai_route_decisions
    drop constraint if exists ai_route_decisions_answer_agrees_with_rule;

-- The one shape that is always wrong: a row that refused *and* claims a model. The reverse — a
-- model that no longer exists — is history, not an error, and forbidding it is what broke model
-- deletion in the first place.
alter table ai_route_decisions
    add constraint ai_route_decisions_unresolved_carries_no_model check (
        rule <> 'unresolved'
        or (resolved_model_id is null and resolved_provider_id is null)
    );
