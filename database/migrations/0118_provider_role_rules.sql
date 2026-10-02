-- Omnion · 0118 · The role rules get a table of their own (REQ-065, slice 3).
--
-- Slice 2 gave the attribute map rows (0117) rather than another blob inside `config`. The role
-- rules have the same argument and a stronger version of it: the rules are *ordered* and the
-- order is the semantics ("first match wins"). A JSON array keeps its order, so that alone is not
-- the reason — the reason is that a rule set is read on the sign-in path and written from an
-- editor, and the two disagree exactly where it hurts: an editor that writes the array back
-- re-serialises it, and anything that ever reorders a JSON object key silently changes which
-- role a colleague gets. Rows have an explicit `position`, so order is a column and a drag.
--
-- Why each column is the shape it is:
--
-- * `when_kind` is a closed set, not a free-form expression. `claim` / `group` / `department` /
--   `title` / `always` are the four things the identity actually carries, and a rule language that
--   grows an expression evaluator is a rule language with an injection surface.
-- * `when_operator` is closed for the same reason, and `regex` is included but *validated at save
--   time* — a rule that does not compile is a sign-in that silently falls through to the default
--   role, which is the quietest possible way to hand out the wrong access.
-- * `role_id` is a foreign key to `roles`, so a deleted role cannot leave a rule pointing at
--   nothing. `scope_type` + `site_id` mirror `role_bindings.scope_type` / `site_id` exactly: the
--   rule predicts a *binding*, so it must be able to say the same two things a binding can.
-- * `stop` is separate from `enabled` on purpose. A disabled rule is skipped; a matching rule with
--   `stop` ends the search; a matching rule without it lets a later, broader rule still be
--   considered. Collapsing the two would make "temporarily off" and "stop here" the same edit.
--
-- `site_id` gets no foreign key here, for the same reason `role_bindings.site_id` does not: the
-- sites table is created by the tenancy migration and the check below carries the shape rule that
-- a foreign key would otherwise express. A site-scoped rule without a site is refused by the
-- constraint rather than by the application, because a rule that stores an unbound uuid is a rule
-- that matches and grants nothing.

create table provider_role_rules (
    id uuid primary key default gen_random_uuid(),
    provider_id uuid not null references auth_providers (id) on delete cascade,
    position integer not null default 0,
    when_kind text not null default 'claim'
        check (when_kind in ('claim', 'group', 'department', 'title', 'always')),
    when_key text not null default '',
    when_operator text not null default 'equals'
        check (when_operator in ('equals', 'contains', 'starts_with', 'regex')),
    when_value text not null default '',
    role_id uuid not null references roles (id) on delete cascade,
    scope_type text not null default 'organization'
        check (scope_type in ('organization', 'site')),
    site_id uuid,
    stop boolean not null default false,
    enabled boolean not null default true,
    created_at timestamptz not null default now(),
    -- `always` is the catch-all and takes no key; every other kind must name what it reads.
    constraint provider_role_rules_always_shape check (
        (when_kind = 'always' and when_key = '' and when_value = '')
        or (when_kind <> 'always' and when_key <> '')
    ),
    -- A rule that names a site must carry the site, and one that does not must not.
    constraint provider_role_rules_scope_shape check (
        (scope_type = 'organization' and site_id is null)
        or (scope_type = 'site' and site_id is not null)
    )
);

-- The evaluator reads one provider's rules in order; the dry run reads the same rows.
create index provider_role_rules_provider_idx
    on provider_role_rules (provider_id, position);

-- Two rules cannot occupy the same slot: a tie would be broken by an unspecified order, and the
-- whole point of the feature is that the order is what the operator wrote.
create unique index provider_role_rules_position_idx
    on provider_role_rules (provider_id, position);
