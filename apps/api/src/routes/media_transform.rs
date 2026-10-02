//! `/api/v1/media/transformation-presets` and the preset read path (REQ-010, slice 3).
//!
//! A preset is a named transformation, and the name is part of a public URL: a page asks for
//! `/api/v1/media/{id}/raw?preset=card` and gets the same pixels for as long as the preset and
//! the file are unchanged. Four rules hold across this file:
//!
//! * **The name is validated twice, in step.** The API and the migration's check constraint
//!   accept the same character class. A name the API accepts and the database refuses is a 500 a
//!   caller cannot act on; a name the API refuses and the database accepts is a row no URL can
//!   ever reach.
//! * **An unknown preset falls back to the original bytes** rather than 404-ing. A page whose
//!   preset was deleted must still render its image; a 404 would replace a picture with a broken
//!   one. The response says which happened, through a header, so an operator can see it.
//! * **A build is inserted after its bytes are stored**, never before: a cache row pointing at
//!   an object that was never written is worse than a miss, because it is invisible until
//!   somebody requests the URL.
//! * **Derivatives are cacheable for a year.** The key is content-addressed, so the same inputs
//!   always produce the same object and a CDN can hold it without revalidating — but only when
//!   the client is told it may.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response as AxumResponse;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_media::{
    Fit, ImageFormat, MediaError, NewDerivative, NewPreset, Preset, Recipe, derivative_filename,
    find_derivative, insert_derivative, list_presets, served_for, transform_bytes,
    validate_preset_name,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::routes::media::site_in_scope;
use crate::routes::media_files::file_in_scope;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One preset, as the settings screen reads it.
#[derive(Debug, Serialize)]
pub struct PresetBody {
    /// Preset id.
    pub id: Uuid,
    /// The name that appears in the URL.
    pub name: String,
    /// Target width, when the preset names one.
    pub width: Option<i32>,
    /// Target height, when the preset names one.
    pub height: Option<i32>,
    /// How the source is fitted into the box.
    pub fit: String,
    /// Emitted format.
    pub format: String,
    /// Encoder quality, 1-100.
    pub quality: i32,
    /// A human-readable summary: `1200 x 630 · cover · WebP q80`.
    pub summary: String,
    /// An example URL for a given file, so the operator can copy one and see it work.
    pub example_query: String,
}

/// A preset as the caller describes it.
#[derive(Debug, Deserialize)]
pub struct PresetInput {
    /// The name that appears in the URL.
    pub name: String,
    /// Target width, when the preset names one.
    pub width: Option<i32>,
    /// Target height, when the preset names one.
    pub height: Option<i32>,
    /// How the source is fitted into the box; defaults to `cover`.
    #[serde(default)]
    pub fit: Option<String>,
    /// Emitted format; defaults to `webp`.
    #[serde(default)]
    pub format: Option<String>,
    /// Encoder quality, 1-100; defaults to 80.
    #[serde(default)]
    pub quality: Option<i32>,
    /// Watermark applied on top, if any.
    #[serde(default)]
    pub watermark_media_id: Option<Uuid>,
}

impl PresetInput {
    /// Reduce the request into the crate's own type, which is where validation happens.
    ///
    /// Defaults are applied *here* rather than in the database so that the row always carries an
    /// explicit value: a preset row that leaves `fit` to a column default becomes impossible to
    /// reason about once the default is changed.
    fn into_new(self) -> std::result::Result<NewPreset, ApiError> {
        let fit = match self.fit.as_deref() {
            Some(raw) => Fit::parse(raw)?,
            None => Fit::Cover,
        };
        let format = match self.format.as_deref() {
            Some(raw) => ImageFormat::parse(raw)?,
            None => ImageFormat::WebP,
        };
        Ok(omnion_media::validate_new(NewPreset {
            name: self.name,
            width: self.width,
            height: self.height,
            fit,
            format,
            quality: self.quality.unwrap_or(80),
            watermark_media_id: self.watermark_media_id,
        })?)
    }
}

/// The query string of the raw route, as the panel sends it.
#[derive(Debug, Deserialize)]
pub struct RawQuery {
    /// The named transformation to apply.
    pub preset: Option<String>,
}

