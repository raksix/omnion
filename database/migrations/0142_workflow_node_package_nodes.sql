-- 0055_workflow_node_package_nodes.sql — what a package installed, recorded on the ledger row.
--
-- REQ-087, slice 4. `0053_workflow_credentials.sql` created `workflow_node_packages` with the
-- REQ's own columns — key, version, source, checksum, permissions, enabled — and slice 2
-- wired a read path and a placeholder install. Slice 4 makes the install validate, and the
-- first thing that needs a column is the one the REQ's *removal* sentence depends on:
--
--     "removal disables them and flags dependent workflows instead of breaking them"
--
-- To flag a dependent workflow, the remover has to know which nodes the package owned. The
-- manifest that answers that belongs to whoever installed it — the platform keeps a checksum,
-- not a copy — so the keys have to be recorded at install time or the removal can only say
-- "something used this". A remover that cannot name what it broke cannot honour "instead of
-- breaking them": the person gets a generic warning and no way to act on it.
--
-- `node_keys` is therefore a column, and it is the *namespaced* set the registry actually
-- holds (`package.node`), not the manifest's local names. The distinction is the whole reason
-- to store it: a workflow's graph names `acme.echo`, and a remover matching on `echo` finds
-- nothing and reports a clean removal over a broken workflow.
--
-- Two decisions worth stating:
--
-- 1. **It is an array with a check, not a free-form object.** A second shape here would be a
--    second thing for the remover to read, and a remover that reads two shapes is a remover
--    that is right about one of them.
-- 2. **It is not a foreign key and not a live reference.** The node definitions live in the
--    in-process registry, not in this database, so nothing here can be integrity-checked
--    against them — a check constraint can only say the shape. Storing names is honest
--    bookkeeping; claiming they are verified would not be.

alter table workflow_node_packages
    add column if not exists node_keys jsonb not null default '[]'::jsonb;

-- The shape is checked here because the remover is the one consumer and a row it cannot read
-- is a row it will skip silently.
alter table workflow_node_packages
    drop constraint if exists workflow_node_packages_node_keys_is_array;

alter table workflow_node_packages
    add constraint workflow_node_packages_node_keys_is_array
    check (jsonb_typeof(node_keys) = 'array');

-- A node key is `package.node`: a lower-case local name, a dot, a lower-case local name.
--
-- This cannot be a `check` constraint, and finding out why is worth the note: PostgreSQL
-- forbids a subquery in a CHECK, and "does every element of this array match a pattern" is
-- inherently a subquery. A `jsonb_path_query_array` wrapped in an IMMUTABLE function is the
-- supported way to say it, and the function is immutable because it only reads its own
-- argument. Every other node-key shape check in this schema (workflows, credentials) is a
-- constraint on a *scalar* column, which is why the obvious shape worked there and not here.
--
-- The trade is stated rather than hidden: a constraint on a function is only as good as the
-- function, so the function carries the regex in one place and the check calls it.
create or replace function workflow_node_key_shape_ok(keys jsonb) returns boolean
language sql
immutable
as $$
    select jsonb_path_query_array(
               keys,
               '$[*] ? (@.type() != "string" || !(@ like_regex "^[a-z0-9][a-z0-9_-]{0,63}\\.[a-z0-9][a-z0-9_-]{0,63}$" flag "i"))'
           ) = '[]'::jsonb
    or jsonb_typeof(keys) <> 'array';
$$;

comment on function workflow_node_key_shape_ok(jsonb) is
    'True when every element of a node-key array is a namespaced `package.node` key.';

alter table workflow_node_packages
    drop constraint if exists workflow_node_packages_node_key_shape;

alter table workflow_node_packages
    add constraint workflow_node_packages_node_key_shape
    check (workflow_node_key_shape_ok(node_keys));

-- The remove path reads the keys of a live row; the library screen reads them alongside the
-- row's own state chip. Both are per-organization reads of one row each, so the partial index
-- on live rows is the one that matters.
create index if not exists workflow_node_packages_live_idx
    on workflow_node_packages (organization_id, key)
    where removed_at is null;
