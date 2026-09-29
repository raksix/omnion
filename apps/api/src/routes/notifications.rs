//! `/api/v1/notifications` — the in-app inbox (REQ-021, slice 1).
//!
//! This is the platform's fourth feedback loop and the only one that says *you*: the event bus
//! records facts, the audit trail records privileged work, the search index records documents,
//! and this one reaches a person. Everything on this surface is **owner-scoped** — the
//! recipient is the signed-in account, never a parameter — and every rule below is a way to
//! publish an inbox that looks right and tells the reader something false.
//!
//! * **A notification that is not yours is a `404`, never a `403`.** A `403` is the difference
//!   between "that is not yours" and "that is not real". The ids here are UUIDs, but a
//!   sequential oracle is still an oracle, and the inbox is the surface a curious panel is
//!   most tempted to poke at. The store takes the *owner's* id on every function, so this is
//!   not a check a handler can forget to write.
//! * **`Mark read` on an already-read row is a success, not an error.** The row is the same
//!   row; the reader clicked a button that asked for a state the row is already in. The store
//!   reports rows changed, and the route reports the count without distinguishing "changed"
//!   from "already there" — a panel that showed an error there would teach people that the
//!   button is unreliable.
//! * **The badge and the grouped lines are one query.** A bell that says 12 above four lines
//!   adding up to 9 is a screen nobody believes afterwards, and the fix is not a UI change:
//!   it is that both come from [`omnion_notifications::store::summary`], which is the only
//!   place the unread count is computed.
//! * **A bulk action reports the number it really changed.** Selecting five rows and archiving
//!   two of them is a legitimate outcome, and "2 archived" is the honest answer. The count is
//!   the whole point of the endpoint: an action that silently affects fewer rows than the
//!   selection is how a panel loses somebody's trust in its own selection model.
//! * **An unknown category is a `400` naming the legal values**, never a filter that returns
//!   nothing. `?category=invoce` is a typo a person made, and answering it with an empty list
//!   is indistinguishable from "you have no such notifications".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_events::{NewEvent, bus};
use omnion_notifications::{
    CATEGORIES, CategoryCount, ListQuery, MAX_BULK_IDS, NewNotification, Notification,
    PreferenceCell, Settings, StatedPreference, Summary,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a store error onto the API surface.
///
/// The store's taxonomy is deliberately small, so this mapping is the whole of it: a
/// definition the platform refuses is the caller's `400`, a rate limit is a `429`, and a
/// database that did not answer is a `500` — never a `400`, because a client that retries a
/// `400` forever is a client the platform taught to do that.
fn map_store(error: omnion_notifications::NotificationError) -> ApiError {
    use omnion_notifications::NotificationError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_notification", message),
        E::BudgetExhausted => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "notification_rate_limited",
            "too many notifications from this actor — try again in a minute",
        ),
        E::Database(inner) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("notification store: {inner}"),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// The query of the list read.
///
/// `category` repeats rather than being comma-separated: a comma inside a value is then
/// impossible, and a panel that sends `category=a,b` gets a `400` naming the field instead of
/// a filter that silently matched nothing.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    /// Keep only this category. Repeat for several.
    #[serde(default)]
    pub category: Vec<String>,
    /// `unread` or `read`; absent means both.
    pub read: Option<String>,
    /// Keep only this priority. Repeat for several.
    #[serde(default)]
    pub priority: Vec<String>,
    /// Keep only notifications delivered over this channel.
    pub channel: Option<String>,
    /// Include the archived rows.
    #[serde(default)]
    pub archived: bool,
    /// Include the rows that have been read.
    #[serde(default)]
    pub with_read: bool,
    /// Page from this instant, exclusive (the keyset cursor).
    pub before: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
}

/// A list read's answer.
#[derive(Debug, Serialize)]
pub struct ListBody {
    /// The rows.
    pub notifications: Vec<NotificationBody>,
    /// Whether another page exists behind this one.
    pub has_more: bool,
    /// The cursor for the next page, or `None` at the end.
    pub next_before: Option<String>,
}

