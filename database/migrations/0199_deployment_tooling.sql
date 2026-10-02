-- The release manifest cache, the artifact rows, the environment bundles and the upgrade plans
-- (docs/requests/REQ-128, slice 4).
--
-- **Why these four tables are one migration.** They are one feature — "what release is this
-- instance running, and how does an operator move it to the next one" — and splitting them
-- would let a deployment have a manifest cache with no artifact rows, or bundles with no plan,
-- which are both states no screen can render. The request's data model lists them together for
-- that reason and this file keeps them together.
--
-- **`release_manifests.raw` is the signed document as received, never a re-derivation.** A digest
-- has to be re-checkable against what the publisher signed, and a manifest rebuilt from the tree
-- it is later compared against is exactly the thing that cannot be re-checked: it agrees with
-- itself. So the raw body is stored beside the columns the screens read.
--
-- **`upgrade_plans` persists an operator's ACKNOWLEDGEMENT, not their plan.** The steps are
-- derived from two manifests and are regenerated whenever either changes — an operator editing
-- a plan is editing a derived document. The one thing that is a durable fact about a person is
-- that they read the warning and accepted it, so that is the column pair, and the acknowledgement
-- is tied to a specific (from, to, topology) triple rather than to a plan id: a plan regenerated
-- for a newer target does not inherit the consent somebody gave for an older one.
--
-- **No foreign key from the acknowledgement to `users`.** Every other audit-shaped column in the
-- tree references the actor, and this one does not, on purpose: an installation's upgrade
-- history has to survive an account being deleted, and an acknowledgement that disappears with
-- the operator leaves a plan that demands consent nobody can give.
create table release_manifests (
    version            text primary key,
    channel            text not null default 'stable',
    source_commit      text,
    core_min           text,
    -- The migrations this release ships, in file order. The upgrade plan's delta is the
    -- DIFFERENCE of two of these lists, so an unsorted or empty one silently changes which
    -- migrations an upgrade claims to apply.
    migrations         text[] not null default '{}',
    -- The publisher's own claim. It is NOT consulted as a proof of reversibility: a manifest
    -- that says `false` and a release with no down-script gate both mean "nobody has
    -- established that the down script works", and the API refuses to report the first as the
    -- second. `deployment.manifest::destructiveness` holds that rule and has a test for it.
    migrations_destructive boolean not null default false,
    notes_md           text not null default '',
    upgrade_notes_url  text,
    fetched_at         timestamptz not null default now(),
    raw                jsonb not null default '{}',
    constraint release_manifests_channel_known
        check (channel in ('stable', 'beta', 'edge'))
);

create index release_manifests_fetched_idx
    on release_manifests (fetched_at desc);

comment on column release_manifests.raw is
    'The signed manifest exactly as received, so a digest can be re-checked against the publisher''s document rather than against a re-derivation of it.';

-- One row per published artifact of one version. `digest` is an image digest for `image` and a
-- file checksum for everything else, which is why it is nullable rather than not null: a release
-- may publish a chart with no CLI binaries, and a row that had to invent a digest to exist is a
-- row the panel would show a fake reference for.
create table release_artifacts (
    id                bigserial primary key,
    version           text not null references release_manifests (version) on delete cascade,
    kind              text not null check (kind in ('image', 'cli', 'chart', 'sbom', 'compose')),
    -- The pull reference, the binary name, the chart name.
    name              text not null,
    digest            text,
    platforms         text[] not null default '{}',
    size_bytes        bigint,
    download_url      text,
    published_at      timestamptz,
    manifest_version  text not null,
    -- One artifact is one artifact. Re-fetching a manifest updates the row rather than adding a
    -- second one, because "two digests for 0.5.0" is a question no screen can answer.
    constraint release_artifacts_unique unique (version, kind, name),
    constraint release_artifacts_size_sane check (size_bytes is null or size_bytes >= 0)
);

create index release_artifacts_version_kind_idx
    on release_artifacts (version, kind);

comment on table release_artifacts is
    'Published artifacts of one release. digest is a digest for an image and a checksum for a file; it is nullable because a release may legitimately publish one kind and not another, and the screen renders an explicit "not published" row instead of a blank.';

