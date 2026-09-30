-- Omnion · 0174 · AI workflow builder: the draft store
--
-- "If an invoice is 7 days overdue, email the customer; if 14 days, create a task for the
-- sales owner" produces the workflow directly (docs/requests/REQ-046). This table is the
-- lifecycle that answer travels through: a prompt becomes a row in `generating`, a validated
-- definition turns it into a `draft`, a person decides (`approved` / `rejected`), and the
-- workflow it became is recorded so an activation can be reported back as `activated`.
--
-- One table and no run-side schema on purpose. `workflows.steps` is already jsonb, so
-- approval is a plain insert through the same store the workflow surface uses, and
-- `ai.prompt` is a registry action rather than a step kind. Released migrations are
-- append-only (docs/05-VERSIONING.md).

-- One generated-workflow draft.
create table ai_workflow_drafts (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        not null references organizations (id) on delete cascade,
    site_id          uuid        references sites (id) on delete set null,
    title            text        not null,
    prompt           text        not null,
    -- The model's own explanation, markdown. `null` until an answer validated.
    rationale        text,
    -- The definition the workflow API already accepts: `{ trigger, steps }`. `null` while
    -- the row is still `generating`, so "no answer yet" and "an answer of `null`" cannot
    -- be confused by a reader.
    definition       jsonb,
    status           text        not null default 'generating',
    -- The workflow approval materialised; `null` for every status but `approved`/`activated`.
    workflow_id      uuid        references workflows (id) on delete set null,
    -- Frozen at generation: the model the draft was written by, so a registry change later
    -- cannot silently rewrite what a decision was made about.
    model_key        text,
    tokens_input     integer,
    tokens_output    integer,
    error            text,
    -- The revision prompt an operator sent when asking for changes; kept beside the answer
    -- it produced rather than in a separate trail, because a draft that cannot say what it
    -- was asked to change is a review screen with nothing to review.
    revision_note    text,
    revision_count   integer     not null default 0,
    created_by       uuid        references users (id) on delete set null,
    decided_by       uuid        references users (id) on delete set null,
    decision_reason  text,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    decided_at       timestamptz,
    constraint ai_workflow_drafts_status_check
        check (status in ('generating', 'draft', 'approved', 'activated', 'rejected', 'failed')),
    constraint ai_workflow_drafts_title_check check (length(btrim(title)) between 1 and 120),
    constraint ai_workflow_drafts_prompt_check check (length(btrim(prompt)) between 1 and 4000),
    -- A definition is an object or it is nothing: the engine deserialises it into
    -- `WorkflowDefinition`, and a scalar would fail there with a message about the shape of
    -- JSON rather than about the field that is wrong.
    constraint ai_workflow_drafts_definition_check
        check (definition is null or jsonb_typeof(definition) = 'object'),
    -- `decided_at` belongs to a decision and to nothing else. Without this a rejected draft
    -- could carry no reason with a decided timestamp, and the review screen's "rejected on"
    -- line would read as a bug rather than as the gap it is.
    constraint ai_workflow_drafts_decision_check
        check (decided_at is null or status in ('approved', 'activated', 'rejected'))
);

-- The console's own list: newest first, one organization only.
create index ai_workflow_drafts_org_idx
    on ai_workflow_drafts (organization_id, created_at desc);

-- The drafts an operator still has to do something about. A partial index, because the list
-- screen asks for open drafts far more often than it asks for decided ones, and `rejected`
-- rows grow without bound.
create index ai_workflow_drafts_open_idx
    on ai_workflow_drafts (status, updated_at desc)
    where status in ('generating', 'draft', 'approved');

-- One draft per materialised workflow. A second draft approved onto the same workflow would
-- make "which draft produced this rule?" answerable two ways.
create unique index ai_workflow_drafts_workflow_key
    on ai_workflow_drafts (workflow_id)
    where workflow_id is not null;

-- Token counts are never negative: a provider that reports a negative count would otherwise
-- be summed into a budget as if it were a spend, and `null` (did not report) stays distinct
-- from `0` (reported nothing).
alter table ai_workflow_drafts
    add constraint ai_workflow_drafts_tokens_check
    check ((tokens_input is null or tokens_input >= 0)
       and (tokens_output is null or tokens_output >= 0));
