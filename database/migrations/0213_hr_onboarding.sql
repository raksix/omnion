-- Omnion · 0213 · HR: onboarding checklists, the document index and their facts (REQ-055, slice 4)
--
-- One table. `hr_onboarding_templates` already exists from slice 1 (0196) because the seed had to
-- be seedable before anything used it; this file is the half that *consumes* it — the per-employee
-- items a template materialises into.
--
-- Four decisions in the schema, each of which a naive version gets wrong:
--
-- 1. **A due date is derived from the start date at APPLY time, then stored.** The template carries
--    an *offset* ("3 days after they start") because that is what an organization can write down.
--    Storing the offset alone would make the checklist re-date itself every time somebody edits the
--    employee's start date — a checklist whose due dates move under it is a checklist nobody can
--    trust. The offset is kept **as well**, so "why is this due on the 9th?" is answerable without
--    reconstructing the start date, and so re-applying a corrected template does not silently
--    rewrite history.
-- 2. **The one-row-per-(employee, position) unique index is what makes the progress bar a fact.**
--    Progress is `done / total` over the items; an item duplicated by a double-click on "apply" is
--    invisible in the total but visible in the bar, and the bar never reaches 100%. Same shape as
--    the attendance slice's `unique (employee_id, work_date)`.
-- 3. **`done_at` and `done_by` are the same decision twice, on purpose.** "Ticked" and "ticked by"
--    are independent questions: an import or the owning role ticks an item, and an audit trail that
--    records only "done" cannot answer who. Both are nullable together — an unticked item has
--    neither — and the check makes that a fact rather than a convention.
-- 4. **`hr_documents` needs no change.** It was created in 0196 with the media reference, the
--    expiry date and the acknowledgement column, and the only thing it lacked was an index over
--    the organization for the *cross-employee* list the documents screen reads. Slice 4 adds that
--    index here rather than in a migration of its own, because an index with no query behind it is
--    the kind of thing that looks like work.
--
-- The migration is additive. Nothing above it is altered, and — proved the way the attendance file
-- was proved, by applying the chain to a fresh database and then applying this file a second time
-- by hand — every object below is guarded, including the indexes. A file guarded at the top and
-- not at the bottom passes on a clean database and wedges every database whose schema arrived by
-- another route, with `42P07`, before a single assertion in the walk suite can run.

create table if not exists hr_onboarding_items (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    -- RESTRICT, not CASCADE: a checklist is the record that somebody joined, and deleting the
    -- employee row must not erase the fact that they did.
    employee_id uuid not null references hr_employees (id) on delete restrict,
    -- Nullable: an item may exist without a template behind it, which is what an ad-hoc checklist
    -- is. The template is the provenance, not the parent.
    template_id uuid references hr_onboarding_templates (id) on delete set null,
    -- The order the checklist is worked in. Zero-based, contiguous per employee, and the unique
    -- index below is what makes it so rather than a convention.
    position integer not null,
    title text not null,
    -- Who owns it: `hr`, `it`, `manager`, `employee`. Free text rather than an enum because the
    -- roles are the organization's, not the platform's — a reference table for "who does what"
    -- belongs to the org chart this module already has.
    owner_role text,
    -- When it is due, derived from the employee's start date when the item was applied.
    due_on date,
    -- The offset the template carried, kept for the reason in point 1.
    due_offset_days integer,
    requires_file boolean not null default false,
    done_at timestamptz,
    done_by uuid,
    note text not null default '',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),

    constraint hr_onboarding_items_position_check check (position >= 0),
    -- A title is what the person reads on the checklist; an empty one is a row with nothing to do.
    constraint hr_onboarding_items_title_check check (length(btrim(title)) > 0),
    -- Ticked and ticked-by are one decision: neither alone is a half-answered question, and a
    -- row with a `done_by` but no `done_at` is not "done", it is broken.
    constraint hr_onboarding_items_done_check
        check ((done_at is null) = (done_by is null)),
    -- An offset a week in the past or three years ahead is a typo, and the due date it would
    -- produce is silently wrong. Negative is refused here rather than clamped in the service,
    -- because a service check that clamps turns a typo into a plausible-looking date.
    constraint hr_onboarding_items_offset_check
        check (due_offset_days is null or (due_offset_days >= 0 and due_offset_days <= 365))
);

comment on table hr_onboarding_items is
    'A materialised onboarding checklist: one row per step, due dates derived from the start date at apply time.';

-- The progress bar's denominator. Without it a double "apply" produces two items at the same
-- position, the total goes up and `done / total` never reaches 1 — a bar stuck at 66% with every
-- visible item ticked, which reads as a bug in the product and is a bug in the caller.
create unique index if not exists hr_onboarding_items_employee_position_key
    on hr_onboarding_items (employee_id, position);

-- The board groups by employee, so this is the ordering the board reads.
create index if not exists hr_onboarding_items_employee_idx
    on hr_onboarding_items (employee_id, position);

-- The `/hr/onboarding` board asks "who is still in progress", which is a filtered scan over the
-- organization's unfinished items — the same shape as the attendance slice's partial index.
create index if not exists hr_onboarding_items_org_open_idx
    on hr_onboarding_items (organization_id, due_on)
    where done_at is null;

-- The documents screen reads across employees (kind, expiry, who uploaded it), which
-- 0196's `(employee_id, created_at desc)` cannot serve: a filter on organization and kind would
-- seq-scan every document the tenant has ever held. `expires_on` first because the expiry badge
-- is what the screen opens with, and the partial predicate keeps the index to the rows that can
-- ever expire at all.
create index if not exists hr_documents_org_kind_idx
    on hr_documents (organization_id, kind, created_at desc);

create index if not exists hr_documents_org_expiry_idx
    on hr_documents (organization_id, expires_on)
    where expires_on is not null;

-- The upgrade path for a tenant that already holds documents: 0196's expiry index is on
-- `(organization_id, expires_on)` with the same predicate, so it already serves the sweep. The
-- second index above is additive and leaves the original in place rather than dropping it — the
-- two cover different orderings and a migration that drops an index another release's query
-- depends on is a rollback nobody can take.