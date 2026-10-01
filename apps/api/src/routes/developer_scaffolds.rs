//! The scaffold archive as a file, and the `sdk_scaffolds` row it is recorded in (REQ-033, slice 4).
//!
//! Slice 4 shipped `POST /dev/sdks/scaffold` returning the file tree, and a response field
//! `object_key` that nothing in the platform ever filled in. The tree was previewable and the
//! archive was not downloadable: a developer could read every generated file on screen and still
//! have nothing to unpack. This file closes that, and it is deliberately its own module because
//! the two halves it joins — object storage and the database — are the two things a test with
//! neither will happily report as working.
//!
//! # The key is derived, never accepted
//!
//! The object key is built here from the tenant, the organization and the scaffold's own slug:
//! `developer/scaffolds/{org}/{slug}-{target}.zip`. Nothing in the request chooses it, because
//! a caller-supplied key is a caller-supplied path — and [`omnion_storage`] validates keys, but a
//! validation failure at write time is a `500` the user cannot act on, not a refused request. The
//! slug rule (`[A-Za-z0-9_-]`, 3–64 characters) already guarantees the character set
//! [`omnion_storage::keys`] allows, so the derived key is valid by construction and the check is
//! a *test* rather than a runtime branch.
//!
//! # Generation is a pure function, so the same bytes come back
//!
//! The archive is rebuilt on download rather than read from the store. The stamp the zip carries
//! is derived from the row's `created_at` ([`stamp_for`]), not from the clock, so regenerating an
//! archive weeks later produces the same bytes as the download on the day it was generated. The
//! alternative — storing the bytes and serving them — is what the `object_key` implies and is
//! *not* what this does, because a stored copy that nobody revalidates is a copy of a template
//! that has since changed, and a developer comparing two starters would be comparing them to
//! themselves. The object is still written: the audit needs a durable artefact, and a generation
//! with no recorded object is a generation nobody can prove happened.
//!
//! # What is not in the response
//!
//! No bodies, no template text, no organization name — an archive is a file, and a file is
//! bytes. The `Content-Disposition` filename comes from
//! [`omnion_developer::archive::filename_for`], which builds it only from slug characters, so it
//! cannot inject a header.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use omnion_developer::archive::{entries_of, filename_for, zip};
use omnion_developer::scaffold::{ScaffoldKind, ScaffoldTarget};
use omnion_developer::templates::generate;
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::developer::organization_of;
use crate::state::AppState;

/// The content type every archive is served as.
///
/// Not `application/octet-stream`: a browser that is told octet-stream downloads a file with no
/// name it recognises and no connection to what the button said it would give. `application/zip`
/// is what the bytes are, and a `Content-Disposition` attachment completes the statement.
const ZIP_CONTENT_TYPE: &str = "application/zip";

/// One recorded generation, as the panel's list shows it.
#[derive(Debug, Serialize)]
pub struct ScaffoldRecord {
    /// The row's id.
    pub id: Uuid,
    /// Which starter.
    pub kind: String,
    /// The name it was generated for.
    pub name: String,
    /// Which environment it was aimed at.
    pub target: String,
    /// How many files it holds.
    pub file_count: usize,
    /// Its uncompressed total.
    pub byte_size: usize,
    /// When it was generated.
    pub created_at: OffsetDateTime,
}

/// The listing's envelope.
#[derive(Debug, Serialize)]
pub struct ScaffoldList {
    /// The rows, newest first.
    pub scaffolds: Vec<ScaffoldRecord>,
}

/// `GET /api/v1/dev/sdks/scaffolds` — what this tenant has generated.
///
/// Scoped to the organization by the query, not by a filter applied after the read: a list that
/// fetched every tenant's rows and dropped most of them would put one tenant's generation history
/// in another's memory before discarding it.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<ScaffoldList>, ApiError> {
    let organization_id = organization_of(&current)?;

    let rows = sqlx::query(
        r#"
        select id, kind, name, target, byte_size, created_at
        from sdk_scaffolds
        where organization_id = $1
        order by created_at desc, id desc
        limit 50
        "#,
    )
    .bind(organization_id)
    .fetch_all(state.db().pool())
    .await
    .map_err(decode_error)?;

    let scaffolds = rows
        .into_iter()
        .map(|row| {
            let kind: String = row.get("kind");
            let name: String = row.get("name");
            ScaffoldRecord {
                id: row.get("id"),
                kind: kind.clone(),
                name: name.clone(),
                target: row.get("target"),
                // `file_count` is not a column: the row is an audit of generations, not a code
                // store, so the count is regenerated from the same template the archive is
                // rather than stored beside a number that could disagree with it.
                file_count: count_files(&kind, &name),
                byte_size: row
                    .get::<Option<i64>, _>("byte_size")
                    .map(|value| value.max(0) as usize)
                    .unwrap_or(0),
                created_at: row.get("created_at"),
            }
        })
        .collect();

    Ok(Json(ScaffoldList { scaffolds }))
}