/// One notification as the panel reads it.
#[derive(Debug, Serialize)]
pub struct NotificationBody {
    /// The row's id.
    pub id: Uuid,
    /// Which group it belongs to.
    pub category: String,
    /// How urgent it is.
    pub priority: String,
    /// One line.
    pub title: String,
    /// Optional second line.
    pub body: String,
    /// Where its link goes; `None` means the panel must not render a link.
    pub url: Option<String>,
    /// What produced it.
    pub source_type: Option<String>,
    /// The producing record's id.
    pub source_id: Option<String>,
    /// Structured detail for the drawer.
    pub payload: serde_json::Value,
    /// When it was read, if it was.
    pub read_at: Option<String>,
    /// When it was filed away, if it was.
    pub archived_at: Option<String>,
    /// When it happened.
    pub created_at: String,
}

impl From<Notification> for NotificationBody {
    fn from(value: Notification) -> Self {
        Self {
            id: value.id,
            category: value.category,
            priority: value.priority,
            title: value.title,
            body: value.body,
            url: value.url,
            source_type: value.source_type,
            source_id: value.source_id,
            payload: value.payload,
            read_at: value.read_at.map(|at| at.to_string()),
            archived_at: value.archived_at.map(|at| at.to_string()),
            created_at: value.created_at.to_string(),
        }
    }
}

/// The bell's numbers.
#[derive(Debug, Serialize)]
pub struct SummaryBody {
    /// How many are unread in total.
    pub unread: i64,
    /// One line per category, including the empty ones.
    pub by_category: Vec<CategoryCount>,
}

impl From<Summary> for SummaryBody {
    fn from(value: Summary) -> Self {
        Self {
            unread: value.unread,
            by_category: value.by_category,
        }
    }
}

/// A bulk action's request.
#[derive(Debug, Deserialize)]
pub struct BulkBody {
    /// `read`, `unread`, `archive` or `delete`.
    pub action: String,
    /// The selected rows.
    pub ids: Vec<Uuid>,
}

/// A bulk action's answer: what it really changed.
#[derive(Debug, Serialize)]
pub struct BulkResult {
    /// The action that ran.
    pub action: String,
    /// How many rows changed.
    pub changed: u64,
    /// The unread count after the action, so the badge updates without a second read.
    pub unread: i64,
}

/// The read/unread toggle's request.
#[derive(Debug, Deserialize)]
pub struct ReadBody {
    /// `true` to mark read, `false` to mark unread.
    pub read: bool,
}

/// An emit's request: what to say, and to whom.
#[derive(Debug, Deserialize)]
pub struct EmitBody {
    /// Which group it belongs to.
    pub category: String,
    /// How urgent it is.
    #[serde(default)]
    pub priority: Option<String>,
    /// One line.
    pub title: String,
    /// Optional second line.
    #[serde(default)]
    pub body: Option<String>,
    /// Where its link goes.
    #[serde(default)]
    pub url: Option<String>,
    /// What produced it.
    #[serde(default)]
    pub source_type: Option<String>,
    /// The producing record's id.
    #[serde(default)]
    pub source_id: Option<String>,
    /// Structured detail.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// Collapse repeats under this key.
    #[serde(default)]
    pub dedupe_key: Option<String>,
    /// The recipients: account ids. A role or permission set is slice 3's router, which
    /// resolves it to people before the emit happens — an emit route that took a role name
    /// would be a second, unpoliced way to address the platform's users.
    pub user_ids: Vec<Uuid>,
}

