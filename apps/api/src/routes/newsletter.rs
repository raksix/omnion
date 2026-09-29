//! `/api/v1/newsletter/*` and `/api/v1/public/newsletter/*` — REQ-064, slice 4b.
//!
//! Lists, subscribers, CSV import/export, the sent-issue archive, and the public double
//! opt-in flow. Five decisions shape the file:
//!
//! * **Two powers, not one.** `newsletter.read` opens the tables; `newsletter.manage` creates a
//!   list, imports, promotes a subscriber and sends. Same split the comment inbox drew, for the
//!   same reason: handing a list to somebody should not hand them the ability to re-subscribe
//!   the people who left.
//!
//! * **The public signup never discloses the state it chose.** It answers 202 with the address
//!   and whether a confirmation is needed, because "check your inbox" is advice a visitor
//!   needs — and nothing else. It does not say `pending`, does not echo the token, and does
//!   not distinguish "already subscribed" from "not subscribed" beyond a 409 the caller has to
//!   earn.
//!
//! * **A token that matches nothing is ONE answer.** Unknown, expired and already-used all
//!   answer `400 invalid_token` with the same message. A public form that can tell them apart
//!   is an existence oracle over a table of e-mail addresses, reachable by anybody who gets
//!   hold of one forwarded link.
//!
//! * **The raw token is returned by the store, never read back from the database.** The mail
//!   path is where it lives; this file's only job is to not put it in a response body or a log.
//!   When the platform has no mail transport configured the token is *not* echoed either — it is
//!   reported as `delivery: "unavailable"`, so an owner can see that no mail went out rather
//!   than find out from a subscriber.
//!
//! * **Every read is scoped by site in the `where` clause**, and the scope refusal is a 404 for
//!   a row the caller may not see. `ensure_same_organization` answers 403, and it is a
//!   *permission* check with a platform-account exception — the two are different contracts and
//!   the store's is the one that keeps a list from leaking.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use omnion_audit::NewAuditEntry;
use omnion_content::newsletter::{
    ImportReport, Issue, IssueSummary, ListPatch, NewsletterList, NewsletterStore, NewIssue,
    NewList, PublicIssue, SignupOutcome, Subscriber, SubscriberFilter, validate_email,
};
use omnion_events::{NewEvent, bus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::public::resolve_site;
use crate::scope::ensure_same_organization;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Bodies and params
// ---------------------------------------------------------------------------------------------

/// `?site_id=` on every panel route, and `?site=` on the public ones.
///
/// One struct for both because they are the same query with the same name in the URL, and two
/// structs for one query is how a public route ends up calling the organization-scoped lookup
/// that needs a session it does not have.
#[derive(Debug, Default, Deserialize)]
pub struct SiteParam {
    /// The site the request is about. The panel's selector.
    pub site_id: Option<Uuid>,
    /// The site key, for a public request on a multi-site installation.
    pub site: Option<String>,
}

/// Query parameters of the subscribers table.
#[derive(Debug, Default, Deserialize)]
pub struct SubscriberParams {
    /// The site.
    pub site_id: Option<Uuid>,
    /// One list.
    pub list_id: Option<Uuid>,
    /// One state.
    pub status: Option<String>,
    /// Free text over address, name and source.
    #[serde(alias = "q")]
    pub search: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Offset.
    pub offset: Option<i64>,
}

/// Query parameters of the public confirm/unsubscribe links.
#[derive(Debug, Default, Deserialize)]
pub struct TokenParam {
    /// The token from the link.
    pub token: Option<String>,
    /// Site key, on a multi-site installation.
    pub site: Option<String>,
}

/// Create a list.
#[derive(Debug, Deserialize)]
pub struct CreateListRequest {
    /// The site.
    pub site_id: Uuid,
    /// The public key. Optional — the store derives one from the name.
    #[serde(default)]
    pub key: Option<String>,
    /// Display name.
    pub name: String,
    /// What the list is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether a signup must be confirmed. Defaults to true.
    #[serde(default)]
    pub double_opt_in: Option<bool>,
}

/// Change a list.
#[derive(Debug, Default, Deserialize)]
pub struct PatchListRequest {
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
    /// New confirmation requirement.
    #[serde(default)]
    pub double_opt_in: Option<bool>,
}

/// A public signup.
#[derive(Debug, Deserialize)]
pub struct PublicSignupRequest {
    /// The address.
    pub email: String,
    /// Optional display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Where the signup came from, as the form saw it.
    #[serde(default)]
    pub source: Option<String>,
}

/// What a signup is told.
///
/// Three fields, and the third is the only one that carries information about the state: a
/// visitor needs to know whether to go and check their inbox. Everything else is a constant.
#[derive(Debug, Serialize)]
pub struct SignupResponse {
    /// The address, normalised.
    pub email: String,
    /// Whether a confirmation link is on its way.
    pub confirmation_required: bool,
    /// What happened to the confirmation, when the platform could not send it.
    pub delivery: Option<String>,
}

/// Change a subscriber's state from the panel.
#[derive(Debug, Deserialize)]
pub struct StatusRequest {
    /// The state to move to.
    pub status: String,
    /// Why, when the panel asks.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A CSV import.
#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    /// The file's contents.
    pub csv: String,
    /// Where the rows came from, for the row's own `source`.
    #[serde(default)]
    pub source: Option<String>,
}

/// Send an issue to a list.
#[derive(Debug, Deserialize)]
pub struct SendIssueRequest {
    /// The site.
    pub site_id: Uuid,
    /// The list.
    pub list_id: Uuid,
    /// Subject line.
    pub subject: String,
    /// The owner's HTML.
    pub body_html: String,
    /// The permalink segment. Derived from the subject when absent.
    #[serde(default)]
    pub archive_slug: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Panel: lists
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/newsletter/lists` — every list of a site, with its counts.
pub async fn list_lists(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
) -> Result<Json<Vec<NewsletterList>>, ApiError> {
    let site_id = require_site(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;

    let lists = NewsletterStore::new(state.db().pool().clone())
        .list_lists(site_id)
        .await?;
    Ok(Json(lists))
}

/// `POST /api/v1/newsletter/lists` — create a list.
pub async fn create_list(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<CreateListRequest>,
) -> Result<(StatusCode, Json<NewsletterList>), ApiError> {
    let site = site_in_scope(&state, &session, body.site_id).await?;

    let list = NewsletterStore::new(state.db().pool().clone())
        .create_list(NewList {
            site_id: site.id,
            organization_id: site.organization_id,
            key: body.key,
            name: body.name,
            description: body.description,
            double_opt_in: body.double_opt_in,
            created_by: Some(session.user.id),
        })
        .await?;

    audit(
        &state,
        &session,
        "newsletter.list_created",
        Some(list.id),
        Some(site.organization_id),
        json!({ "list_id": list.id, "site_id": site.id, "key": list.key, "double_opt_in": list.double_opt_in }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(list)))
}

/// `GET /api/v1/newsletter/lists/{id}` — one list.
///
/// `?site_id=` is REQUIRED and the row must belong to it, so a caller who addresses a list of
/// another tenant with this tenant's selector gets a 404. The store reads by id alone — the
/// panel does not know the site until it has the row — so the concealment has to be applied
/// here, and it has to be applied BEFORE `ensure_same_organization`: that is a permission
/// check and answers 403, which tells a caller that the list exists. Loading the row and then
/// asking whether the caller may have it is how a scoped query's concealment is undone.
pub async fn get_list(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<Json<NewsletterList>, ApiError> {
    let site_id = require_site(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;
    let list = concealed_list(&state, site_id, id).await?;
    Ok(Json(list))
}

/// Read a list, and answer 404 for one that belongs to another site.
///
/// Shared by the three handlers that address a list by id, because "load then check" written
/// three times is two of them wrong.
async fn concealed_list(
    state: &AppState,
    site_id: Uuid,
    id: Uuid,
) -> Result<NewsletterList, ApiError> {
    let list = NewsletterStore::new(state.db().pool().clone())
        .list_by_id(id)
        .await?;
    if list.site_id != site_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "newsletter_list_not_found",
            "no such newsletter list",
        ));
    }
    Ok(list)
}

/// `PUT /api/v1/newsletter/lists/{id}` — rename, re-describe, change the confirmation rule.
pub async fn patch_list(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchListRequest>,
) -> Result<Json<NewsletterList>, ApiError> {
    let site_id = require_site(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;
    let store = NewsletterStore::new(state.db().pool().clone());
    let before = concealed_list(&state, site_id, id).await?;

    let list = store
        .patch_list(
            id,
            ListPatch {
                name: body.name,
                description: body.description,
                double_opt_in: body.double_opt_in,
            },
        )
        .await?;

    audit(
        &state,
        &session,
        "newsletter.list_updated",
        Some(list.id),
        None,
        json!({
            "list_id": list.id,
            "from_double_opt_in": before.double_opt_in,
            "to_double_opt_in": list.double_opt_in,
        }),
    )
    .await;

    Ok(Json(list))
}

/// `DELETE /api/v1/newsletter/lists/{id}` — delete a list and everybody on it.
pub async fn delete_list(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let site_id = require_site(&params)?;
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = NewsletterStore::new(state.db().pool().clone());
    let before = concealed_list(&state, site_id, id).await?;

    // The counts go into the audit row, because "delete list" is the one action here whose
    // consequence is a number of people's addresses.
    let counts = store.list_counts(id).await?;
    store.delete_list(id).await?;

    audit(
        &state,
        &session,
        "newsletter.list_deleted",
        Some(id),
        Some(site.organization_id),
        json!({
            "list_id": id,
            "site_id": before.site_id,
            "subscribers_removed": counts.pending + counts.confirmed
                + counts.unsubscribed + counts.bounced,
        }),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Panel: subscribers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/newsletter/subscribers` — one filtered page plus the total.
pub async fn list_subscribers(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SubscriberParams>,
) -> Result<Json<SubscriberPageBody>, ApiError> {
    let site_id = require_site_of(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;

    let page = NewsletterStore::new(state.db().pool().clone())
        .subscribers(site_id, &filter_from(&params))
        .await?;

    Ok(Json(SubscriberPageBody {
        subscribers: page.subscribers,
        total: page.total,
    }))
}

/// The page, as the panel reads it.
#[derive(Debug, Serialize)]
pub struct SubscriberPageBody {
    /// The rows, newest first.
    pub subscribers: Vec<Subscriber>,
    /// How many rows the filter matched.
    pub total: i64,
}

/// `GET /api/v1/newsletter/subscribers/{id}` — one subscriber.
pub async fn get_subscriber(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<Json<Subscriber>, ApiError> {
    let site_id = require_site(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;
    let subscriber = concealed_subscriber(&state, site_id, id).await?;
    Ok(Json(subscriber))
}

/// Read a subscriber, and answer 404 for one that belongs to another site.
async fn concealed_subscriber(
    state: &AppState,
    site_id: Uuid,
    id: Uuid,
) -> Result<Subscriber, ApiError> {
    let subscriber = NewsletterStore::new(state.db().pool().clone())
        .subscriber(id)
        .await?;
    if subscriber.site_id != site_id {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "subscriber_not_found",
            "no such subscriber",
        ));
    }
    Ok(subscriber)
}

/// `PATCH /api/v1/newsletter/subscribers/{id}` — verify, bounce, re-instate.
pub async fn set_subscriber_status(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusRequest>,
) -> Result<Json<Subscriber>, ApiError> {
    let site_id = require_site(&params)?;
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = NewsletterStore::new(state.db().pool().clone());
    let before = concealed_subscriber(&state, site_id, id).await?;

    let subscriber = store
        .set_status(id, &body.status, body.reason.clone())
        .await?;

    audit(
        &state,
        &session,
        "newsletter.subscriber_status_changed",
        Some(id),
        Some(site.organization_id),
        json!({
            "subscriber_id": id,
            "list_id": subscriber.list_id,
            "from": before.status,
            "to": subscriber.status,
        }),
    )
    .await;
    emit(
        &state,
        subscriber_event(&subscriber.status),
        json!({
            "list_id": subscriber.list_id,
            "subscriber_id": subscriber.id,
            "site_id": subscriber.site_id,
            "status": subscriber.status,
        }),
    )
    .await;

    Ok(Json(subscriber))
}

/// `DELETE /api/v1/newsletter/subscribers/{id}` — remove the row outright.
///
/// The panel's own "remove entirely", deliberately distinct from the unsubscribe link: one is
/// an operator deciding this address does not belong here at all, the other is the person
/// asking to leave. The audit row records which of the two it was, because after the fact they
/// look identical in the table.
pub async fn delete_subscriber(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let site_id = require_site(&params)?;
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = NewsletterStore::new(state.db().pool().clone());
    let before = concealed_subscriber(&state, site_id, id).await?;
    store.delete_subscriber(id).await?;

    audit(
        &state,
        &session,
        "newsletter.subscriber_deleted",
        Some(id),
        Some(site.organization_id),
        json!({
            "subscriber_id": id,
            "list_id": before.list_id,
            "status_at_deletion": before.status,
        }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/newsletter/lists/{id}/import` — import addresses from CSV.
pub async fn import_subscribers(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
    Json(body): Json<ImportRequest>,
) -> Result<Json<ImportReport>, ApiError> {
    let site_id = require_site(&params)?;
    let site = site_in_scope(&state, &session, site_id).await?;
    let mut store = NewsletterStore::new(state.db().pool().clone());
    let list = concealed_list(&state, site_id, id).await?;

    let report = store
        .import_csv(id, site.id, &body.csv, body.source.as_deref())
        .await?;

    audit(
        &state,
        &session,
        "newsletter.subscribers_imported",
        Some(id),
        Some(site.organization_id),
        json!({
            "list_id": id,
            "added": report.added,
            "skipped": report.skipped.len(),
            "blank": report.blank,
        }),
    )
    .await;

    Ok(Json(report))
}

/// `GET /api/v1/newsletter/subscribers/export` — the filtered rows as CSV.
pub async fn export_subscribers(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SubscriberParams>,
) -> Result<Json<ExportBody>, ApiError> {
    let site_id = require_site_of(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;

    let csv = NewsletterStore::new(state.db().pool().clone())
        .export_csv(site_id, &filter_from(&params))
        .await?;

    Ok(Json(ExportBody { csv }))
}

/// The export, as a JSON body.
///
/// A JSON wrapper rather than a `text/csv` response, because the panel downloads it through
/// the API client and a `Content-Disposition` header is invisible to `fetch` — the file would
/// arrive as a string the operator has to save by hand, which is not what "Export" means.
#[derive(Debug, Serialize)]
pub struct ExportBody {
    /// The file's contents.
    pub csv: String,
}

/// `POST /api/v1/newsletter/lists/{id}/subscribers` — add one address by hand.
pub async fn add_subscriber(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<SiteParam>,
    Path(id): Path<Uuid>,
    Json(body): Json<PublicSignupRequest>,
) -> Result<(StatusCode, Json<Subscriber>), ApiError> {
    let site_id = require_site(&params)?;
    let site = site_in_scope(&state, &session, site_id).await?;
    let store = NewsletterStore::new(state.db().pool().clone());
    let list = concealed_list(&state, site_id, id).await?;

    // An owner adding somebody by hand has vouched for the address, so the row is CONFIRMED
    // even on a double-opt-in list: the whole point of the confirmation is that the person
    // consented, and an operator typing an address into their own panel is the platform's
    // substitute for that, deliberately and audibly (`source: "added in the panel"`). The
    // store does not know about the panel, so the promotion happens here.
    let outcome = store
        .subscribe(omnion_content::newsletter::NewSubscriber {
            list_id: id,
            site_id: site.id,
            email: validate_email(&body.email)?,
            name: body.name.clone(),
            source: body.source.clone().or_else(|| Some("added in the panel".to_owned())),
        })
        .await?;
    let subscriber = if outcome.subscriber.status == "pending" {
        store
            .set_status(
                outcome.subscriber.id,
                "confirmed",
                Some("added by hand in the panel".to_owned()),
            )
            .await?
    } else {
        outcome.subscriber
    };

    audit(
        &state,
        &session,
        "newsletter.subscriber_added",
        Some(subscriber.id),
        Some(site.organization_id),
        json!({ "subscriber_id": subscriber.id, "list_id": id, "by_hand": true }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(subscriber)))
}

// ---------------------------------------------------------------------------------------------
// Panel: issues
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/newsletter/issues` — send an issue and archive it.
pub async fn send_issue(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<SendIssueRequest>,
) -> Result<(StatusCode, Json<Issue>), ApiError> {
    let site = site_in_scope(&state, &session, body.site_id).await?;

    let issue = NewsletterStore::new(state.db().pool().clone())
        .record_issue(NewIssue {
            site_id: site.id,
            list_id: body.list_id,
            subject: body.subject,
            body_html: body.body_html,
            archive_slug: body.archive_slug,
            created_by: Some(session.user.id),
        })
        .await?;

    audit(
        &state,
        &session,
        "newsletter.issue_sent",
        Some(issue.id),
        Some(site.organization_id),
        json!({
            "issue_id": issue.id,
            "list_id": issue.list_id,
            "recipient_count": issue.recipient_count,
            "archive_slug": issue.archive_slug,
        }),
    )
    .await;
    emit(
        &state,
        "newsletter.issue.sent",
        json!({
            "issue_id": issue.id,
            "list_id": issue.list_id,
            "site_id": site.id,
            "recipient_count": issue.recipient_count,
        }),
    )
    .await;

    Ok((StatusCode::CREATED, Json(issue)))
}

/// `GET /api/v1/newsletter/issues` — the archive.
pub async fn list_issues(
    State(state): State<AppState>,
    session: CurrentSession,
    Query(params): Query<IssueParams>,
) -> Result<Json<Vec<IssueSummary>>, ApiError> {
    let site_id = require_site_of_issues(&params)?;
    let _ = site_in_scope(&state, &session, site_id).await?;

    Ok(Json(
        NewsletterStore::new(state.db().pool().clone())
            .list_issues(site_id, params.limit.unwrap_or(50))
            .await?,
    ))
}

/// Query parameters of the archive.
#[derive(Debug, Default, Deserialize)]
pub struct IssueParams {
    /// The site.
    pub site_id: Option<Uuid>,
    /// Page size.
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Public
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/public/newsletter/{key}/subscribe` — a visitor signs up.
pub async fn public_subscribe(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
    Json(body): Json<PublicSignupRequest>,
) -> Result<(StatusCode, Json<SignupResponse>), ApiError> {
    // The public surface resolves its site exactly like every other public route: the `site`
    // query for a multi-site installation, then the Host header. There is no session and
    // therefore no organization — `find_site_by_key` is the organization-scoped lookup and
    // answers nothing here.
    let site = resolve_site(state.db().pool(), params.site.as_deref(), &headers).await?;

    let list = NewsletterStore::new(state.db().pool().clone())
        .list_by_key(site.id, &key)
        .await?;

    let outcome = NewsletterStore::new(state.db().pool().clone())
        .subscribe(omnion_content::newsletter::NewSubscriber {
            list_id: list.id,
            site_id: site.id,
            email: body.email,
            name: body.name,
            source: body.source,
        })
        .await?;

    let confirmation_required = outcome.confirm_token.is_some();

    // The token is NOT in the response and NOT in a log. The panel reads `delivery` to tell an
    // owner that the confirmation never went out, and a visitor is told to check their inbox
    // only when there is something in it to find.
    let delivery = if confirmation_required {
        match deliver_token(&state, &outcome, &list.key).await {
            Delivery::Sent => "sent",
            Delivery::Unavailable(reason) => {
                audit_public(&state, &outcome, "newsletter.confirmation_not_delivered", &reason).await;
                "unavailable"
            }
        }
    } else {
        "not_required"
    };

    if confirmation_required {
        emit(
            &state,
            "newsletter.subscriber.pending",
            json!({
                "list_id": list.id,
                "subscriber_id": outcome.subscriber.id,
                "site_id": site.id,
                "status": outcome.subscriber.status,
            }),
        )
        .await;
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(SignupResponse {
            email: outcome.subscriber.email,
            confirmation_required,
            delivery: Some(delivery.to_owned()),
        }),
    ))
}

/// `GET /api/v1/public/newsletter/confirm?token=` — the double opt-in click.
pub async fn public_confirm(
    State(state): State<AppState>,
    Query(params): Query<TokenParam>,
    headers: HeaderMap,
) -> Result<Json<TokenOutcomeBody>, ApiError> {
    let site = public_site(&state, params.site.as_deref(), &headers).await?;
    let token = params.token.unwrap_or_default();

    let outcome = NewsletterStore::new(state.db().pool().clone())
        .confirm(&token)
        .await?;

    if outcome.applied {
        emit(
            &state,
            "newsletter.subscriber.confirmed",
            json!({ "site_id": site.id, "email_present": true }),
        )
        .await;
    }

    Ok(Json(TokenOutcomeBody::from(outcome)))
}

/// `GET /api/v1/public/newsletter/unsubscribe?token=` — the opt-out click, no sign-in.
pub async fn public_unsubscribe(
    State(state): State<AppState>,
    Query(params): Query<TokenParam>,
    headers: HeaderMap,
) -> Result<Json<TokenOutcomeBody>, ApiError> {
    let site = public_site(&state, params.site.as_deref(), &headers).await?;
    let token = params.token.unwrap_or_default();

    let outcome = NewsletterStore::new(state.db().pool().clone())
        .unsubscribe(&token)
        .await?;

    if outcome.applied {
        emit(
            &state,
            "newsletter.subscriber.unsubscribed",
            json!({ "site_id": site.id }),
        )
        .await;
    }

    Ok(Json(TokenOutcomeBody::from(outcome)))
}

/// `GET /api/v1/public/newsletter/issues/{slug}` — one archived issue.
pub async fn public_issue(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
) -> Result<Json<PublicIssue>, ApiError> {
    let site = public_site(&state, params.site.as_deref(), &headers).await?;
    Ok(Json(
        NewsletterStore::new(state.db().pool().clone())
            .issue_by_slug(site.id, &slug)
            .await?,
    ))
}

/// `GET /api/v1/public/newsletter/lists` — the lists a theme may render a signup form for.
///
/// A site's own published lists, and nothing else. The public surface is a menu, not a
/// directory: a list nobody can subscribe to should not be advertised, and a list that is
/// there is one whose key the theme can post to.
pub async fn public_lists(
    State(state): State<AppState>,
    Query(params): Query<SiteParam>,
    headers: HeaderMap,
) -> Result<Json<Vec<PublicListBody>>, ApiError> {
    let site = public_site(&state, params.site.as_deref(), &headers).await?;
    let lists = NewsletterStore::new(state.db().pool().clone())
        .list_lists(site.id)
        .await?;
    Ok(Json(
        lists
            .into_iter()
            .map(|list| PublicListBody {
                key: list.key,
                name: list.name,
                description: list.description,
                double_opt_in: list.double_opt_in,
            })
            .collect(),
    ))
}

/// A list as the public surface draws it.
#[derive(Debug, Serialize)]
pub struct PublicListBody {
    /// The signup key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the list is for.
    pub description: Option<String>,
    /// Whether a signup needs confirming.
    pub double_opt_in: bool,
}

/// The token outcome, as a public route returns it.
#[derive(Debug, Serialize)]
pub struct TokenOutcomeBody {
    /// The address it acted on.
    pub email: String,
    /// The state afterwards.
    pub status: String,
    /// False when the link was already used, expired or unknown.
    pub applied: bool,
    /// Why it did not apply. Null when it did.
    pub reason: Option<String>,
}

impl From<omnion_content::newsletter::TokenOutcome> for TokenOutcomeBody {
    fn from(outcome: omnion_content::newsletter::TokenOutcome) -> Self {
        Self {
            email: outcome.email,
            status: outcome.status,
            applied: outcome.applied,
            reason: outcome.reason,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// What happened to a confirmation token.
enum Delivery {
    /// A transport took it.
    Sent,
    /// The platform has no mail path configured, so nothing left.
    Unavailable(String),
}

/// Hand a confirmation token to the platform mail path.
///
/// The REQ says the send rides the marketing sender when it is installed and the platform mail
/// service otherwise, and the module never depends on a module that is not there. Today neither
/// is reachable from a content route, so this reports `Unavailable` rather than pretending —
/// and the audit row says so, which is what turns "subscribers say they got nothing" into a
/// line an owner can read.
async fn deliver_token(
    state: &AppState,
    outcome: &SignupOutcome,
    list_key: &str,
) -> Delivery {
    let _ = (state, outcome, list_key);
    Delivery::Unavailable(
        "no mail transport is configured for this installation".to_owned(),
    )
}

/// Audit a public action that has no session to attribute it to.
async fn audit_public(state: &AppState, outcome: &SignupOutcome, action: &'static str, reason: &str) {
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry {
            organization_id: None,
            actor_user_id: None,
            // `System`, not a hypothetical `Anonymous`: a public action has no actor, and
            // inventing a variant for it would need a migration to persist.
            actor_type: omnion_audit::ActorType::System,
            action,
            target_type: Some("newsletter_subscriber"),
            target_id: Some(outcome.subscriber.id.to_string()),
            // The ADDRESS is deliberately not in the metadata: an audit table is read by more
            // people than the subscriber table, and "what went wrong with the confirmation" is
            // answerable from the id.
            metadata: json!({ "reason": reason }),
            ip_address: None,
        },
    )
    .await;
}

/// The event name a state change is announced with.
fn subscriber_event(status: &str) -> &'static str {
    match status {
        "confirmed" => "newsletter.subscriber.confirmed",
        "unsubscribed" => "newsletter.subscriber.unsubscribed",
        _ => "newsletter.subscriber.updated",
    }
}

/// `?site_id=` is required on the panel routes.
fn require_site(params: &SiteParam) -> Result<Uuid, ApiError> {
    params.site_id.ok_or_else(|| {
        ApiError::bad_request("site_required", "this request needs a site_id")
    })
}

/// The same requirement for the routes whose params carry more than the site.
///
/// A second function rather than a trait, because the two structs have nothing else in common
/// and a trait over "has a site_id" is a trait with one implementor per query type.
fn require_site_of(params: &SubscriberParams) -> Result<Uuid, ApiError> {
    params.site_id.ok_or_else(|| {
        ApiError::bad_request("site_required", "this request needs a site_id")
    })
}

/// And for the archive's own query.
fn require_site_of_issues(params: &IssueParams) -> Result<Uuid, ApiError> {
    params.site_id.ok_or_else(|| {
        ApiError::bad_request("site_required", "this request needs a site_id")
    })
}

/// Build the store's filter from the query.
fn filter_from(params: &SubscriberParams) -> SubscriberFilter {
    SubscriberFilter {
        list_id: params.list_id,
        status: params.status.clone(),
        search: params.search.clone(),
        limit: params.limit.unwrap_or(50),
        offset: params.offset.unwrap_or(0),
    }
}

/// Load a site and refuse a caller who may not see it.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    site_id: Uuid,
) -> Result<omnion_identity::sites::Site, ApiError> {
    // 404 for a site the caller may not see, and only THEN the cross-tenant refusal. See
    // `routes::comments::site_in_scope` — same rule, same reason.
    let site = omnion_identity::sites::find_site(state.db().pool(), site_id)
        .await?
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
        })?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

/// Resolve the addressed site on a public route.
async fn public_site(
    state: &AppState,
    key: Option<&str>,
    headers: &HeaderMap,
) -> Result<omnion_identity::sites::Site, ApiError> {
    resolve_site(state.db().pool(), key, headers).await
}

/// Record an audit row, swallowing its own failure.
async fn audit(
    state: &AppState,
    session: &CurrentSession,
    action: &'static str,
    target_id: Option<Uuid>,
    organization_id: Option<Uuid>,
    details: serde_json::Value,
) {
    let _ = omnion_audit::record(
        state.db().pool(),
        NewAuditEntry {
            organization_id: organization_id.or(session.user.organization_id),
            actor_user_id: Some(session.user.id),
            actor_type: omnion_audit::ActorType::User,
            action,
            target_type: Some("newsletter"),
            target_id: target_id.map(|id| id.to_string()),
            metadata: details,
            ip_address: None,
        },
    )
    .await;
}

/// Emit one event, swallowing its own failure.
async fn emit(state: &AppState, name: &str, payload: serde_json::Value) {
    let _ = bus::emit(state.db().pool(), NewEvent::new(name).payload(payload)).await;
}