/// `GET /api/v1/dev/sdks/scaffolds/{id}/download` — the archive, as a file.
///
/// The row is read *first* and scoped to the caller's organization before anything is generated.
/// That order is the whole cross-tenant rule: a lookup that generated first and checked the row
/// second would be correct in every test and would run the generator on another tenant's slug.
pub async fn download(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&current)?;

    let row = sqlx::query(
        r#"
        select kind, name, target, created_at
        from sdk_scaffolds
        where id = $1 and organization_id = $2
        "#,
    )
    .bind(id)
    .bind(organization_id)
    .fetch_optional(state.db().pool())
    .await
    .map_err(decode_error)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "scaffold_not_found",
            "this organization has no generated scaffold with that id",
        )
    })?;

    let kind = ScaffoldKind::parse(&row.get::<String, _>("kind")).map_err(ApiError::from)?;
    let name: String = row.get("name");
    let target = ScaffoldTarget::parse(&row.get::<String, _>("target")).map_err(ApiError::from)?;
    let created_at: OffsetDateTime = row.get("created_at");

    let scaffold = generate(kind, &name, target).map_err(ApiError::from)?;
    let archive = zip(&entries_of(&scaffold), stamp_for(created_at));

    // The object is written on download, not on generation, and that is the trade this makes
    // explicit: a `POST` that stored bytes would be a generation whose artefact nobody fetched,
    // and a download that did not store would be a download with no durable record. Writing
    // here means the object exists if and only if somebody actually received the bytes.
    //
    // A store that refuses is NOT turned into a failed download. The developer already has the
    // bytes in their response; failing the request over an audit copy they never see would turn a
    // working feature into an error page. The failure is logged instead.
    let key = object_key(organization_id, &scaffold.slug(), target);
    if let Err(error) = state
        .storage()
        .put(&key, &archive.bytes, ZIP_CONTENT_TYPE)
        .await
    {
        tracing::warn!(%key, %error, "a scaffold archive could not be stored");
    }

    let filename = filename_for(&scaffold);
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                error.to_string(),
            )
        })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ZIP_CONTENT_TYPE)
        .header(header::CONTENT_DISPOSITION, disposition)
        // The archive is generated from a template on every request, so it is the one download
        // on this surface a cache must not keep: a cached copy outlives the template that made
        // it, and the developer unpacks a starter from a version of Omnion they are no longer
        // running.
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-omnion-scaffold-bytes", archive.bytes.len())
        .body(Body::from(archive.bytes))
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                error.to_string(),
            )
        })?)
}

/// `POST /api/v1/dev/sdks/scaffolds` — generate, record and return the tree.
///
/// The record is written **before** the response, and it is written in the same call: a scaffold
/// whose audit row failed is not a scaffold this platform can answer "who generated this?" about,
/// so refusing is the honest answer and a lost row is worse than a lost request. The object's own
/// bytes are written by the download — see [`download`].
pub async fn record(
    State(state): State<AppState>,
    current: CurrentSession,
    axum::extract::Json(input): axum::extract::Json<crate::routes::developer_sdks::ScaffoldInput>,
) -> Result<Json<ScaffoldRecord>, ApiError> {
    let organization_id = organization_of(&current)?;

    let kind = ScaffoldKind::parse(&input.kind).map_err(ApiError::from)?;
    let target = ScaffoldTarget::parse(&input.target).map_err(ApiError::from)?;
    let scaffold = generate(kind, &input.name, target).map_err(ApiError::from)?;

    let row: (Uuid,) = sqlx::query_as(
        r#"
        insert into sdk_scaffolds (organization_id, kind, name, target, object_key, byte_size, created_by)
        values ($1, $2, $3, $4, $5, $6, $7)
        returning id
        "#,
    )
    .bind(organization_id)
    .bind(kind.as_str())
    .bind(&scaffold.name)
    .bind(target.as_str())
    // The key is derived now and the object lands under it on download. It is `not null` in the
    // migration, and a placeholder would be a lie the audit could not correct later.
    .bind(object_key(organization_id, &scaffold.slug(), target))
    .bind(scaffold.byte_size as i64)
    .bind(current.user.id)
    .fetch_one(state.db().pool())
    .await
    .map_err(decode_error)?;

    Ok(Json(ScaffoldRecord {
        id: row.0,
        kind: scaffold.kind.as_str().to_string(),
        name: scaffold.name.clone(),
        target: target.as_str().to_string(),
        file_count: scaffold.files.len(),
        byte_size: scaffold.byte_size,
        created_at: OffsetDateTime::now_utc(),
    }))
}

/// Where a scaffold's archive lives.
///
/// Built from the tenant, the organization and the slug — all three constrained before they get
/// here — so the key is valid under [`omnion_storage::keys::validate_key`] by construction rather
/// than by a runtime check that would surface as a `500`.
pub fn object_key(organization_id: Uuid, slug: &str, target: ScaffoldTarget) -> String {
    format!(
        "developer/scaffolds/{organization_id}/{slug}-{}.zip",
        target.as_str()
    )
}