/// The result of asking for a derivative.
#[derive(Debug, Serialize)]
pub struct DerivativeBody {
    /// Object key of the generated bytes — the CDN-facing identity of this derivative.
    pub cache_key: String,
    /// Pixel width of the result.
    pub width: i32,
    /// Pixel height of the result.
    pub height: i32,
    /// Size of the generated bytes.
    pub size_bytes: u64,
    /// Content type of the generated bytes.
    pub content_type: String,
    /// Whether this call built it or read it from the cache.
    pub built: bool,
}

/// The label the settings screen shows for a format, which is not the value the URL carries.
fn format_label(format: ImageFormat) -> &'static str {
    ImageFormat::ALL
        .iter()
        .find(|(value, _)| *value == format.as_str())
        .map_or("WebP", |(_, label)| *label)
}

impl PresetBody {
    /// Describe one preset for the panel.
    fn build(preset: &Preset) -> Self {
        let fit = Fit::parse(&preset.fit).unwrap_or(Fit::Cover);
        let format = ImageFormat::parse(&preset.format).unwrap_or(ImageFormat::WebP);
        let box_label = match (preset.width, preset.height) {
            (Some(w), Some(h)) => format!("{w} x {h}"),
            (Some(w), None) => format!("{w} wide"),
            (None, Some(h)) => format!("{h} tall"),
            (None, None) => "original size".to_string(),
        };
        Self {
            id: preset.id,
            name: preset.name.clone(),
            width: preset.width,
            height: preset.height,
            fit: fit.as_str().to_string(),
            format: format.as_str().to_string(),
            quality: preset.quality,
            summary: format!(
                "{box_label} · {} · {} q{}",
                fit.as_str(),
                format_label(format),
                preset.quality
            ),
            example_query: format!("?preset={}", preset.name),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Preset CRUD
// ---------------------------------------------------------------------------------------------

/// A site's presets, as the settings screen reads them.
///
/// A named type rather than a `serde_json::Value`: a response body that is a map is a response
/// body nothing can check. Naming it means a change to the wire format is a compile error here
/// instead of a surprise in a browser.
#[derive(Debug, Serialize)]
pub struct PresetListResponse {
    /// Every preset, in name order.
    pub presets: Vec<PresetBody>,
}

/// List a site's presets.
pub async fn list(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<Json<PresetListResponse>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let presets = list_presets(state.db().pool(), site.id).await?;
    let body: Vec<PresetBody> = presets.iter().map(PresetBody::build).collect();
    Ok(Json(PresetListResponse { presets: body }))
}

/// Create a preset.
pub async fn create(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Query(query): Query<SiteQuery>,
    Json(input): Json<PresetInput>,
) -> std::result::Result<(StatusCode, Json<PresetBody>), ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let preset = omnion_media::create_preset(state.db().pool(), site.id, input.into_new()?).await?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("media.preset_created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({ "preset": preset.name, "site_id": site.id })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.preset_created")
            .target("media_preset", preset.id.to_string())
            .metadata(json!({ "site_id": site.id, "name": preset.name }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(PresetBody::build(&preset))))
}

/// Edit a preset.
///
/// An edit deliberately does **not** touch the existing derivatives: their keys already include
/// the old definition, so they become unreachable rather than stale, and a page still asking for
/// the previous pixels keeps the object it was cached with.
pub async fn update(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(preset_id): Path<Uuid>,
    Query(query): Query<SiteQuery>,
    Json(input): Json<PresetInput>,
) -> std::result::Result<Json<PresetBody>, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let before = omnion_media::require_preset_by_id(state.db().pool(), site.id, preset_id).await?;
    let preset =
        omnion_media::update_preset(state.db().pool(), site.id, preset_id, input.into_new()?)
            .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.preset_updated")
            .target("media_preset", preset.id.to_string())
            .metadata(json!({
                "site_id": site.id,
                "name": preset.name,
                "previous_name": before.name,
            }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(PresetBody::build(&preset)))
}

/// Delete a preset and, by cascade, every derivative built from it.
pub async fn delete(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Path(preset_id): Path<Uuid>,
    Query(query): Query<SiteQuery>,
) -> std::result::Result<StatusCode, ApiError> {
    let site = site_in_scope(&state, &current, query.site_id).await?;
    let preset = omnion_media::require_preset_by_id(state.db().pool(), site.id, preset_id).await?;

    if !omnion_media::delete_preset(state.db().pool(), site.id, preset_id).await? {
        return Err(MediaError::PresetNotFound { name: preset.name }.into());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "media.preset_deleted")
            .target("media_preset", preset_id.to_string())
            .metadata(json!({ "site_id": site.id, "name": preset.name }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// The read path
// ---------------------------------------------------------------------------------------------

/// Serve one file, optionally through a preset.
///
/// Without `?preset=` this is the ordinary read. With it, the answer is the derivative: built on
/// the first request, served from the cache on every later one, and cacheable by a CDN for a year
/// because its key is a hash of the inputs.
pub async fn raw_with_preset(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(media_id): Path<Uuid>,
    Query(query): Query<RawQuery>,
    headers: axum::http::HeaderMap,
) -> std::result::Result<AxumResponse, ApiError> {
    let media = file_in_scope(&state, &current, media_id).await?;
    // One gate, before every branch — the original, a cached derivative and a freshly built
    // one are the same file, and a derivative of a quarantined file is still a quarantined
    // file. Putting the check inside `serve_original` instead would leave the *cached* path
    // (the one a published page actually fetches, served with `max-age=31536000`) open.
    crate::routes::media::ensure_servable(&state, &current, &media).await?;
    let range = crate::routes::media::range_header(&headers);

    let Some(requested) = query
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        // No preset: the original bytes, through the same headers as before.
        return serve_original(&state, &media, range, &headers).await;
    };

    let name = match validate_preset_name(requested) {
        Ok(name) => name,
        Err(_err) => {
            // A malformed name cannot match a preset, so this is the fallback case, not a 400:
            // the page still gets its image. The reason is logged rather than returned, because
            // the response's job is to be an image, not to explain why the fallback happened.
            tracing::debug!(
                preset = requested,
                "unusable preset name, serving the original"
            );
            return serve_original(&state, &media, range, &headers).await;
        }
    };

    let preset =
        match omnion_media::find_preset_by_name(state.db().pool(), media.site_id, &name).await? {
            Some(preset) => preset,
            None => {
                // The spec is explicit: an unknown preset falls back to the original. A 404 here would
                // replace a picture on a live page with a broken one the moment somebody renames a
                // preset, and the page's own markup would have to change too.
                tracing::info!(
                    media_id = %media.id,
                    preset = %name,
                    "unknown preset, serving the original"
                );
                return serve_original(&state, &media, range, &headers).await;
            }
        };

    let served = served_for(&preset, media.site_id, &media.checksum);

    if let Some(cached) = find_derivative(state.db().pool(), &served.cache_key).await?
        && let Ok(bytes) = state.storage().get(&cached.storage_key).await
    {
        return Ok(derivative_response(
            Served {
                bytes,
                content_type: cached.content_type.clone(),
                // The identity of the *pixels*, which is what a caller reads and looks up.
                cache_key: cached.cache_key.clone(),
                // The object they live at, which is what the store answers on.
                storage_key: cached.storage_key.clone(),
                built: false,
            },
            derivative_filename(&media.filename, preset_recipe(&preset).format),
        ));
    }

    build_derivative(&state, &media, &preset, &served).await
}

/// Build one derivative and record it.
async fn build_derivative(
    state: &AppState,
    media: &omnion_media::MediaFile,
    preset: &Preset,
    served: &omnion_media::Served,
) -> std::result::Result<AxumResponse, ApiError> {
    let source = state.storage().get(&media.storage_key).await?;
    let transformed = transform_bytes(&media.content_type, &source, &served.recipe)?;

    // The object goes first, then the row. A failure between the two leaves an object nothing
    // points at (reclaimable), which is a far better state than a row pointing at bytes that do
    // not exist (a 404 that looks like a bug).
    let stored = state
        .storage()
        .put(
            &served.storage_key,
            &transformed.bytes,
            &transformed.content_type,
        )
        .await?;

    let derivative = insert_derivative(
        state.db().pool(),
        NewDerivative {
            media_id: media.id,
            preset_id: preset.id,
            cache_key: served.cache_key.clone(),
            storage_key: served.storage_key.clone(),
            content_type: transformed.content_type.clone(),
            size_bytes: stored.size_bytes as i64,
            width: transformed.width as i32,
            height: transformed.height as i32,
            source_checksum: media.checksum.clone(),
        },
    )
    .await?;

    Ok(derivative_response(
        Served {
            bytes: transformed.bytes,
            content_type: transformed.content_type,
            cache_key: derivative.cache_key,
            storage_key: derivative.storage_key,
            built: true,
        },
        derivative_filename(&media.filename, served.recipe.format),
    ))
}

/// The original bytes, with the same guard the un-preset route applies.
///
/// The `Range` header is honoured here rather than only on the bare `/raw` path, because a
/// derivative and the original are the *same file* to a client: a page that asks for
/// `?preset=card` because the preset was deleted must still be able to window what comes back,
/// and a client that learns "ranges work only without a preset" stops asking for them.
async fn serve_original(
    state: &AppState,
    media: &omnion_media::MediaFile,
    range: Option<&str>,
    request_headers: &axum::http::HeaderMap,
) -> std::result::Result<AxumResponse, ApiError> {
    // This is the panel's real read path — `raw_with_preset` falls through to it whenever no
    // preset is named — so it is the one a thumbnail in the library grid and a preview pane both
    // hit. A validator added to `/media/{id}/raw` alone would have been one the walk never
    // reaches: the route that *looks* like the read path is this one.
    let conditional = crate::routes::media::conditional_headers(
        request_headers,
        &media.checksum,
        media.updated_at.or(Some(media.created_at)),
    );
    if conditional.verdict == omnion_media::validators::Conditional::NotModified {
        return crate::routes::media::not_modified(&conditional);
    }
    let plan = omnion_media::serve_plan(&media.content_type);
    let (status, bytes) =
        crate::routes::media::read_window(state, &media.storage_key, range, media.size()).await?;
    let ranges = crate::routes::media::range_header_values(range, media.size(), &bytes)?;

    let mut response = AxumResponse::new(Body::from(bytes));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(plan.content_type)?);
    headers.insert(
        header::CONTENT_DISPOSITION,
        header_value(&format!(
            "{}; filename=\"{}\"",
            plan.disposition, media.filename
        ))?,
    );
    headers.insert(header::CACHE_CONTROL, header_value("private, max-age=300")?);
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, header_value("nosniff")?);
    crate::routes::media::apply_range_headers(headers, ranges)?;
    crate::routes::media::apply_validators(headers, &conditional)?;
    Ok(response)
}

/// What a derivative response is made of, kept apart from the headers.
///
/// The cache key and the storage key are two different strings that both appear in this module and
/// are trivially confused — one is the identity of the pixels, the other is where they live, and
/// the first version of this function took one parameter where it needed two. That is why they are
/// fields of one struct here rather than three positional `&str`s.
struct Served {
    /// The encoded bytes.
    bytes: Vec<u8>,
    /// Content type of those bytes.
    content_type: String,
    /// Hash of the inputs — the identity of the pixels, and the value of the header.
    cache_key: String,
    /// Object key the bytes live at.
    storage_key: String,
    /// Whether this call built them or read them from the cache.
    built: bool,
}

/// One response for a generated derivative.
fn derivative_response(derivative: Served, filename: String) -> AxumResponse {
    let mut response = axum::response::Response::new(Body::from(derivative.bytes));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&derivative.content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    // A year, because the URL's bytes are addressed by a hash of their inputs: the same URL
    // always means the same pixels, and no amount of time changes that. `immutable` tells a
    // browser the same, which is what stops a re-upload from looking like a cache bug.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // The *cache key*, not the object key: this header is the identity of the pixels, and it is
    // the value `media_derivatives.cache_key` holds. The object key embeds the cache key inside
    // a path, so a caller that read the header and looked the value up in that column would find
    // nothing — a header that looks like an identifier and is not one is worse than none.
    if let Ok(value) = HeaderValue::from_str(&derivative.cache_key) {
        headers.insert("x-omnion-derivative", value);
    }
    if let Ok(value) = HeaderValue::from_str(&derivative.storage_key) {
        headers.insert("x-omnion-object", value);
    }
    headers.insert(
        "x-omnion-cache",
        HeaderValue::from_static(if derivative.built { "1" } else { "0" }),
    );
    response
}

/// The recipe of a stored preset, for the filename helper.
fn preset_recipe(preset: &Preset) -> Recipe {
    Recipe::of(preset)
}

// ---------------------------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------------------------

/// The site a preset route operates on, taken from the query string.
#[derive(Debug, Deserialize)]
pub struct SiteQuery {
    /// Site the presets belong to.
    pub site_id: Uuid,
}

/// Build one response header value, refusing anything unusable.
fn header_value(value: &str) -> std::result::Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|err| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            err.to_string(),
        )
    })
}

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> std::result::Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// The instant a preset row was last written, for the settings screen's "edited" column.
#[must_use]
pub fn edited_at(preset: &Preset) -> OffsetDateTime {
    preset.updated_at
}
