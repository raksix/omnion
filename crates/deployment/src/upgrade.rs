//! The upgrade module the HTTP layer calls: turn two cached manifests into a stored plan, and
//! record the acknowledgement (docs/requests/REQ-128, slice 4).
//!
//! This exists so [`crate::plan`] (the decision) and [`crate::store`] (the persistence) can each
//! be tested against their own, and so the composition that production uses is itself a named
//! thing with a test. The composition is where the two halves meet, and that is where the
//! interesting failures live:
//!
//! * **The `from` manifest is the one this instance is running.** For a real deployment it comes
//!   from `BuildInfo`, not from the cache — an install whose cache has been pruned must still be
//!   able to plan the upgrade *off* the version it runs, and a plan that cannot name its own
//!   current version is a plan the screen cannot offer.
//! * **The `to` manifest comes from the cache**, and a missing one is a refusal naming the
//!   version rather than an empty plan.
//! * **The plan is regenerated on every read** and stored so the acknowledgement survives. The
//!   stored `steps` column is the *derived* document: it is what the operator acknowledged, and
//!   it is shown again after a re-generation so the two can be compared.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::manifest::ReleaseManifest;
use crate::plan::{PlanContext, UpgradePlan};
use crate::store::{self, StoredUpgradePlan};

/// Everything the upgrade screen reads in one response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpgradeSummary {
    /// The version this instance is running.
    pub current_version: String,
    /// The newest cached release, when there is one.
    pub target_version: Option<String>,
    /// The plan, when a target is cached and the range is a valid upgrade.
    pub plan: Option<UpgradePlan>,
    /// The stored row the plan was written to.
    pub stored: Option<StoredUpgradePlan>,
    /// Every way the plan cannot be executed as written.
    pub problems: Vec<crate::plan::PlanProblem>,
    /// `true` when the plan needs an acknowledgement it does not have.
    pub requires_acknowledgement: bool,
    /// Why there is no plan, when there is none. A screen with a silent empty plan is a screen
    /// an operator cannot act on, so the reason is data rather than absence.
    pub unavailable: Option<String>,
}

/// Build and store the plan from the running version to a cached target.
///
/// `current_version` is the version this instance runs — from the build info, not the cache.
/// `to_version` is the target; `None` means the newest cached release of the channel.
pub async fn prepare(
    pool: &PgPool,
    current_version: &str,
    to_version: Option<&str>,
    channel: &str,
    topology: &str,
    bundle_kind: &str,
    created_by: Option<Uuid>,
) -> Result<UpgradeSummary> {
    let to = match to_version {
        Some(version) => store::find_manifest(pool, version).await?,
        None => store::latest_manifest(pool, channel).await?,
    };
    let Some(to) = to else {
        return Ok(UpgradeSummary {
            current_version: current_version.to_owned(),
            target_version: None,
            plan: None,
            stored: None,
            problems: Vec::new(),
            requires_acknowledgement: false,
            unavailable: Some(
                "no release manifest is cached yet — run an update check to fetch one".to_owned(),
            ),
        });
    };

    // The running version may not be in the cache at all (a build from a branch, a pruned
    // cache), and a plan needs BOTH lists to compute the migration delta. So the current
    // version is described as a manifest with no migrations rather than refused: the plan then
    // reports every migration the target ships as new, which is the honest reading of "this
    // install has none of them recorded".
    let from = find_or_describe_running(pool, current_version).await?;

    let context = PlanContext {
        marked_destructive: vec![],
        policy_exists: policy_has_landed(pool).await,
        target_image: target_image(&to),
    };
    let (plan, stored) =
        store::plan_and_store(pool, &from, &to, topology, bundle_kind, context, created_by).await?;
    let problems = crate::plan::verify_plan(&plan, &to);
    let requires_acknowledgement =
        plan.checklist.requires_acknowledgement && !plan.checklist.acknowledged;

    Ok(UpgradeSummary {
        current_version: current_version.to_owned(),
        target_version: Some(to.version.clone()),
        plan: Some(plan),
        stored: Some(stored),
        problems,
        requires_acknowledgement,
        unavailable: None,
    })
}

