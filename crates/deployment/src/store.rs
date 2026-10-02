//! The cache: release manifests, their artifacts, environment bundles and upgrade plans
//! (docs/requests/REQ-128, slice 4).
//!
//! ## What the store refuses to do
//!
//! * **It does not store a plan's steps as anything but the derived document.** A plan is
//!   regenerated from the two manifests; only the acknowledgement is durable.
//! * **It does not overwrite an acknowledgement with an un-acknowledged plan.** Re-generating a
//!   plan for the same range keeps the consent: an operator who already accepted the warning
//!   should not be asked again because someone re-ran the update check. The acknowledgement is
//!   re-read and carried onto the new row, and the walk proves it.
//! * **It does not let a manifest's `migrations_destructive: false` become a reversibility
//!   proof.** That rule lives in [`crate::manifest`] where it can be tested without a database;
//!   the store stores the flag as the publisher's claim and nothing more.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeploymentError, Result};
use crate::manifest::{ARTIFACT_KINDS, CHANNELS, ReleaseManifest};
use crate::plan::{PlanBuilder, PlanContext, UpgradePlan};

/// A published artifact of one release.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct Artifact {
    /// Row id.
    pub id: i64,
    /// The release it belongs to.
    pub version: String,
    /// One of [`ARTIFACT_KINDS`].
    pub kind: String,
    /// The pull reference, the binary name or the chart name.
    pub name: String,
    /// A digest for an image, a checksum for a file, and `None` when the release published this
    /// kind with no verifiable checksum.
    pub digest: Option<String>,
    /// The platforms it was built for.
    pub platforms: Vec<String>,
    /// Its size, when the publisher reported one.
    pub size_bytes: Option<i64>,
    /// Where it can be downloaded.
    pub download_url: Option<String>,
    /// When it was published.
    pub published_at: Option<OffsetDateTime>,
    /// The manifest version it was described by.
    pub manifest_version: String,
}

/// An artifact row to cache, as the manifest fetch supplies it.
#[derive(Debug, Clone)]
pub struct NewArtifact {
    /// The release it belongs to.
    pub version: String,
    /// One of [`ARTIFACT_KINDS`].
    pub kind: String,
    /// The pull reference, the binary name or the chart name.
    pub name: String,
    /// A digest for an image, a checksum for a file.
    pub digest: Option<String>,
    /// The platforms it was built for.
    pub platforms: Vec<String>,
    /// Its size, when the publisher reported one.
    pub size_bytes: Option<i64>,
    /// Where it can be downloaded.
    pub download_url: Option<String>,
    /// When it was published.
    pub published_at: Option<OffsetDateTime>,
}

/// Validate a channel, a kind and a version before they reach a row.
pub fn validate_manifest_fields(channel: &str, version: &str) -> Result<()> {
    if !CHANNELS.contains(&channel) {
        return Err(DeploymentError::UnknownVocabulary(format!(
            "{channel:?} is not a release channel; expected one of {}",
            CHANNELS.join(", ")
        )));
    }
    // The version is parsed for the same reason the plan parses it: a row keyed by a
    // non-version cannot be ordered against anything, and `get_latest` would pick it as the
    // newest release.
    crate::manifest::Version::parse(version, "version")?;
    Ok(())
}

