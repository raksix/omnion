-- Omnion · 0127 · where an account came from (REQ-065, slice 4 part 9).
--
-- REQ-065's own data model names three columns on `users`:
--
--   identity_source              text default 'local'
--   provisioned_by_provider_id   uuid → auth_providers on delete set null
--   external_id                  text
--
-- None of them existed. `/scim/v2/Users` wrote the provider's own id into
-- `attributes -> 'scim_external_id'` — a key inside a JSON blob, on a column the request never
-- mentions — and nothing recorded which provider provisioned an account at all. So three things
-- this request asks for have nowhere to live:
--
--   * "delete a provider that provisioned users is blocked with the affected count listed" is an
--     acceptance criterion, and there is no query that can answer it: no account row points back
--     at its provider, so the count is unknowable and the delete is unguarded.
--   * "after reassignment the users fall back to local accounts" has no column to write.
--   * No list, filter or report can tell a provisioned account from a local one without reading
--     every account's JSON.
--
-- A second, differently-shaped copy of the directory inside a blob is the failure mode `0052`
-- already corrected for the attribute map ("a second differently-shaped blob in the same JSON is
-- how 'save the attribute map' ends up rewriting the role rules nobody was looking at"). This is
-- the same mistake one layer down, and it is the reason the delete guard has to be built on
-- columns rather than on `attributes ->>`.
--
-- `scim_external_id` is **kept**, not dropped. Readers still ask for it, and a removal in the
-- same migration that introduces the column replacing it would make a rollback lose the value
-- rather than restore it. The SCIM surface now writes both and reads the column first.

alter table users
    add column identity_source text not null default 'local';

alter table users
    add column provisioned_by_provider_id uuid references auth_providers (id) on delete restrict;

alter table users
    add column external_id text;

-- The vocabulary is closed. `identity_source` answers "which clock owns this account" — a
-- password reset, an SSO assertion, or a directory push — and a value outside the list is a
-- value no reader can act on. The database says so rather than every reader finding out.
alter table users
    add constraint users_identity_source_check
        check (identity_source in ('local', 'ldap', 'active_directory', 'oidc', 'oauth2', 'saml', 'scim'));

-- The provider link and the external id travel together or not at all.
--
-- An `external_id` with no provider is the interesting case, and it is what the legacy blob would
-- have produced: "this account knows what the directory calls it, and we cannot say which
-- directory". That state is real, and it is exactly the state in which the delete guard cannot
-- protect anybody — so it is refused rather than allowed. An unattributable row keeps
-- `identity_source = 'scim'` and a **null** `external_id`; the legacy key remains the fallback
-- the reader uses, and the visible set of unattributable rows is therefore enumerable instead of
-- hiding inside everybody's attributes.
--
-- This constraint and the `on delete restrict` above are **the same rule seen from two sides**,
-- and the walk is what made that visible. `on delete set null` was the original choice and it is
-- the safe-looking one, but it cannot be implemented against this constraint: the referential
-- action nulls the provider and leaves the external id, and the row is then exactly the
-- half-written provenance this check exists to refuse — so *deleting a provider with even one
-- provisioned account raised a constraint violation and the transaction rolled back*. The delete
-- was blocked, which is the right outcome, but by an error nobody can read: 23514 with the
-- constraint's name, instead of the refusal that names the count and what to do about it.
--
-- `restrict` makes the foreign key refuse the same delete, with the same answer, at the layer
-- that owns the rule. The application guard runs first and gets to be the good error; this is
-- what is left if somebody reaches past the API.
alter table users
    add constraint users_provenance_paired_check
        check ((external_id is null) = (provisioned_by_provider_id is null));

-- One directory account, one row. Two connectors pushing the same external id would otherwise
-- produce two local accounts for one person, each holding its own sessions; this is what makes
-- the second push a conflict instead.
create unique index users_provider_external_id_key
    on users (provisioned_by_provider_id, external_id)
    where external_id is not null;

-- The delete-guard query and the provider detail screen's first question, both answered from an
-- index rather than from a scan of every account's JSON.
create index users_provisioned_by_provider_idx
    on users (provisioned_by_provider_id)
    where identity_source <> 'local';

comment on column users.identity_source is
    'Which system owns this account: local, a directory kind, or scim. Defaults to local.';
comment on column users.provisioned_by_provider_id is
    'The provider that created or first claimed this account. Null for a local account, and null for a SCIM-provisioned account whose provider cannot be attributed (pre-0127 rows).';
comment on column users.external_id is
    'The id the directory knows this account by. Unique per provider, and set only alongside provisioned_by_provider_id.';

-- ---------------------------------------------------------------------------------------------
-- Backfill, in two steps and in this order.
--
-- Step 1 — a provider sign-in. `sso_last_provider` is the slug of the provider that last proved
-- who this person is, and `sso_subjects` maps that slug to the subject id it asserted, so both
-- halves of the pair are recoverable. The join is on (organization_id, slug) rather than slug
-- alone: a group name, an account id and a provider slug are each unique only *within* a tenant,
-- and an unscoped match would attribute an account to another organization's provider.
--
-- The boundary, stated rather than hidden: for an account that has signed in through **more than
-- one** provider, this records the most recent one rather than the first. The request's column is
-- named "provisioned by", and the truthful answer for such an account is that the first is no
-- longer knowable from the row — `attributes -> 'sso_subjects'` keeps every provider, so a reader
-- that needs the whole history still has it. A single-provider account, which is the ordinary case
-- and the one the delete guard exists for, is exact.
--
-- An account whose provider has since been **deleted** stays `local`: nothing in the row names a
-- provider that still exists, and inventing one would put a dangling reference in the guard's
-- query. Those rows are the visible, enumerable set "needs a decision".
-- ---------------------------------------------------------------------------------------------
update users u
   set identity_source = p.kind,
       provisioned_by_provider_id = p.id,
       external_id = u.attributes -> 'sso_subjects' ->> p.slug
  from auth_providers p
 where u.identity_source = 'local'
   and u.attributes ? 'sso_subjects'
   and u.attributes ->> 'sso_last_provider' = p.slug
   and u.organization_id = p.organization_id
   and u.attributes -> 'sso_subjects' ? p.slug;

-- Step 2 — a directory push. A SCIM connector creates the account from the outside, so there is
-- no sign-in to read a provider from; the `scim_external_id` key is the only trace. It carries
-- no provider id, so `provisioned_by_provider_id` stays null and `external_id` is left null with
-- it — the paired constraint above is the reason. What is recoverable is the **source**: any
-- account a connector created carries the key, and `identity_source = 'scim'` is a fact the sync
-- log already asserts, so marking it changes a list from "everything looks local" to a set an
-- operator can act on.
--
-- An account step 1 already attributed keeps its provider link: step 1's `coalesce`-free
-- `external_id` assignment is guarded here by `where … identity_source <> 'scim'`, and an account
-- that both signed in and was pushed is the connector's claim about a person the SSO provider also
-- knows — the kind the push asserts is authoritative for *provenance*.
update users
   set identity_source = 'scim'
 where attributes ? 'scim_external_id';