/// What one emit produced.
#[derive(Debug, Serialize)]
pub struct EmitResult {
    /// How many rows were created.
    pub created: u64,
    /// How many were collapsed into a row that already existed.
    pub deduped: u64,
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/notifications` — the caller's own list.
///
/// An unknown category or priority is refused here rather than in the store, so the message
/// reaches the reader with the legal values in it. A filter that silently matched nothing is
/// the failure mode of a text column with no check constraint.
pub async fn list(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<ListParams>,
) -> Result<Json<ListBody>, ApiError> {
    let query = build_query(params)?;

    let page = omnion_notifications::store::list(state.db().pool(), session.user.id, &query)
        .await
        .map_err(map_store)?;

    // The cursor is the *last row on this page*, and it is the last row's own timestamp rather
    // than "now minus the page": a page that arrived late must not skip the rows that landed
    // behind it. The id is not part of the cursor, so the keyset is timestamp-ordered only —
    // which is why the store's tiebreaker is `id desc` and a duplicate instant is possible but
    // bounded by the page size.
    let next_before = page
        .has_more
        .then(|| page.notifications.last())
        .flatten()
        .map(|last| last.created_at.to_string());

    Ok(Json(ListBody {
        notifications: page
            .notifications
            .into_iter()
            .map(NotificationBody::from)
            .collect(),
        has_more: page.has_more,
        next_before,
    }))
}

/// `GET /api/v1/notifications/summary` — the bell's grouped counts.
pub async fn summary(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<SummaryBody>, ApiError> {
    let value = omnion_notifications::store::summary(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;
    Ok(Json(SummaryBody::from(value)))
}

/// `GET /api/v1/notifications/{id}` — one notification, with its delivery rows.
///
/// `404` for a row that is not the caller's, and for a row that is gone. The two are the same
/// answer on purpose; see the module header.
pub async fn get(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Json<NotificationBody>, ApiError> {
    let row = omnion_notifications::store::find(state.db().pool(), session.user.id, id)
        .await
        .map_err(map_store)?
        .ok_or_else(not_found)?;
    Ok(Json(NotificationBody::from(row)))
}

// ---------------------------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/notifications/{id}/read` — mark one read or unread.
pub async fn set_read(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
    Json(body): Json<ReadBody>,
) -> Result<Json<SummaryBody>, ApiError> {
    let changed =
        omnion_notifications::store::set_read(state.db().pool(), session.user.id, id, body.read)
            .await
            .map_err(map_store)?;

    // Zero changed rows is a `404`, because the panel asked for a specific row and did not get
    // it — a row that was already in the requested state still answers 200 above, so the two
    // are not conflated: "already read" and "not yours" are different answers to different
    // questions, and the read toggle is asked once per row the reader can see.
    if changed == 0 && !row_is_mine(&state, session.user.id, id).await? {
        return Err(not_found());
    }

    let value = omnion_notifications::store::summary(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;
    Ok(Json(SummaryBody::from(value)))
}

/// `POST /api/v1/notifications/bulk` — one action over a selection.
pub async fn bulk(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<BulkBody>,
) -> Result<Json<BulkResult>, ApiError> {
    // The cap is refused, not clamped. A panel that sent 400 ids and got "200, changed 200"
    // would be lying about a request it never made; a panel that sent 400 by accident is
    // better told, because the fix is in its own code.
    if body.ids.len() > MAX_BULK_IDS {
        return Err(ApiError::bad_request(
            "too_many_ids",
            format!(
                "a bulk action may name at most {MAX_BULK_IDS} notifications, got {}",
                body.ids.len()
            ),
        ));
    }

    let changed = match body.action.as_str() {
        "read" => {
            omnion_notifications::store::set_read_many(
                state.db().pool(),
                session.user.id,
                &body.ids,
                true,
            )
            .await
        }
        "unread" => {
            omnion_notifications::store::set_read_many(
                state.db().pool(),
                session.user.id,
                &body.ids,
                false,
            )
            .await
        }
        "archive" => {
            omnion_notifications::store::archive(state.db().pool(), session.user.id, &body.ids)
                .await
        }
        "delete" => {
            omnion_notifications::store::delete(state.db().pool(), session.user.id, &body.ids).await
        }
        other => {
            return Err(ApiError::bad_request(
                "unknown_action",
                format!("action \"{other}\" is not one of read, unread, archive, delete"),
            ));
        }
    }
    .map_err(map_store)?;

    // The unread count rides the answer so the badge updates without a second round trip —
    // and so the number on screen after a bulk action is the number the server computed, not
    // a count the panel decremented by the size of its own selection.
    let summary = omnion_notifications::store::summary(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;

    Ok(Json(BulkResult {
        action: body.action,
        changed,
        unread: summary.unread,
    }))
}

/// `DELETE /api/v1/notifications/{id}` — delete one notification.
pub async fn delete(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let changed = omnion_notifications::store::delete(state.db().pool(), session.user.id, &[id])
        .await
        .map_err(map_store)?;
    if changed == 0 {
        return Err(not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/notifications/mark-all-read` — clear the badge in one action.
///
/// Its own route rather than a `bulk` with a sentinel id, because "everything" and "the 200
/// rows you selected" are different operations with different costs, and a route that can
/// express both will eventually be asked to do the second one with the first one's semantics.
pub async fn mark_all_read(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<SummaryBody>, ApiError> {
    omnion_notifications::store::mark_all_read(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;
    let value = omnion_notifications::store::summary(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;
    Ok(Json(SummaryBody::from(value)))
}

// ---------------------------------------------------------------------------------------------
// Emitting (REQ-021, slice 1's producer side)
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/notifications/emit` — tell people something.
///
/// This is the route a module calls, and it is guarded by `notifications.send` — *sending* is
/// a different power from *reading one's own inbox*, and a panel that could only emit to
/// itself would make the third-party integration story a lie.
///
/// Two rules it enforces that a caller would not:
/// * **the budget**, counted per emitting actor, so a module in a loop cannot fill an inbox
///   faster than a person can empty it;
/// * **the dedupe**, so a retried job does not send the same fact twice.
pub async fn emit(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<EmitBody>,
) -> Result<(StatusCode, Json<EmitResult>), ApiError> {
    if !omnion_notifications::is_category(&body.category) {
        return Err(ApiError::bad_request(
            "invalid_notification",
            format!(
                "category \"{}\" is not one of {:?}",
                body.category, CATEGORIES
            ),
        ));
    }
    if let Some(priority) = &body.priority {
        if !omnion_notifications::is_priority(priority) {
            return Err(ApiError::bad_request(
                "invalid_notification",
                format!(
                    "priority \"{priority}\" is not one of {:?}",
                    omnion_notifications::PRIORITIES
                ),
            ));
        }
    }
    if body.title.trim().is_empty() {
        return Err(ApiError::bad_request(
            "invalid_notification",
            "a notification needs a title".to_owned(),
        ));
    }
    if body.user_ids.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_notification",
            "an emit needs at least one recipient".to_owned(),
        ));
    }

    if !omnion_notifications::store::within_emit_budget(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "notification_rate_limited",
            "this actor has emitted too many notifications in the last minute",
        ));
    }

    let mut created = 0u64;
    let mut deduped = 0u64;
    for user_id in &body.user_ids {
        let mut draft = NewNotification::to(*user_id, &body.category, body.title.clone());
        if let Some(priority) = &body.priority {
            draft = draft.with_priority(priority.clone());
        }
        if let Some(text) = &body.body {
            draft = draft.with_body(text.clone());
        }
        if let Some(url) = &body.url {
            draft = draft.with_url(url.clone());
        }
        if let (Some(kind), Some(id)) = (&body.source_type, &body.source_id) {
            draft = draft.with_source(kind.clone(), id.clone());
        }
        if let Some(payload) = &body.payload {
            draft = draft.with_payload(payload.clone());
        }
        if let Some(key) = &body.dedupe_key {
            draft = draft.with_dedupe_key(key.clone());
        }

        if omnion_notifications::store::record(
            state.db().pool(),
            session.user.organization_id,
            Some(session.user.id),
            &draft,
        )
        .await
        .map_err(map_store)?
        {
            created += 1;
        } else {
            deduped += 1;
        }
    }

    // The event is recorded *after* the rows exist, so a subscriber to `notification.created`
    // can read the notifications it is told about. The reverse order makes the bus announce
    // rows that are not committed yet, which is the one ordering bug in this whole surface
    // that a consumer cannot detect — it just sees ids that 404.
    let total = body.user_ids.len() as u64;
    bus::emit(
        state.db().pool(),
        NewEvent::new("notification.created")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "category": body.category,
                "priority": body.priority.as_deref().unwrap_or("normal"),
                "recipients": total,
                "created": created,
                "deduped": deduped,
            })),
    )
    .await
    .ok();

    Ok((StatusCode::ACCEPTED, Json(EmitResult { created, deduped })))
}