-- A generated environment bundle: the compose file, the values file and the notes an operator
-- downloads. `config` holds the request (domain, registry, presets, TLS mode) and NEVER a
-- credential — the generator's own record has no field one can arrive in, and the column is
-- jsonb so the next field somebody adds is the first place a value could leak.
create table environment_bundles (
    id                  uuid primary key,
    name                text not null,
    kind                text not null check (kind in ('compose-small', 'compose-enterprise', 'helm')),
    version             text not null,
    config              jsonb not null default '{}',
    -- [{name, size, sha256}] per generated file.
    files               jsonb not null default '[]',
    -- The checksum of the bundle as a whole, so the download list and the panel agree.
    checksum            text not null,
    generated_by        uuid,
    generated_at        timestamptz not null default now(),
    download_count      int not null default 0,
    last_downloaded_at  timestamptz,
    constraint environment_bundles_name_version_unique unique (name, version),
    constraint environment_bundles_downloads_sane check (download_count >= 0)
);

create index environment_bundles_generated_idx
    on environment_bundles (generated_at desc);

create index environment_bundles_name_version_idx
    on environment_bundles (name, version);

comment on table environment_bundles is
    'A generated install bundle. Re-generating the same name and version writes a NEW row, so an operator can compare the bundle they are about to apply with the one they applied last week.';

-- The upgrade plan, with the acknowledgement that makes it actionable.
--
-- `steps` is the derived step list (kind, text, command, destructiveness, point of no return) as
-- the generator produced it, stored so an operator can see the plan they acknowledged even after
-- the manifests have moved on.
create table upgrade_plans (
    id                        uuid primary key,
    from_version              text not null,
    to_version                text not null,
    topology                  text not null check (topology in ('compose', 'kubernetes')),
    bundle_kind               text,
    steps                     jsonb not null default '[]',
    -- The three facts the whole acknowledgement is about, denormalised from the plan so the
    -- consent cannot outlive it. `destructive_verdict` is one of the three verdicts rather than
    -- a boolean, because `unknown` and `reversible` are different answers and a boolean makes
    -- them the same one.
    destructive_verdict       text,
    point_of_no_return        int,
    -- NULL and NULL is the un-acknowledged state, and a half-written pair is refused rather
    -- than tolerated: an acknowledgement with no actor is nobody having acknowledged anything.
    destructive_acknowledged_by    uuid,
    destructive_acknowledged_at    timestamptz,
    created_by               uuid,
    created_at               timestamptz not null default now(),
    constraint upgrade_plans_verdict_known check (
        destructive_verdict is null
        or destructive_verdict in ('reversible', 'destructive', 'unknown')
    ),
    -- The acknowledgement is all-or-nothing, and the database holds that rather than the
    -- handler: a row with an actor and no timestamp is a consent with no moment.
    constraint upgrade_plans_acknowledgement_complete check (
        (destructive_acknowledged_by is null and destructive_acknowledged_at is null)
        or (destructive_acknowledged_by is not null and destructive_acknowledged_at is not null)
    ),
    constraint upgrade_plans_point_of_no_return_sane check (
        point_of_no_return is null or point_of_no_return >= 0
    )
);

create index upgrade_plans_range_idx
    on upgrade_plans (from_version, to_version);

-- At most one acknowledged plan per (from, to, topology). Two acknowledged plans for the same
-- range would mean the screen could not answer "has this been accepted" — and a second write
-- would silently create a second answer rather than refusing.
create unique index upgrade_plans_acknowledged_unique
    on upgrade_plans (from_version, to_version, topology)
    where destructive_acknowledged_by is not null;

comment on table upgrade_plans is
    'One generated upgrade plan per version range and topology, with the operator''s acknowledgement of the destructiveness warning. Plans are derived and regenerated; only the acknowledgement is a durable fact.';

-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
-- ---------------------------------------------------------------------------
-- Reverse order, children before parents. `if exists` throughout: a down script that fails
-- halfway leaves an instance in a state neither script can continue from.
--
--   drop table if exists upgrade_plans;
--   drop table if exists environment_bundles;
--   drop table if exists release_artifacts;
--   drop table if exists release_manifests;
--
-- Commented out, like every other migration in this tree, because the up half is run by
-- `Db::migrate` and a reversal written as live statements would be executed by it too — on the
-- first apply it would create and then drop every table this file defines, leaving the instance
-- with no deployment schema and a migration row claiming success.
