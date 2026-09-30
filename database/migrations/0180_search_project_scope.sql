-- Project-scoped search: the index learns which project a document belongs to.
--
-- REQ-133 acceptance: "Global search (REQ-002) returns only resources the caller may see,
-- scoped to their projects, verified with two accounts."
--
-- The gap this closes is not a missing filter but a missing *fact*. `search_documents` carried
-- `organization_id` and `site_id` from its first migration and has never carried which project a
-- row belongs to, so the engine had nothing to narrow by: a search narrowed by provider and
-- organization answered with a workflow from a project the caller is not a member of, because
-- that row's organization was their own. Two people in ONE organization, in different projects,
-- both passed every check the query made.
--
-- The column is nullable and deliberately not constrained to `automation_projects`: pages, media,
-- users and sites belong to no project, so a non-null constraint would make four of the seven
-- providers unindexable. `null` therefore means "belongs to no project", and the read clause is
-- written so that a row without a project is *not* reachable through a project filter — the
-- direction that fails closed rather than the one that fails open.
--
-- The index is partial (`where project_id is not null`) because every provider that does not
-- belong to a project writes null there, and an index over mostly-null columns is a write
-- amplifier on the one hot table in the engine.

alter table search_documents
    add column project_id uuid references automation_projects (id) on delete cascade;

create index search_documents_project_idx on search_documents (project_id)
    where project_id is not null;

comment on column search_documents.project_id is
    'Project that scopes this document (REQ-133). Null for the domains that belong to no project '
    '(pages, media, users, sites): those are organization-scoped, not project-scoped.';