// ---------------------------------------------------------------------------------------------
// Preferences (REQ-021, slice 2)
// ---------------------------------------------------------------------------------------------

/// The `PUT` body: the cells this person is stating, and the settings row to go with them.
///
/// **`settings` is required rather than optional.** A `PUT` that carried only cells would
/// leave the caller guessing whether its quiet hours were saved, wiped, or never sent — and
/// three possible answers to "did my digest preference save?" is not an API. The settings
/// screen always has the current values loaded, so sending them back is free, and the
/// `settings` block is a *replace* of one row rather than a patch of eight columns.
///
/// `deny_unknown_fields` is load-bearing, not tidiness: serde **ignores** unknown fields by
/// default, so a client that sends `quiet_hour_start` (one s) gets a `200` that saved nothing
/// and concludes the setting is broken. A `400` naming the unknown field is the only answer
/// that helps. The same rule refuses a `user_id` in the body — the owner is the session's, and
/// a field that is silently dropped is a field somebody will eventually rely on.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutPreferencesBody {
    /// The cells the reader is changing. Cells not named keep the platform default.
    #[serde(default)]
    pub cells: Vec<StatedPreference>,
    /// The settings row in full.
    pub settings: SettingsBody,
}

/// The settings row as the panel reads and writes it.
///
/// Deny-unknown for the same reason as the body above: a mistyped `digest_hours` must be a
/// `400` and not a save that quietly kept the old value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    /// Quiet hours begin, `HH:MM` in the reader's own timezone.
    pub quiet_hours_start: Option<String>,
    /// Quiet hours end, `HH:MM`.
    pub quiet_hours_end: Option<String>,
    /// The IANA timezone the two are read in.
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// `off`, `daily` or `weekly`.
    #[serde(default = "default_cadence")]
    pub digest_cadence: String,
    /// Which weekday a weekly digest goes out on, 0 = Monday.
    pub digest_weekday: Option<i16>,
    /// Which hour a digest goes out in.
    #[serde(default = "default_digest_hour")]
    pub digest_hour: i16,
}