/// The image reference the target's manifest names, when it names one.
fn target_image(manifest: &ReleaseManifest) -> Option<String> {
    manifest.raw["images"]
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .find(|image| {
                    image["name"]
                        .as_str()
                        .is_some_and(|name| name.ends_with("/api") || name == "api")
                })
                .or_else(|| images.first())
        })
        .map(|image| {
            let name = image["name"].as_str().unwrap_or_default();
            let digest = image["digest"].as_str();
            let tag = image["tag"].as_str().unwrap_or(&manifest.version);
            match digest {
                // A digest is the exact reference an operator pins; a tag next to it would be
                // the mutable thing they were trying to avoid.
                Some(digest) => format!("{name}@{digest}"),
                None => format!("{name}:{tag}"),
            }
        })
}

/// The manifest of the version this instance runs, described from the cache when it is there.
///
/// The `migrations` list is what the delta is computed from, so a cached row is preferred
/// whenever one exists — an install that is two versions behind the cache holds the older
/// manifest only if somebody cached it, and the fallback below is what happens when nobody did.
async fn find_or_describe_running(pool: &PgPool, version: &str) -> Result<ReleaseManifest> {
    if let Some(manifest) = store::find_manifest(pool, version).await? {
        return Ok(manifest);
    }
    Ok(ReleaseManifest {
        version: version.to_owned(),
        channel: "stable".to_owned(),
        source_commit: None,
        core_min: None,
        migrations: Vec::new(),
        migrations_destructive: false,
        notes_md: String::new(),
        upgrade_notes_url: None,
        fetched_at: time::OffsetDateTime::UNIX_EPOCH,
        // Flagged in the raw document so the panel can say WHY the delta is "everything", rather
        // than showing an operator a plan that re-applies migrations with no explanation.
        raw: json!({ "synthesised": true, "reason": "the running version is not in the release cache" }),
    })
}

/// Whether REQ-129's policy has landed in this installation.
///
/// Asked of the **database**, not the source tree: the API has no repository, and the question
/// that matters is whether *this instance* has the gate. Until it does, every plan is `unknown`
/// and the screen says why.
pub async fn policy_has_landed(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>(
        "select exists ( \
             select 1 from information_schema.tables \
              where table_schema = current_schema() and table_name = 'migration_policies')",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn manifest(version: &str, images: Value) -> ReleaseManifest {
        ReleaseManifest {
            version: version.into(),
            channel: "stable".into(),
            source_commit: None,
            core_min: None,
            migrations: vec![],
            migrations_destructive: false,
            notes_md: String::new(),
            upgrade_notes_url: None,
            fetched_at: time::OffsetDateTime::UNIX_EPOCH,
            raw: json!({ "images": images }),
        }
    }

    #[test]
    fn the_target_image_prefers_a_digest_over_a_mutable_tag() {
        // An operator pinning a digest is trying to avoid exactly what a tag can become, so a
        // reference that carries both is a reference that reads as pinned and is not.
        let with_digest = manifest(
            "0.5.0",
            json!([{"name": "ghcr.io/raksix/omnion/api", "tag": "0.5.0", "digest": "sha256:abc"}]),
        );
        assert_eq!(
            target_image(&with_digest).as_deref(),
            Some("ghcr.io/raksix/omnion/api@sha256:abc")
        );

        // Without a digest, the version is the tag.
        let tag_only = manifest("0.5.0", json!([{"name": "ghcr.io/raksix/omnion/api"}]));
        assert_eq!(
            target_image(&tag_only).as_deref(),
            Some("ghcr.io/raksix/omnion/api:0.5.0")
        );

        // The API image is preferred over whatever else the release ships, because that is the
        // one the plan rolls.
        let many = manifest(
            "0.5.0",
            json!([
                {"name": "ghcr.io/raksix/omnion/web"},
                {"name": "ghcr.io/raksix/omnion/api", "tag": "0.5.0"}
            ]),
        );
        assert_eq!(
            target_image(&many).as_deref(),
            Some("ghcr.io/raksix/omnion/api:0.5.0")
        );

        // A manifest with no images names no image, rather than inventing one.
        assert_eq!(target_image(&manifest("0.5.0", json!([]))), None);
    }
}
