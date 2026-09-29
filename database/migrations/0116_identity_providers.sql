-- Omnion · 0116 · The provider registry becomes directory-aware (REQ-065, slice 1).
--
-- `0011_iam_advanced.sql` carries `auth_providers` and REQ-006 shipped the OIDC, OAuth2 and
-- SAML half of it (`crates/identity/src/sso`). This migration widens the **registry row** and
-- nothing else; the mapping, rule and sync-run tables that REQ-065 also asks for take their own
-- migration numbers in their own slices, because the ledger is append-only and a slice that
-- lands tables nobody reads yet is a table nobody migrates.
--
-- What widens, and why each column is here rather than derived at query time:
--
-- * `kind` — `ldap` and `active_directory` join the three protocol kinds. A `check` cannot be
--   altered in place, so the old one is dropped and a wider one takes its place; the column type
--   and every existing row are untouched, which is what docs/05-VERSIONING.md means by additive.
-- * `last_test_at` / `last_test_ok` — the enable gate. `last_test_ok is null` is *never tested*,
--   which is a third state distinct from *tested and failed*, and a provider has to be able to
--   sit in it while an operator fills the form in. Without a stored answer the gate would have
--   to re-run the test on every `POST /enable`, which turns a checkbox into a network round trip
--   to somebody else's server and fails for reasons that have nothing to do with the operator.
-- * `last_sync_*` and `sync_interval_minutes` — the sync column of the list screen. A protocol
--   provider leaves them null, and the screen reads one shape rather than branching per kind.
-- * `plugin_key` — an installed plugin may declare its own provider kind. The key is stored so a
--   plugin removal can find the rows it owns; every platform kind leaves it null.
--
-- Nothing here holds a secret. The bind password and the client secret are both named by a
-- reference and resolved from the environment by the caller, never stored in a row.

-- The kind list widens. Dropped and re-created because `alter table ... drop constraint` is the
-- only way a `check` moves, and the replacement is strictly wider: every value the old constraint
-- accepted is still accepted.
alter table auth_providers drop constraint auth_providers_kind_check;

alter table auth_providers add constraint auth_providers_kind_check
    check (kind in ('ldap', 'active_directory', 'oidc', 'oauth2', 'saml'));

-- When the registry screen last proved this provider, and whether it worked.
alter table auth_providers add column last_test_at timestamptz;
alter table auth_providers add column last_test_ok boolean;

-- Sync bookkeeping. Nulls and zero mean "this kind does not sync", which the screen shows as a
-- dash rather than as a stale date.
alter table auth_providers add column sync_interval_minutes integer not null default 60;
alter table auth_providers add column last_sync_at timestamptz;
alter table auth_providers add column last_sync_status text
    default 'ok' check (last_sync_status in ('ok', 'partial', 'failed'));

-- The plugin declaration that produced this row, when it was not a platform kind.
alter table auth_providers add column plugin_key text;

-- A negative interval would mean "sync every -30 minutes", which is the kind of setting that
-- discovers itself by hammering somebody else's directory. Zero means "never on a schedule",
-- which is a real choice for a provider that is only ever used interactively.
alter table auth_providers add constraint auth_providers_sync_interval_check
    check (sync_interval_minutes between 0 and 10080);

-- The list filters by kind inside one organization, and the only list that grows past a
-- screenful is the enabled one, so the index is partial.
create index auth_providers_org_kind_idx
    on auth_providers (organization_id, kind)
    where enabled;