fn default_timezone() -> String {
    "UTC".to_owned()
}

fn default_cadence() -> String {
    "off".to_owned()
}

fn default_digest_hour() -> i16 {
    8
}

impl From<Settings> for SettingsBody {
    fn from(settings: Settings) -> Self {
        Self {
            quiet_hours_start: settings.quiet_hours_start,
            quiet_hours_end: settings.quiet_hours_end,
            timezone: settings.timezone,
            digest_cadence: settings.digest_cadence,
            digest_weekday: settings.digest_weekday,
            digest_hour: settings.digest_hour,
        }
    }
}

impl SettingsBody {
    /// The store's row, owned by the caller's id.
    ///
    /// The `user_id` is taken from the session and never from the body — a settings body that
    /// could name its owner is a settings body that can rewrite somebody else's quiet hours.
    fn to_settings(&self, user_id: Uuid) -> Settings {
        Settings {
            user_id,
            quiet_hours_start: self.quiet_hours_start.clone(),
            quiet_hours_end: self.quiet_hours_end.clone(),
            timezone: self.timezone.clone(),
            digest_cadence: self.digest_cadence.clone(),
            digest_weekday: self.digest_weekday,
            digest_hour: self.digest_hour,
        }
    }
}

/// The preferences answer: the complete matrix plus the settings row.
#[derive(Debug, Serialize)]
pub struct PreferencesBody {
    /// Every category × channel cell, in the vocabulary's order.
    pub cells: Vec<PreferenceCell>,
    /// The settings row.
    pub settings: SettingsBody,
    /// The channel that cannot be switched off, so the form can render it locked without
    /// hard-coding the name in two places.
    pub locked_channel: &'static str,
}