/// The instant stamped into a regenerated archive.
///
/// The row's `created_at`, floored to a whole second because the DOS timestamp has two-second
/// resolution and the writer floors anyway. Using the clock here instead would make a download
/// three weeks later produce different bytes from the same row, and the platform's own
/// reproducibility test would be a statement about the future.
fn stamp_for(created_at: OffsetDateTime) -> OffsetDateTime {
    let whole = created_at.replace_nanosecond(0).unwrap_or(created_at);
    whole
}

/// How many files a template produces, for the list row.
///
/// A template that starts failing to generate is a `500` on the list, and one that starts
/// producing a different count makes every historical row's number wrong. Both are better
/// reported as a list that says what it can and refuses nothing.
fn count_files(kind: &str, name: &str) -> usize {
    ScaffoldKind::parse(kind)
        .ok()
        .zip(ScaffoldTarget::parse("live").ok())
        .and_then(|(kind, _)| generate(kind, name, ScaffoldTarget::Live).ok())
        .map(|scaffold| scaffold.files.len())
        .unwrap_or(0)
}

/// The database's own error, mapped the way every route that speaks sqlx maps it.
///
/// `ApiError: From<sqlx::Error>` deliberately does not exist in this API (see
/// `routes::developer`), so a blanket `impl From` here would be a *second* place a database
/// failure turns into an HTTP response, and the two would drift. This is the same
/// `CoreError::Unavailable` the key routes use.
fn decode_error(error: sqlx::Error) -> ApiError {
    ApiError::from_core(omnion_core::CoreError::Unavailable {
        dependency: "developer store".into(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_object_key_is_valid_under_the_stores_own_rule() {
        // The store validates keys on write and turns a refusal into a 500 the user cannot act
        // on, so "valid by construction" has to be a test rather than a claim.
        for kind in [
            ScaffoldKind::Plugin,
            ScaffoldKind::Theme,
            ScaffoldKind::Workflow,
        ] {
            for target in [ScaffoldTarget::Live, ScaffoldTarget::Sandbox] {
                let scaffold =
                    generate(kind, "my-starter", target).expect("the template generates");
                let key = object_key(Uuid::nil(), &scaffold.slug(), target);
                assert!(
                    omnion_storage::keys::validate_key(&key).is_ok(),
                    "{key} is not a usable object key"
                );
            }
        }
    }

    #[test]
    fn two_organizations_never_share_an_object_key() {
        // The key is derived from the organization, so a collision is a cross-tenant read of
        // another tenant's generated source. `Uuid::nil()` twice would collide by construction;
        // two distinct ids must not.
        let one = object_key(
            Uuid::from_u128(1),
            "plugin-my-starter",
            ScaffoldTarget::Live,
        );
        let two = object_key(
            Uuid::from_u128(2),
            "plugin-my-starter",
            ScaffoldTarget::Live,
        );
        assert_ne!(one, two);
    }

    #[test]
    fn a_regenerated_archive_is_byte_identical_to_the_one_it_was_recorded_from() {
        // The download rebuilds the bytes rather than reading them back, so the claim that makes
        // that safe is that the rebuild is deterministic. `stamp_for` is the load-bearing part:
        // a clock there would make every download differ from every other one.
        let created = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(2026, time::Month::October, 2).unwrap(),
            time::Time::from_hms(9, 30, 0).unwrap(),
        );
        let scaffold = generate(ScaffoldKind::Plugin, "determinism", ScaffoldTarget::Live)
            .expect("the template generates");
        let one = zip(&entries_of(&scaffold), stamp_for(created)).bytes;
        let two = zip(&entries_of(&scaffold), stamp_for(created)).bytes;
        assert_eq!(one, two);
        // And the bytes are a real archive, not two equal empty vecs. Reading an entry back is
        // the only assertion here that can tell a deterministic *archive* from a deterministic
        // zero.
        assert!(
            omnion_developer::archive::read(&one, "plugin-determinism/omnion.manifest.json")
                .is_some(),
            "the regenerated bytes are not a readable archive"
        );
    }

    #[test]
    fn the_stamp_is_floored_rather_than_rounded_up() {
        // A stamp 900ms past a second, floored, is the second it started in. Rounding up would
        // put the archive's own time after the moment it was generated.
        let stamp = OffsetDateTime::new_utc(
            time::Date::from_calendar_date(2026, time::Month::October, 2).unwrap(),
            time::Time::from_hms(9, 30, 0).unwrap(),
        ) + time::Duration::milliseconds(900);
        assert_eq!(stamp_for(stamp).nanosecond(), 0);
        assert_eq!(stamp_for(stamp).second(), 0);
    }

    #[test]
    fn a_target_is_part_of_the_key_so_live_and_sandbox_do_not_overwrite() {
        let live = object_key(Uuid::nil(), "plugin-demo", ScaffoldTarget::Live);
        let sandbox = object_key(Uuid::nil(), "plugin-demo", ScaffoldTarget::Sandbox);
        assert_ne!(live, sandbox);
        assert!(sandbox.ends_with("-sandbox.zip"), "{sandbox}");
    }
}