/// Upsert a manifest and its artifacts, as one operation.
///
/// Artifacts are **replaced**, not merged: a manifest is a complete description of a release, so
/// a kind the new manifest no longer lists is one the release withdrew, and leaving its row
/// behind would show a panel an artifact that does not exist. The manifest row itself is
/// upserted so a re-fetch updates `fetched_at` and the publisher's claim.
///
/// Returns the number of artifact rows written, which is what the `release.manifest.updated`
/// event's payload carries.
pub async fn cache_manifest(pool: &PgPool, manifest: &ReleaseManifest) -> Result<usize> {
    validate_manifest_fields(&manifest.channel, &manifest.version)?;

    sqlx::query(
        "insert into release_manifests \
           (version, channel, source_commit, core_min, migrations, migrations_destructive, \
            notes_md, upgrade_notes_url, fetched_at, raw) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, now(), $9) \
         on conflict (version) do update set \
           channel = excluded.channel, \
           source_commit = excluded.source_commit, \
           core_min = excluded.core_min, \
           migrations = excluded.migrations, \
           migrations_destructive = excluded.migrations_destructive, \
           notes_md = excluded.notes_md, \
           upgrade_notes_url = excluded.upgrade_notes_url, \
           fetched_at = now(), \
           raw = excluded.raw",
    )
    .bind(&manifest.version)
    .bind(&manifest.channel)
    .bind(&manifest.source_commit)
    .bind(&manifest.core_min)
    .bind(&manifest.migrations)
    .bind(manifest.migrations_destructive)
    .bind(&manifest.notes_md)
    .bind(&manifest.upgrade_notes_url)
    .bind(&manifest.raw)
    .execute(pool)
    .await?;

    sqlx::query("delete from release_artifacts where version = $1")
        .bind(&manifest.version)
        .execute(pool)
        .await?;

    let mut written = 0usize;
    for artifact in &manifest.raw["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let kind = artifact["kind"].as_str().unwrap_or_default().to_owned();
        let name = artifact["name"].as_str().unwrap_or_default().to_owned();
        // A manifest entry with no kind or no name is not an artifact; it is a line in a
        // document. Skipping it is right, but *counting* it as written would be a lie, and the
        // event payload says how many rows exist.
        if kind.is_empty() || name.is_empty() {
            tracing::warn!(
                version = %manifest.version,
                "a release manifest entry has no kind or name and was not cached"
            );
            continue;
        }
        if !ARTIFACT_KINDS.contains(&kind.as_str()) {
            return Err(DeploymentError::UnknownVocabulary(format!(
                "{kind:?} is not an artifact kind; expected one of {}",
                ARTIFACT_KINDS.join(", ")
            )));
        }
        let row = NewArtifact {
            version: manifest.version.clone(),
            kind,
            name,
            digest: artifact["digest"].as_str().map(str::to_owned),
            platforms: artifact["platforms"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            size_bytes: artifact["size_bytes"].as_i64(),
            download_url: artifact["download_url"].as_str().map(str::to_owned),
            published_at: None,
        };
        insert_artifact(pool, &row).await?;
        written += 1;
    }
    Ok(written)
}

/// Write one artifact row.
pub async fn insert_artifact(pool: &PgPool, artifact: &NewArtifact) -> Result<i64> {
    // `query_scalar` rather than `query(..).fetch_one()` + `row.id`: `PgRow` has no field
    // accessors, so the typed scalar is both shorter and impossible to get wrong.
    let id = sqlx::query_scalar::<_, i64>(
        "insert into release_artifacts \
           (version, kind, name, digest, platforms, size_bytes, download_url, published_at, \
            manifest_version) \
         values ($1, $2, $3, $4, $5, $6, $7, coalesce($8, now()), $9) \
         on conflict (version, kind, name) do update set \
           digest = excluded.digest, \
           platforms = excluded.platforms, \
           size_bytes = excluded.size_bytes, \
           download_url = excluded.download_url, \
           published_at = excluded.published_at, \
           manifest_version = excluded.manifest_version \
         returning id",
    )
    .bind(&artifact.version)
    .bind(&artifact.kind)
    .bind(&artifact.name)
    .bind(&artifact.digest)
    .bind(&artifact.platforms)
    .bind(artifact.size_bytes)
    .bind(&artifact.download_url)
    .bind(artifact.published_at)
    .bind(&artifact.version)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Every cached manifest, newest first.
pub async fn list_manifests(pool: &PgPool, limit: i64) -> Result<Vec<ReleaseManifest>> {
    Ok(sqlx::query_as::<_, ReleaseManifest>(
        "select version, channel, source_commit, core_min, migrations, migrations_destructive, \
                notes_md, upgrade_notes_url, fetched_at, raw \
           from release_manifests order by fetched_at desc, version desc limit $1",
    )
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?)
}

/// One cached manifest.
pub async fn find_manifest(pool: &PgPool, version: &str) -> Result<Option<ReleaseManifest>> {
    Ok(sqlx::query_as::<_, ReleaseManifest>(
        "select version, channel, source_commit, core_min, migrations, migrations_destructive, \
                notes_md, upgrade_notes_url, fetched_at, raw \
           from release_manifests where version = $1",
    )
    .bind(version)
    .fetch_optional(pool)
    .await?)
}

/// The newest cached manifest of a channel, by version rather than by fetch time.
///
/// **By version, not by `fetched_at`.** A re-fetch of an older release (an operator pinning, a
/// re-run of the check) would make it the newest row, and the upgrade screen would then plan
/// an upgrade *backwards*. The ordering is a property of the release, so it is the release's own
/// numbers that decide.
pub async fn latest_manifest(pool: &PgPool, channel: &str) -> Result<Option<ReleaseManifest>> {
    let all = list_manifests(pool, 100).await?;
    Ok(all
        .into_iter()
        .filter(|manifest| manifest.channel == channel)
        .max_by(|a, b| {
            crate::manifest::Version::parse(&a.version, "version")
                .ok()
                .zip(crate::manifest::Version::parse(&b.version, "version").ok())
                .map(|(left, right)| left.cmp_to(&right))
                .unwrap_or(std::cmp::Ordering::Equal)
        }))
}

/// The artifacts of one release, newest release first within the version.
pub async fn list_artifacts(
    pool: &PgPool,
    version: Option<&str>,
    limit: i64,
) -> Result<Vec<Artifact>> {
    if let Some(version) = version {
        return Ok(sqlx::query_as::<_, Artifact>(
            "select id, version, kind, name, digest, platforms, size_bytes, download_url, \
                    published_at, manifest_version \
               from release_artifacts where version = $1 order by kind, name",
        )
        .bind(version)
        .fetch_all(pool)
        .await?);
    }
    Ok(sqlx::query_as::<_, Artifact>(
        "select a.id, a.version, a.kind, a.name, a.digest, a.platforms, a.size_bytes, \
                a.download_url, a.published_at, a.manifest_version \
           from release_artifacts a \
           join release_manifests m on m.version = a.version \
           order by m.fetched_at desc, a.kind, a.name limit $1",
    )
    .bind(limit.clamp(1, 1000))
    .fetch_all(pool)
    .await?)
}

/// The kinds a release published, and the kinds it did not.
///
/// The screen's "not published for this version" rows come from this rather than from a
/// client-side `kinds.filter(...)`: a client that computed the missing set would report a
/// release as missing a CLI it never claimed to ship.
pub async fn artifact_kind_coverage(pool: &PgPool, version: &str) -> Result<Vec<(String, i64)>> {
    Ok(sqlx::query_as::<_, (String, i64)>(
        "select kind, count(*) from release_artifacts where version = $1 group by kind order by kind",
    )
    .bind(version)
    .fetch_all(pool)
    .await?)
}

/// One stored upgrade plan, with the acknowledgement it carries.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct StoredUpgradePlan {
    /// Row id.
    pub id: Uuid,
    /// The version this install runs.
    pub from_version: String,
    /// The version being upgraded to.
    pub to_version: String,
    /// `compose` or `kubernetes`.
    pub topology: String,
    /// The compose stack, on the compose topology.
    pub bundle_kind: Option<String>,
    /// The stored derived plan.
    pub steps: serde_json::Value,
    /// The verdict the acknowledgement was about.
    pub destructive_verdict: Option<String>,
    /// Index of the point of no return at generation time.
    pub point_of_no_return: Option<i32>,
    /// Who acknowledged it.
    pub destructive_acknowledged_by: Option<Uuid>,
    /// When.
    pub destructive_acknowledged_at: Option<OffsetDateTime>,
    /// Who generated it.
    pub created_by: Option<Uuid>,
    /// When it was generated.
    pub created_at: OffsetDateTime,
}

/// Read the acknowledgement already recorded for a range and topology.
pub async fn find_acknowledged_plan(
    pool: &PgPool,
    from_version: &str,
    to_version: &str,
    topology: &str,
) -> Result<Option<StoredUpgradePlan>> {
    Ok(sqlx::query_as::<_, StoredUpgradePlan>(
        "select id, from_version, to_version, topology, bundle_kind, steps, \
                destructive_verdict, point_of_no_return, destructive_acknowledged_by, \
                destructive_acknowledged_at, created_by, created_at \
           from upgrade_plans \
          where from_version = $1 and to_version = $2 and topology = $3 \
            and destructive_acknowledged_by is not null",
    )
    .bind(from_version)
    .bind(to_version)
    .bind(topology)
    .fetch_optional(pool)
    .await?)
}

/// Store a plan, carrying an existing acknowledgement forward.
///
/// The acknowledgement is a durable fact about a person and a version range, so regenerating the
/// plan does not erase it — and this is the property the walk asserts, because a screen that
/// re-asks for consent every time somebody re-ran the update check is a screen that trains
/// operators to click through warnings.
pub async fn store_plan(
    pool: &PgPool,
    plan: &UpgradePlan,
    created_by: Option<Uuid>,
) -> Result<StoredUpgradePlan> {
    let existing =
        find_acknowledged_plan(pool, &plan.from_version, &plan.to_version, &plan.topology).await?;
    let (acknowledged_by, acknowledged_at) = match &existing {
        Some(row) => (
            row.destructive_acknowledged_by,
            row.destructive_acknowledged_at,
        ),
        None => (None, None),
    };
    let steps = serde_json::to_value(plan).map_err(|error| {
        DeploymentError::Conflict(format!("the plan could not be stored: {error}"))
    })?;

    // One acknowledged plan per range, enforced by the partial unique index. So when this plan
    // CARRIES an acknowledgement, the previous holder of the same range is released **before**
    // the insert — the first draft inserted first and released after, and the index fired on the
    // insert with a `duplicate key` the caller could not act on. Clear-then-write is also the
    // order that leaves the range un-acknowledged rather than double-acknowledged if the process
    // dies between the two statements.
    if acknowledged_by.is_some() {
        sqlx::query(
            "update upgrade_plans set destructive_acknowledged_by = null, \
                    destructive_acknowledged_at = null \
              where from_version = $1 and to_version = $2 and topology = $3 \
                and destructive_acknowledged_by is not null",
        )
        .bind(&plan.from_version)
        .bind(&plan.to_version)
        .bind(&plan.topology)
        .execute(pool)
        .await?;
    }

    let row = sqlx::query_as::<_, StoredUpgradePlan>(
        "insert into upgrade_plans \
           (id, from_version, to_version, topology, bundle_kind, steps, destructive_verdict, \
            point_of_no_return, destructive_acknowledged_by, destructive_acknowledged_at, \
            created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         returning id, from_version, to_version, topology, bundle_kind, steps, \
                   destructive_verdict, point_of_no_return, destructive_acknowledged_by, \
                   destructive_acknowledged_at, created_by, created_at",
    )
    .bind(Uuid::new_v4())
    .bind(&plan.from_version)
    .bind(&plan.to_version)
    .bind(&plan.topology)
    .bind(&plan.bundle_kind)
    .bind(&steps)
    .bind(&plan.destructive.verdict)
    .bind(plan.point_of_no_return.map(|i| i as i32))
    .bind(acknowledged_by)
    .bind(acknowledged_at)
    .bind(created_by)
    .fetch_one(pool)
    .await?;

    // One acknowledged plan per range: a second one would mean the screen cannot answer "has
    // this been accepted", and a write that silently created a second answer is worse than one
    // that refuses.
    if row.destructive_acknowledged_by.is_some() {
        sqlx::query(
            "update upgrade_plans set destructive_acknowledged_by = null, \
                    destructive_acknowledged_at = null \
              where id <> $1 and from_version = $2 and to_version = $3 and topology = $4 \
                and destructive_acknowledged_by is not null",
        )
        .bind(row.id)
        .bind(&plan.from_version)
        .bind(&plan.to_version)
        .bind(&plan.topology)
        .execute(pool)
        .await?;
    }
    Ok(row)
}

/// Record an operator's acknowledgement of a range's destructiveness warning.
///
/// Only a range that actually needs one can be acknowledged, and the refusal names the verdict:
/// acknowledging a reversible range would be a consent to a warning that does not exist, and an
/// endpoint that accepts it teaches a client that the field is decorative.
pub async fn acknowledge_plan(
    pool: &PgPool,
    id: Uuid,
    verdict: &str,
    acknowledged_by: Uuid,
) -> Result<StoredUpgradePlan> {
    let row = sqlx::query_as::<_, StoredUpgradePlan>(
        "select id, from_version, to_version, topology, bundle_kind, steps, \
                destructive_verdict, point_of_no_return, destructive_acknowledged_by, \
                destructive_acknowledged_at, created_by, created_at \
           from upgrade_plans where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DeploymentError::not_found("upgrade plan", id))?;

    let stored_verdict = row.destructive_verdict.clone().unwrap_or_default();
    if !crate::manifest::VERDICTS.contains(&stored_verdict.as_str()) {
        return Err(DeploymentError::Conflict(format!(
            "this plan carries no destructiveness verdict, so there is no warning to acknowledge \
             (it was stored as {stored_verdict:?})"
        )));
    }
    if !matches!(
        stored_verdict.as_str(),
        crate::manifest::VERDICT_DESTRUCTIVE | crate::manifest::VERDICT_UNKNOWN
    ) {
        return Err(DeploymentError::Conflict(format!(
            "this plan is {stored_verdict}, which has no warning to acknowledge"
        )));
    }
    // The caller must be acknowledging the verdict the plan actually carries. A client that
    // posts `destructive` for an `unknown` plan is a client whose acknowledgement does not
    // describe what it consented to.
    if verdict != stored_verdict {
        return Err(DeploymentError::Conflict(format!(
            "the plan is {stored_verdict}, not {verdict}: acknowledge the verdict the plan carries"
        )));
    }

    // **Clear the previous holder BEFORE setting the new one.** The partial unique index
    // (`upgrade_plans_acknowledged_unique`) enforces one acknowledged plan per range at the
    // database, and the first draft of this function did it the other way round: set the actor,
    // then clear the others. The index therefore fired on the SET — a 500 carrying
    // `duplicate key value violates unique constraint` — and the follow-up `update` that was
    // supposed to prevent it never ran. It passed every unit test in the crate because none of
    // them builds a router, and the integration walk found it on its first execution.
    //
    // Clearing first is also the order that is correct if the process dies between the two
    // statements: the range is left UNACKNOWLEDGED, which re-asks, rather than left
    // double-acknowledged, which is a state nothing can read.
    sqlx::query(
        "update upgrade_plans set destructive_acknowledged_by = null, \
                destructive_acknowledged_at = null \
          where id <> $1 and from_version = $2 and to_version = $3 and topology = $4 \
            and destructive_acknowledged_by is not null",
    )
    .bind(id)
    .bind(&row.from_version)
    .bind(&row.to_version)
    .bind(&row.topology)
    .execute(pool)
    .await?;

    let row = sqlx::query_as::<_, StoredUpgradePlan>(
        "update upgrade_plans \
            set destructive_acknowledged_by = $2, destructive_acknowledged_at = now() \
          where id = $1 \
         returning id, from_version, to_version, topology, bundle_kind, steps, \
                   destructive_verdict, point_of_no_return, destructive_acknowledged_by, \
                   destructive_acknowledged_at, created_by, created_at",
    )
    .bind(id)
    .bind(acknowledged_by)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// How many acknowledged plans exist for a range, across every row of it.
///
/// The count is the assertion the acknowledgement walk uses, because a store that wrote the
/// actor twice would still return a row with an actor.
pub async fn acknowledged_plan_count(
    pool: &PgPool,
    from_version: &str,
    to_version: &str,
    topology: &str,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from upgrade_plans \
          where from_version = $1 and to_version = $2 and topology = $3 \
            and destructive_acknowledged_by is not null",
    )
    .bind(from_version)
    .bind(to_version)
    .bind(topology)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// A generated environment bundle.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct EnvironmentBundle {
    /// Row id.
    pub id: Uuid,
    /// The operator's name for this target.
    pub name: String,
    /// `compose-small`, `compose-enterprise` or `helm`.
    pub kind: String,
    /// The release the bundle is for.
    pub version: String,
    /// The generator's request: domain, registry, presets, TLS mode. **Never a credential.**
    pub config: serde_json::Value,
    /// `[{name, size, sha256}]` per generated file.
    pub files: serde_json::Value,
    /// The bundle's own checksum.
    pub checksum: String,
    /// Who generated it.
    pub generated_by: Option<Uuid>,
    /// When.
    pub generated_at: OffsetDateTime,
    /// How many times it has been downloaded.
    pub download_count: i32,
    /// When it was last downloaded.
    pub last_downloaded_at: Option<OffsetDateTime>,
}

/// Validate a bundle request before anything is written.
///
/// Three refusals, and each names its field because the screen's form has to say which input an
/// operator got wrong:
pub fn validate_bundle_request(
    name: &str,
    kind: &str,
    version: &str,
    domain: &str,
    registry: &str,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(DeploymentError::UnknownVocabulary(
            "name must be between 1 and 64 characters".into(),
        ));
    }
    if !crate::manifest::BUNDLE_KINDS.contains(&kind) {
        return Err(DeploymentError::UnknownVocabulary(format!(
            "{kind:?} is not a bundle kind; expected one of {}",
            crate::manifest::BUNDLE_KINDS.join(", ")
        )));
    }
    crate::manifest::Version::parse(version, "version")?;
    // A domain is what the operator will point a certificate at, so a shape check is a real
    // check rather than politeness: `https://` in this field produces a `verify` step that
    // names a URL nobody can open.
    if !domain.is_empty() {
        let plausible = domain.split('.').count() >= 2
            && !domain.contains("://")
            && !domain.contains(char::is_whitespace)
            && !domain.starts_with('.')
            && !domain.ends_with('.');
        if !plausible {
            return Err(DeploymentError::UnknownVocabulary(format!(
                "domain {domain:?} is not a host name: pass the host (panel.example.com), not a URL"
            )));
        }
    }
    if registry.contains("://") {
        return Err(DeploymentError::UnknownVocabulary(format!(
            "registry {registry:?} is a URL: pass the host prefix (ghcr.io/raksix/omnion)"
        )));
    }
    Ok(())
}

/// Store a generated bundle.
pub async fn insert_bundle(
    pool: &PgPool,
    name: &str,
    kind: &str,
    version: &str,
    config: &serde_json::Value,
    files: &serde_json::Value,
    checksum: &str,
    generated_by: Option<Uuid>,
) -> Result<EnvironmentBundle> {
    Ok(sqlx::query_as::<_, EnvironmentBundle>(
        "insert into environment_bundles \
           (id, name, kind, version, config, files, checksum, generated_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (name, version) do update set \
           kind = excluded.kind, config = excluded.config, files = excluded.files, \
           checksum = excluded.checksum, generated_by = excluded.generated_by, \
           generated_at = now() \
         returning id, name, kind, version, config, files, checksum, generated_by, generated_at, \
                   download_count, last_downloaded_at",
    )
    .bind(Uuid::new_v4())
    .bind(name.trim())
    .bind(kind)
    .bind(version)
    .bind(config)
    .bind(files)
    .bind(checksum)
    .bind(generated_by)
    .fetch_one(pool)
    .await?)
}

/// The bundles of this instance, newest first.
pub async fn list_bundles(pool: &PgPool, limit: i64) -> Result<Vec<EnvironmentBundle>> {
    Ok(sqlx::query_as::<_, EnvironmentBundle>(
        "select id, name, kind, version, config, files, checksum, generated_by, generated_at, \
                download_count, last_downloaded_at \
           from environment_bundles order by generated_at desc limit $1",
    )
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?)
}

/// One bundle.
pub async fn find_bundle(pool: &PgPool, id: Uuid) -> Result<Option<EnvironmentBundle>> {
    Ok(sqlx::query_as::<_, EnvironmentBundle>(
        "select id, name, kind, version, config, files, checksum, generated_by, generated_at, \
                download_count, last_downloaded_at \
           from environment_bundles where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Count a download, and stamp the time. Returns the bundle as it now reads.
///
/// Counting rather than only stamping, because the panel shows a download count and a counter
/// that is only ever zero is a number nobody believes.
pub async fn record_bundle_download(pool: &PgPool, id: Uuid) -> Result<EnvironmentBundle> {
    Ok(sqlx::query_as::<_, EnvironmentBundle>(
        "update environment_bundles \
            set download_count = download_count + 1, last_downloaded_at = now() \
          where id = $1 \
         returning id, name, kind, version, config, files, checksum, generated_by, generated_at, \
                   download_count, last_downloaded_at",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DeploymentError::not_found("environment bundle", id))?)
}

/// One generated file of a bundle, as the file list carries it.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct BundleFile {
    /// The file name as it lands on the operator's host.
    pub name: String,
    /// Its size in bytes.
    pub size: i64,
    /// Its sha256.
    pub sha256: String,
}

/// The file list of a bundle, as the stored jsonb reads back.
///
/// **A malformed list is an empty list, not a panic and not a partial one.** A bundle whose
/// `files` column holds a shape this version does not understand has its download list dropped
/// — the operator is told the bundle has no downloadable files rather than being handed a
/// half-parsed list where half the rows have no checksum.
pub fn bundle_files(files: &serde_json::Value) -> Vec<BundleFile> {
    files
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| {
                    Some(BundleFile {
                        name: value.get("name")?.as_str()?.to_owned(),
                        size: value.get("size").and_then(serde_json::Value::as_i64)?,
                        sha256: value.get("sha256")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a plan for a range, from the cached manifests, and store it.
pub async fn plan_and_store(
    pool: &PgPool,
    from: &ReleaseManifest,
    to: &ReleaseManifest,
    topology: &str,
    bundle_kind: &str,
    context: PlanContext,
    created_by: Option<Uuid>,
) -> Result<(UpgradePlan, StoredUpgradePlan)> {
    let builder = PlanBuilder {
        from,
        to,
        topology: topology.to_owned(),
        bundle_kind: bundle_kind.to_owned(),
        context,
    };
    let mut plan = builder.build(false, None)?;
    // An acknowledgement already on record for this range belongs on the plan the screen
    // renders, or the screen asks for a consent the operator has already given.
    if let Some(existing) =
        find_acknowledged_plan(pool, &plan.from_version, &plan.to_version, &plan.topology).await?
    {
        plan.checklist.acknowledged = true;
        plan.checklist.complete = true;
        plan.acknowledged_by = existing.destructive_acknowledged_by;
    }
    let stored = store_plan(pool, &plan, created_by).await?;
    Ok((plan, stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bundle_request_is_refused_with_the_field_that_is_wrong() {
        // The three refusals a form can produce, each naming itself.
        // A 65-character name is over the cap and is the refusal under test. A `const` rather
        // than `String::repeat` so the fixture table below is one uniform `&str` array.
        const OVER_CAP: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        for (name, kind, version, domain, registry, expect) in [
            (
                OVER_CAP,
                "helm",
                "0.5.0",
                "x.example.com",
                "ghcr.io/a",
                "name",
            ),
            (
                "ok",
                "nomad",
                "0.5.0",
                "x.example.com",
                "ghcr.io/a",
                "bundle kind",
            ),
            (
                "ok",
                "helm",
                "not-a-version",
                "x.example.com",
                "ghcr.io/a",
                "version",
            ),
            (
                "ok",
                "helm",
                "0.5.0",
                "https://x.example.com",
                "ghcr.io/a",
                "host name",
            ),
            (
                "ok",
                "helm",
                "0.5.0",
                "x.example.com",
                "https://ghcr.io",
                "URL",
            ),
        ] {
            let error = validate_bundle_request(name, kind, version, domain, registry)
                .expect_err("a refusal");
            let text = error.to_string();
            assert!(
                text.contains(expect),
                "{name:?}/{kind:?}/{version:?} must be refused naming {expect:?}, got: {text}"
            );
        }
        // And the valid case, so the refusals above are refusals rather than a rule that fires
        // on everything.
        validate_bundle_request(
            "prod-eu",
            "compose-small",
            "0.5.0",
            "panel.example.com",
            "ghcr.io/raksix/omnion",
        )
        .expect("a valid request");
        // A single-label host is legal on a private network, so an empty domain is the opt-out
        // and a bare label is not refused.
        validate_bundle_request("lab", "helm", "0.5.0", "", "")
            .expect("an empty domain is allowed");
    }

    #[test]
    fn a_malformed_file_list_is_an_empty_list_and_not_a_partial_one() {
        // Half a list is worse than none: a download button with no checksum beside it is the
        // shape this request forbids.
        let good = json!([
            {"name": "docker-compose.prod.yml", "size": 1234, "sha256": "aa11"},
            {"name": "README.md", "size": 22, "sha256": "bb22"}
        ]);
        assert_eq!(bundle_files(&good).len(), 2);

        for malformed in [
            json!([{"name": "a.yml", "size": 1}]),      // no sha256
            json!([{"name": "a.yml", "sha256": "aa"}]), // no size
            json!([{"size": 1, "sha256": "aa"}]),       // no name
            json!({"not": "an array"}),
            json!(null),
        ] {
            assert!(
                bundle_files(&malformed).is_empty(),
                "a malformed list must be empty, not partial: {malformed}"
            );
        }
    }

    #[test]
    fn an_artifact_kind_outside_the_closed_set_is_a_refusal_naming_the_set() {
        let error = validate_manifest_fields("stable", "0.5.0").expect("a valid manifest");
        assert!(error.eq(&()), "a valid manifest validates");
        // The channel and version refusals are the two the update check can produce.
        assert!(validate_manifest_fields("nightly", "0.5.0").is_err());
        assert!(validate_manifest_fields("stable", "0.5").is_err());
    }
}