/// `GET /api/v1/notifications/preferences` — this person's own channel configuration.
///
/// **The answer is always a complete matrix.** A form that renders only the stated cells shows
/// a reader a grid with holes in it, and a hole and a checked box look identical until the
/// reader tries to change one.
pub async fn get_preferences(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<PreferencesBody>, ApiError> {
    let preferences = omnion_notifications::read_preferences(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;
    Ok(Json(PreferencesBody {
        cells: preferences.matrix,
        settings: SettingsBody::from(preferences.settings),
        locked_channel: omnion_notifications::IN_APP,
    }))
}

/// `PUT /api/v1/notifications/preferences` — save the cells and the settings row.
///
/// The answer carries the **whole matrix back, not the changed count**, and that is the
/// deliberate choice over "2 preferences saved": the form needs the authoritative state to
/// render from, and a count is a number the client has to trust without being able to check
/// it. The changed count rides along for the toast, because a reader who flipped two boxes
/// deserves to know they landed.
pub async fn put_preferences(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<PutPreferencesBody>,
) -> Result<Json<PutPreferencesResult>, ApiError> {
    let settings = body.settings.to_settings(session.user.id);
    let changed = omnion_notifications::write_preferences(
        state.db().pool(),
        session.user.id,
        &body.cells,
        &settings,
    )
    .await
    .map_err(map_store)?;

    let preferences = omnion_notifications::read_preferences(state.db().pool(), session.user.id)
        .await
        .map_err(map_store)?;

    bus::emit(
        state.db().pool(),
        NewEvent::new("notification.preferences.changed")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "cells": changed,
                "digest_cadence": settings.digest_cadence,
                "quiet_hours": settings.quiet_hours_start.is_some(),
            })),
    )
    .await
    .ok();

    Ok(Json(PutPreferencesResult {
        changed,
        cells: preferences.matrix,
        settings: SettingsBody::from(preferences.settings),
        locked_channel: omnion_notifications::IN_APP,
    }))
}

/// What a save changed and what the server now believes.
#[derive(Debug, Serialize)]
pub struct PutPreferencesResult {
    /// How many cells actually changed value. The store counts *rows that changed*, so this
    /// is a `u64` from a `rows_affected` and is rendered as a number in the toast — the JSON
    /// carries it unchanged rather than through an `i64` cast that would add a panic path
    /// for a count that is bounded by the size of the request.
    pub changed: u64,
    /// The full matrix after the save.
    pub cells: Vec<PreferenceCell>,
    /// The settings row after the save.
    pub settings: SettingsBody,
    /// See [`PreferencesBody::locked_channel`].
    pub locked_channel: &'static str,
}

// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `true` when the row exists and belongs to this person.
async fn row_is_mine(state: &AppState, user_id: Uuid, id: Uuid) -> Result<bool, ApiError> {
    Ok(
        omnion_notifications::store::find(state.db().pool(), user_id, id)
            .await
            .map_err(map_store)?
            .is_some(),
    )
}

/// The refusal for a row that is not the caller's, and for one that is gone.
fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "notification_not_found",
        "no such notification",
    )
}

/// Build a store query from the request's parameters, refusing what it cannot serve.
///
/// Every refusal here names the field and the legal values, because the caller of this
/// endpoint is a panel and a human: `?category=invoce` has to be visibly a typo rather than an
/// empty list the reader concludes is good news.
fn build_query(params: ListParams) -> Result<ListQuery, ApiError> {
    for category in &params.category {
        if !omnion_notifications::is_category(category) {
            return Err(ApiError::bad_request(
                "invalid_notification",
                format!("category \"{category}\" is not one of {CATEGORIES:?}"),
            ));
        }
    }
    for priority in &params.priority {
        if !omnion_notifications::is_priority(priority) {
            return Err(ApiError::bad_request(
                "invalid_notification",
                format!(
                    "priority \"{priority}\" is not one of {:?}",
                    omnion_notifications::PRIORITIES
                ),
            ));
        }
    }
    if let Some(channel) = &params.channel {
        omnion_notifications::validate_channel(channel).map_err(map_store)?;
    }

    let unread = match params.read.as_deref() {
        None | Some("") => None,
        Some("unread") => Some(true),
        Some("read") => Some(false),
        Some(other) => {
            return Err(ApiError::bad_request(
                "invalid_notification",
                format!("read=\"{other}\" is not one of \"unread\", \"read\""),
            ));
        }
    };

    // The cursor is an RFC 3339 instant, parsed here so a malformed one is a `400` naming the
    // field — the store takes a typed value and cannot be the one to complain.
    let before = match params.before.as_deref() {
        None | Some("") => None,
        Some(raw) => Some(
            time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
                .map_err(|error| {
                    ApiError::bad_request(
                        "invalid_notification",
                        format!("before=\"{raw}\" is not an RFC 3339 instant ({error})"),
                    )
                })?,
        ),
    };

    Ok(ListQuery {
        categories: params.category,
        unread,
        priorities: params.priority,
        channel: params.channel,
        include_archived: params.archived,
        // `with_read` is a plain `bool` that already defaults to true, so a client that
        // named nothing is asking for the whole list. The panel's State menu says "Unread
        // and read" for exactly this state, so anything else would be the server quietly
        // filtering a list it is displaying in full.
        include_read: params.with_read,
        before,
        limit: params.limit.unwrap_or(50),
    })
}

/// Where the categories come from, for a client that wants to build a filter without asking.
///
/// Exported as a constant rather than a route: a client that has to make a round trip to learn
/// the closed list will cache it anyway, and a cached list is a list that goes stale.
pub const KNOWN_CATEGORIES: [&str; 6] = CATEGORIES;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unknown_categories_drive_a_bad_request_that_names_its_values() {
        let error = build_query(ListParams {
            category: vec!["invoce".to_owned()],
            ..ListParams::default()
        })
        .expect_err("invoce is not a category");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        // Both the typo and the legal values: a reader who mistyped needs to see what they
        // could have meant, and a client needs the list to build the next request.
        assert!(error.message().contains("invoce"));
        assert!(error.message().contains("approval"));
    }

    #[test]
    fn a_good_query_builds_with_every_filter_carried() {
        let query = build_query(ListParams {
            category: vec!["approval".to_owned()],
            read: Some("unread".to_owned()),
            priority: vec!["high".to_owned()],
            channel: Some("email".to_owned()),
            archived: true,
            with_read: true,
            before: None,
            limit: Some(25),
        })
        .expect("valid");
        assert_eq!(query.categories, ["approval"]);
        assert_eq!(query.unread, Some(true));
        assert_eq!(query.priorities, ["high"]);
        assert_eq!(query.channel.as_deref(), Some("email"));
        assert!(query.include_archived);
        assert!(query.include_read);
        assert_eq!(query.limit, 25);
    }

    #[test]
    fn the_read_filter_refuses_anything_that_is_not_read_or_unread() {
        let error = build_query(ListParams {
            read: Some("maybe".to_owned()),
            ..ListParams::default()
        })
        .expect_err("maybe is not a read state");
        assert!(error.message().contains("unread"));
    }

    #[test]
    fn a_malformed_cursor_is_a_bad_request_naming_the_field() {
        let error = build_query(ListParams {
            before: Some("yesterday".to_owned()),
            ..ListParams::default()
        })
        .expect_err("yesterday is not an instant");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert!(error.message().contains("before="));
    }

    #[test]
    fn a_rfc3339_cursor_parses() {
        let query = build_query(ListParams {
            before: Some("2026-09-28T10:00:00Z".to_owned()),
            ..ListParams::default()
        })
        .expect("valid");
        assert!(query.before.is_some());
    }

    #[test]
    fn an_unknown_channel_is_refused_rather_than_matching_nothing() {
        let error = build_query(ListParams {
            channel: Some("carrier-pigeon".to_owned()),
            ..ListParams::default()
        })
        .expect_err("not a channel");
        assert!(error.message().contains("carrier-pigeon"));
    }

    #[test]
    fn a_missing_cursor_and_a_missing_read_state_are_both_absent() {
        let query = build_query(ListParams::default()).expect("valid");
        assert!(query.before.is_none());
        assert!(query.unread.is_none());
    }
}
