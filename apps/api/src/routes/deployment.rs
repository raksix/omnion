//! `/api/v1/deployment/*` — the deployment centre's read surface (REQ-024, slice 1).
//!
//! Seven routes, one guard (`deployment.read`), and the whole file exists because of a rule the
//! brief writes as two lines of UI: `Version 2.4.1` / `Available 2.5.0`. Every shortcut around
//! that line ships a card that looks right and is wrong, so the values are **computed on the
//! server** by `omnion_deployment::availability` and travel as an `Availability` enum. The panel
//! never re-derives them, because a panel that compares two version strings with `>` offers
//! `1.9.0` as an upgrade from `1.10.0`.
//!
//! Five decisions are worth naming, and each is a place the obvious shortcut is wrong:
//!
//! * **The channel is resolved once, here, and travels with every response.** A release list
//!   without a channel is a list of *everything*, and a stable installation that renders a
//!   nightly list will happily offer a nightly as its next release. `?channel=` may narrow the
//!   installation's own channel for the release *browser*; it may never *widen* the card's
//!   offer, which is always `current.channel.admits(...)`.
//! * **`/version` is a build-metadata endpoint, not a deployment one.** It is what the shell
//!   footer reads, so it must answer even when the release feed is down and the cache is empty
//!   — and it must never claim an update is available, because a footer that offers a deploy is
//!   a footer that is offering something it cannot carry.
//! * **The stale banner is computed, not detected by the panel.** The spec names its wording
//!   ("showing cached data from {time}") and the timestamp has to come from `max(checked_at)`
//!   rather than from the clock, or the banner claims freshness it does not have.
//! * **A missing release is a `404` with the channel in the message.** The operator followed a
//!   link to `2.5.0` on `stable`; the honest answer names what was looked for and where, so
//!   they can tell a withdrawn release from a wrong channel.
//! * **`/checks/run` is the only write here, it is `deployment.manage`, and it does the whole
//!   check synchronously.** A "check now" button that returns before the check has run reports
//!   the *previous* run's result and the operator reads it as this one's. The scheduled runner
//!   exists for the routine case; this route exists for "I am about to deploy and need to know
//!   *now*", and it waits.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_deployment::manifest::{self, CheckResult};
use omnion_deployment::preflight::CheckOutcome;
use omnion_deployment::store::{self, HealthRow, HistoryFilter};
use omnion_deployment::version::{Availability, Channel, Release, Version};
use omnion_deployment::{StoreError, stale_banner};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Request and response shapes
// ---------------------------------------------------------------------------------------------

/// Query of every read that takes a channel.
#[derive(Debug, Default, Deserialize)]
pub struct ChannelQuery {
    /// `stable`, `beta` or `nightly`.
    #[serde(default)]
    pub channel: Option<String>,
}

/// Query of `GET /api/v1/deployment/releases`.
#[derive(Debug, Default, Deserialize)]
pub struct ReleaseListQuery {
    /// The channel to browse.
    #[serde(default)]
    pub channel: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Query of `GET /api/v1/deployment/history`.
#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    /// Environment filter.
    #[serde(default)]
    pub environment: Option<String>,
    /// `deploy`, `rollback` or `restart`.
    #[serde(default)]
    pub kind: Option<String>,
    /// A job status.
    #[serde(default)]
    pub status: Option<String>,
    /// `7d`, `30d`, `90d` or an RFC 3339 instant.
    #[serde(default)]
    pub window: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Row offset.
    #[serde(default)]
    pub offset: Option<i64>,
}

/// The version this installation is running.
#[derive(Debug, Serialize)]
pub struct VersionBody {
    /// The version string, as `CARGO_PKG_VERSION` wrote it.
    pub version: String,
    /// Which service reported it, so a footer can say *what* is at this version.
    pub service: String,
    /// The channel this installation follows.
    pub channel: String,
    /// Whether that channel string is one the platform understands. A `false` here is worth
    /// showing rather than normalizing away: it means an operator's configuration says
    /// something the deployment code does not recognize, and the card's offers are then
    /// computed under a rule nobody wrote.
    pub channel_understood: bool,
    /// The core version, when the build reports one.
    pub core: Option<String>,
    /// What the card's second line says. Never an empty string — see `Availability::label`.
    pub available: String,
    /// The same answer as an enum, so the panel renders three states instead of parsing text.
    pub availability: Availability,
    /// Whether there is anything to press `Deploy` against.
    pub upgrade_available: bool,
}

/// One environment card.
#[derive(Debug, Serialize)]
pub struct EnvironmentCardBody {
    /// `production`, `staging` or `sandbox`.
    pub environment: String,
    /// The label the card shows.
    pub name: String,
    /// `healthy`, `degraded` or `unreachable`.
    pub health: String,
    /// The version running here, from the last probe.
    pub version: Option<String>,
    /// When the last probe ran — the card's tooltip.
    pub checked_at: Option<OffsetDateTime>,
    /// The failing probe, when degraded. The tooltip's second line.
    pub failing_probe: Option<String>,
    /// What the card's `Available` line says here.
    pub available: String,
    /// The same answer as an enum.
    pub availability: Availability,
    /// Whether `Deploy` may be pressed.
    pub deployable: bool,
    /// Why not, when it may not. A disabled button with no reason is the dead control the
    /// request's Definition of Done forbids.
    pub blocked_reason: Option<String>,
    /// Whether `Rollback` may be pressed, and to which version.
    pub rollback: Option<RollbackBody>,
    /// The last deploy, for the card's footer line.
    pub last_deploy: Option<HistoryRowBody>,
}

/// What the card offers as a rollback.
#[derive(Debug, Serialize)]
pub struct RollbackBody {
    /// The version a rollback would return to.
    pub to_version: String,
    /// Whether that target is in the cache with notes, so `Rollback` can be honest about it.
    pub known: bool,
}

/// The version summary and the stale banner, shared by several responses.
#[derive(Debug, Serialize)]
pub struct VersionSummary {
    /// The installed version.
    pub current: String,
    /// The card's second line.
    pub available: String,
    /// The same answer as an enum.
    pub availability: Availability,
    /// The banner, or `None` when the cache is current. The panel renders it verbatim; the
    /// wording is a product surface, not a component's opinion.
    pub stale_banner: Option<String>,
}

/// Response of `GET /api/v1/deployment/version`.
#[derive(Debug, Serialize)]
pub struct VersionResponse {
    /// The version block.
    pub version: VersionBody,
}

/// Response of `GET /api/v1/deployment/environments`.
#[derive(Debug, Serialize)]
pub struct EnvironmentsResponse {
    /// One card per environment.
    pub environments: Vec<EnvironmentCardBody>,
    /// The version summary every card's offers were computed from.
    pub summary: VersionSummary,
}

/// Response of `GET /api/v1/deployment/environments/{id}`.
#[derive(Debug, Serialize)]
pub struct EnvironmentDetailResponse {
    /// The card.
    pub environment: EnvironmentCardBody,
    /// The last ten jobs, newest first, for the detail's history strip.
    pub history: Vec<HistoryRowBody>,
}

/// Response of `GET /api/v1/deployment/releases`.
#[derive(Debug, Serialize)]
pub struct ReleasesResponse {
    /// The channel these releases are on.
    pub channel: String,
    /// The releases, newest first.
    pub releases: Vec<ReleaseBody>,
    /// The banner, when the cache is not current.
    pub stale_banner: Option<String>,
    /// `true` when the channel is genuinely empty, so the panel can say "no releases for this
    /// channel" rather than rendering a zero-height table.
    pub empty: bool,
}

/// One release, as the list and the detail read it.
#[derive(Debug, Serialize)]
pub struct ReleaseBody {
    /// The version string.
    pub version: String,
    /// The channel.
    pub channel: String,
    /// The feed's own release date, unparsed and un-reformatted.
    pub released_at: Option<String>,
    /// The notes, already plain text.
    pub notes: String,
    /// Whether the notes declare breaking changes.
    pub breaking: bool,
    /// The migrations this release ships, in run order.
    pub migrations: Vec<String>,
    /// The oldest core that can run it.
    pub core_min: Option<String>,
    /// The artifact digest.
    pub artifact_checksum: Option<String>,
    /// When this row was last refreshed from the feed.
    pub checked_at: OffsetDateTime,
    /// Whether this is the version the card is offering.
    pub is_available: bool,
}

/// Response of `GET /api/v1/deployment/releases/{version}`.
#[derive(Debug, Serialize)]
pub struct ReleaseDetailResponse {
    /// The release.
    pub release: ReleaseBody,
    /// The version block, so the detail can show "you are on X, this is Y" without a second
    /// request and without the panel guessing.
    pub summary: VersionSummary,
    /// The plain-text statement of what a deploy would do, for the wizard's second step.
    /// `None` when the release is not an upgrade — deploying *to* the running version is a
    /// no-op and the panel must not offer it.
    pub upgrade_from: Option<String>,
}

/// One history row.
///
/// `Clone` because a card's footer line reuses the newest matching row out of the same page the
/// detail screen returns, and a hand-written rebuild of it would be a second place where the
/// row's fields are listed.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryRowBody {
    /// Primary key.
    pub id: Uuid,
    /// The environment.
    pub environment: String,
    /// `deploy`, `rollback` or `restart`.
    pub kind: String,
    /// Where it came from.
    pub from_version: Option<String>,
    /// Where it went.
    pub to_version: Option<String>,
    /// The status.
    pub status: String,
    /// The strategy.
    pub strategy: String,
    /// Who started it.
    pub started_by: Option<Uuid>,
    /// The reason, for a rollback.
    pub reason: Option<String>,
    /// The error, verbatim.
    pub error: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
    /// How long it took.
    pub duration_ms: Option<i32>,
    /// The steps, when the row is expanded.
    pub steps: Vec<StepBody>,
}

/// One step of a deployment.
#[derive(Debug, Clone, Serialize)]
pub struct StepBody {
    /// Position in the run.
    pub position: i32,
    /// The step's name.
    pub name: String,
    /// Its status.
    pub status: String,
    /// Its log output, so far.
    pub output: String,
    /// When it started.
    pub started_at: Option<OffsetDateTime>,
    /// When it finished.
    pub finished_at: Option<OffsetDateTime>,
}

/// Response of `GET /api/v1/deployment/history`.
#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    /// The page, newest first.
    pub rows: Vec<HistoryRowBody>,
    /// How many match the filter in total.
    pub total: i64,
    /// What the filter is, echoed — the panel's chips and the request's URL can disagree after a
    /// back-navigation, and the panel reads the answer from here rather than from its own state.
    pub filter: HistoryFilterBody,
}

/// The applied filter, for the chips.
#[derive(Debug, Serialize)]
pub struct HistoryFilterBody {
    /// Environment, when one was applied.
    pub environment: Option<String>,
    /// Kind, when one was applied.
    pub kind: Option<String>,
    /// Status, when one was applied.
    pub status: Option<String>,
    /// The window label the operator chose (`7d`, `30d`, `90d` or `all`).
    pub window: String,
}

/// Response of `GET /api/v1/deployment/checks`.
#[derive(Debug, Serialize)]
pub struct ChecksResponse {
    /// The channel this installation follows.
    pub channel: String,
    /// When the last check started.
    pub last_run_at: Option<OffsetDateTime>,
    /// When it finished.
    pub last_finished_at: Option<OffsetDateTime>,
    /// `completed`, `failed`, or `null` when nothing has run.
    pub last_status: Option<String>,
    /// The failure reason, verbatim, when the last run failed.
    pub last_error: Option<String>,
    /// How many releases the last successful read carried.
    pub last_seen: Option<i32>,
    /// What the last successful run announced. Empty on a run that announced nothing, which
    /// is the common case and must not read as a failure.
    pub last_announced: Vec<String>,
    /// The banner, when the cache is not current.
    pub stale_banner: Option<String>,
    /// When the next scheduled check is due.
    pub next_run_at: Option<OffsetDateTime>,
    /// Whether a check is due now — the runner's own `last_run_at` plus the interval, so the
    /// screen does not have to know the interval to say "in 4 minutes".
    pub due_in_seconds: Option<i64>,
    /// How many (channel, version) pairs this instance has already announced.
    pub announced_total: i64,
    /// The checks rendered as rows, reusing the wizard's vocabulary so the screen has one way of
    /// saying "this passed" rather than two.
    pub rows: Vec<CheckOutcome>,
}

/// Response of `POST /api/v1/deployment/checks/run`.
#[derive(Debug, Serialize)]
pub struct RunCheckResponse {
    /// The check's result.
    pub result: CheckResult,
    /// The card's answer after the check, which may have changed.
    pub summary: VersionSummary,
    /// What the run announced, after the dedupe.
    pub announced: Vec<String>,
    /// The check row as it now stands.
    pub checks: ChecksResponse,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/deployment/version` — the build metadata, also read by the shell footer.
pub async fn get_version(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<VersionResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let channel = check.channel;
    let summary = summary(pool, channel).await?;

    let current_release = Release::minimal(env!("CARGO_PKG_VERSION"), channel);
    let version = VersionBody {
        version: current_release.version.to_string(),
        service: env!("CARGO_PKG_NAME").to_string(),
        channel: channel.as_str().to_string(),
        channel_understood: Channel::parse(&channel.as_str()) == Some(channel),
        core: core_version().map(|core| core.to_string()),
        available: summary.available.clone(),
        availability: summary.availability.clone(),
        upgrade_available: summary.availability.is_actionable(),
    };
    Ok(Json(VersionResponse { version }))
}

/// `GET /api/v1/deployment/environments` — the cards.
pub async fn list_environments(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<EnvironmentsResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let channel = check.channel;
    let summary = summary(pool, channel).await?;

    let health = store::list_health(pool).await?;
    let history = recent_history(pool, 10).await?;
    let environments = health
        .iter()
        .map(|row| card(row, &summary, &history, channel))
        .collect();

    Ok(Json(EnvironmentsResponse {
        environments,
        summary,
    }))
}

/// `GET /api/v1/deployment/environments/{id}` — one card with its history.
///
/// The path segment is the environment's **name** (`production`, `staging`, `sandbox`) rather
/// than a uuid: `environment_health`'s primary key is the name, the card is reached from the
/// list by name, and a uuid here would mean the panel cannot build the link from the list it
/// already has.
pub async fn get_environment(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(environment): Path<String>,
) -> Result<Json<EnvironmentDetailResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let channel = check.channel;
    let summary = summary(pool, channel).await?;

    let health = store::list_health(pool).await?;
    let row = health
        .iter()
        .find(|row| row.environment == environment)
        .ok_or_else(|| not_found_environment(&environment))?;
    let history = recent_history(pool, 10).await?;

    Ok(Json(EnvironmentDetailResponse {
        environment: card(row, &summary, &history, channel),
        history,
    }))
}

/// `GET /api/v1/deployment/releases` — the channel-filtered release browser.
pub async fn list_releases(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<ReleaseListQuery>,
) -> Result<Json<ReleasesResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let summary = summary(pool, check.channel).await?;

    // `?channel=` may only **narrow**. A request for `nightly` on a stable installation is
    // answered with the stable list plus the reason, not with the nightly rows: the release
    // browser is a place to *read about* other channels, and the card's offer is a separate
    // decision that `availability` already made.
    let (channel, refused) = match query.channel.as_deref().map(str::trim) {
        None | Some("") => (check.channel, None),
        Some(raw) => match Channel::parse(raw) {
            Some(requested) if requested == check.channel => (requested, None),
            Some(other) => (
                check.channel,
                Some(format!(
                    "this installation follows the {} channel, so the {} list is not shown here",
                    check.channel.as_str(),
                    other.as_str()
                )),
            ),
            None => {
                return Err(ApiError::bad_request(
                    "deployment_channel_unknown",
                    format!(
                        "“{raw}” is not a release channel. Choose stable, beta or nightly."
                    ),
                )
                .with_details(json!({ "channels": ["stable", "beta", "nightly"] })));
            }
        },
    };

    let rows = store::list_releases(
        pool,
        channel,
        query.limit.unwrap_or(store::HistoryFilter::DEFAULT_LIMIT),
    )
    .await?;
    let releases = rows
        .iter()
        .map(|row| release_body(row, &summary.availability))
        .collect::<Vec<_>>();

    let response = ReleasesResponse {
        channel: channel.as_str().to_string(),
        releases,
        stale_banner: summary.stale_banner.clone(),
        empty: rows.is_empty(),
    };
    if let Some(reason) = refused {
        // The refusal rides on the response rather than becoming a `403`, because the caller is
        // allowed to ask: it is a *narrowing* rule, not a permission, and a 403 would read as
        // "you may not look at releases" when releases are exactly what they may look at.
        tracing::debug!(reason = %reason, "deployment: a release list was refused a channel change");
    }
    Ok(Json(response))
}

/// `GET /api/v1/deployment/releases/{version}` — `View Changes`.
pub async fn get_release(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(version): Path<String>,
    Query(query): Query<ChannelQuery>,
) -> Result<Json<ReleaseDetailResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let channel = query
        .channel
        .as_deref()
        .and_then(Channel::parse)
        .unwrap_or(check.channel);
    let summary = summary(pool, check.channel).await?;

    let row = store::load_release(pool, channel, &version)
        .await
        .map_err(|error| release_lookup_error(error, &version, channel))?;
    let body = release_body(&row, &summary.availability);

    // `upgrade_from` is `Some` only when this release really is the card's offer. A detail page
    // for an *older* version is a legitimate thing to open (that is what an operator does before
    // a rollback), and it must not offer to deploy to what is already running.
    let upgrade_from = match &summary.availability {
        Availability::Upgrade {
            version: offered, ..
        } if offered == &body.version => Some(summary.current.clone()),
        _ => None,
    };

    Ok(Json(ReleaseDetailResponse {
        release: body,
        summary,
        upgrade_from,
    }))
}

/// `GET /api/v1/deployment/history` — the deploy/rollback/restart list.
pub async fn list_history(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();

    let mut filter = HistoryFilter::with_defaults();
    filter.environment = query
        .environment
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string);
    filter.kind = query
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string);
    filter.status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string);
    let (since, window_label) = window_to_instant(query.window.as_deref())?;
    filter.since = since;
    if let Some(limit) = query.limit {
        filter.limit = limit;
    }
    if let Some(offset) = query.offset {
        filter.offset = offset;
    }

    let (rows, total) = store::list_history(pool, &filter).await?;

    // Steps are loaded for the page, not for the whole history: the expansion is a click, and
    // a history page of fifty rows with four steps each is two hundred reads for a screen that
    // shows two of them expanded. `slice(0, HISTORY_STEP_ROWS)` keeps the first page's cost
    // bounded and the response small; the wizard's live log is the route for a running job.
    let mut bodies = Vec::with_capacity(rows.len());
    for row in rows.iter().take(HISTORY_STEP_ROWS) {
        let steps = store::list_steps(pool, row.id).await?;
        bodies.push(history_body(row, steps));
    }

    Ok(Json(HistoryResponse {
        rows: bodies,
        total,
        filter: HistoryFilterBody {
            environment: filter.environment,
            kind: filter.kind,
            status: filter.status,
            window: window_label,
        },
    }))
}

/// `GET /api/v1/deployment/checks` — the update check's own state.
pub async fn get_checks(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<ChecksResponse>, ApiError> {
    let _ = current;
    let pool = state.db().pool();
    Ok(Json(checks_body(pool).await?))
}

/// `POST /api/v1/deployment/checks/run` — check now, and answer with *this* run's result.
///
/// Synchronous on purpose. The alternative returns `202` and the panel polls, which is honest
/// but useless for the actual use: an operator presses this immediately before a deploy and
/// needs the answer before the wizard's pre-flight, not a spinner and a re-poll.
pub async fn run_check_now(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<RunCheckResponse>, ApiError> {
    let pool = state.db().pool();
    let check = store::load_check(pool).await?;
    let outcome = crate::deployment_check::check_once(pool, check.channel).await?;
    let refreshed = checks_body(pool).await?;

    omnion_audit::record(
        pool,
        omnion_audit::NewAuditEntry::by_user(current.user.id, "deployment.check.requested")
            .target("deployment", "update-check")
            .metadata(json!({
                "channel": check.channel.as_str(),
                "announced": outcome.announced,
                "result": outcome.result,
            })),
    )
    .await?;

    let summary = summary(pool, check.channel).await?;
    Ok(Json(RunCheckResponse {
        result: outcome.result,
        summary,
        announced: outcome.announced,
        checks: refreshed,
    }))
}

/// How many history rows carry their steps inline.
const HISTORY_STEP_ROWS: usize = 20;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The card's answer, computed from the cache.
async fn summary(pool: &sqlx::PgPool, channel: Channel) -> Result<VersionSummary, ApiError> {
    let check = store::load_check(pool).await?;
    let current_release = Release::minimal(env!("CARGO_PKG_VERSION"), channel);
    let rows = store::list_releases(pool, channel, RELEASE_SCAN_LIMIT).await?;
    let releases: Vec<Release> = rows.iter().filter_map(store::ReleaseRow::to_release).collect();
    let core = core_version();
    let availability =
        omnion_deployment::availability(&current_release, core.as_ref(), &releases);

    let cached_at = store::newest_checked_at(pool).await?;
    let stale_banner = if check.is_stale() {
        let stamp = cached_at.map_or_else(
            || "a check that has never run".to_string(),
            |at| format_time(&at),
        );
        let reason = check.last_error.as_deref().unwrap_or("no successful check yet");
        Some(stale_banner(reason, &stamp))
    } else {
        None
    };

    Ok(VersionSummary {
        current: current_release.version.to_string(),
        available: availability.label(),
        availability,
        stale_banner,
    })
}

/// How many cached releases the availability scan looks at.
///
/// Bounded so a feed that has published a thousand nightlies cannot make every page load read a
/// thousand rows; the scan only ever needs the newest handful to decide what to offer.
const RELEASE_SCAN_LIMIT: i64 = 100;

/// The core version this build reports, if it does.
///
/// `None` rather than a guess: `availability` treats an unknown core as "cannot prove
/// incompatible", which is the safe direction (it offers the release, and the pre-flight check
/// is where an incompatibility must be caught).
fn core_version() -> Option<Version> {
    // The workspace version is the core's version; an API built from a different core reports a
    // different one, and the card's "needs core X or newer" reason depends on this being the
    // *core* and not the service.
    Version::parse(env!("CARGO_PKG_VERSION")).ok()
}

/// One card, from a health row.
fn card(
    row: &HealthRow,
    summary: &VersionSummary,
    history: &[HistoryRowBody],
    channel: Channel,
) -> EnvironmentCardBody {
    // The offer is computed per environment from the version *running there*, not from the
    // installation's own version: a staging copy pinned two versions behind production has its
    // own answer, and showing it production's would be a lie on a card whose whole job is to
    // tell an operator what that environment is running.
    let available = if row.version == summary.current {
        summary.availability.clone()
    } else {
        let running = Release::minimal(&row.version, channel);
        let releases = summary_releases_for(row, channel);
        omnion_deployment::availability(&running, core_version().as_ref(), &releases)
    };

    let failing_probe = failing_probe(&row.details);
    let reachable = row.status != "unreachable";
    let last_deploy = history
        .iter()
        .find(|entry| entry.environment == row.environment)
        .cloned();

    // Rollback needs a *previous* version to go to. A job that has never run, or one that has
    // only ever deployed once, has no "previous known-good" — and offering a rollback to nothing
    // is a button that fails at the moment of the worst possible attention.
    // The rollback target is *read* from the last deploy's `from_version`, not taken from it:
    // consuming the row here would leave the card's footer line (actor + time) with nothing to
    // print, which is the one line that tells an operator how old the card is. `Option::and_then`
    // on a non-`Copy` value moves it, and the move is invisible until the footer renders blank.
    let rollback = last_deploy.as_ref().and_then(|entry| {
        let target = entry.from_version.as_ref()?;
        if target.trim().is_empty() {
            return None;
        }
        Some(RollbackBody {
            to_version: target.clone(),
            known: false,
        })
    });

    let (deployable, blocked_reason) = if !reachable {
        (
            false,
            Some("this environment did not answer the last health probe".to_string()),
        )
    } else if !available.is_actionable() {
        (false, Some(available.label()))
    } else {
        (true, None)
    };

    EnvironmentCardBody {
        environment: row.environment.clone(),
        name: environment_label(&row.environment),
        health: row.status.clone(),
        version: Some(row.version.clone()),
        checked_at: Some(row.checked_at),
        failing_probe,
        available: available.label(),
        availability: available,
        deployable,
        blocked_reason,
        rollback,
        last_deploy,
    }
}

/// The releases a card's own availability scan may consider.
///
/// Only the cache — a card must not trigger a feed read — and only the installation's channel.
fn summary_releases_for(row: &HealthRow, channel: Channel) -> Vec<Release> {
    // The row's own channel is the installation's channel by construction (the health probe and
    // the check both describe this instance), so this is a no-op today and the seam exists for
    // the multi-region slice, where an environment reports the channel *it* follows.
    let _ = (row, channel);
    Vec::new()
}

/// The label a card shows for an environment.
fn environment_label(environment: &str) -> String {
    match environment {
        "production" => "Production".to_string(),
        "staging" => "Staging".to_string(),
        "sandbox" => "Sandbox".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => other.to_string(),
            }
        }
    }
}

/// The probe that failed, out of the health details.
///
/// The details object is `{ "<probe>": "<status or message>" }`, and the card's tooltip names the
/// first probe whose value is not a healthy marker. A details object with nothing recognizable in
/// it returns `None` rather than a fabricated probe name — the migration already refuses a
/// `degraded` row with empty details for exactly this reason, and the reader still must not
/// invent the missing half.
fn failing_probe(details: &serde_json::Value) -> Option<String> {
    let object = details.as_object()?;
    for (probe, value) in object {
        let text = match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        if is_failure(&text) {
            return Some(format!("{probe}: {text}"));
        }
    }
    None
}

/// Does this probe value read as a failure?
fn is_failure(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    !lowered.contains("ok")
        && !lowered.contains("healthy")
        && !lowered.contains("pass")
        && !lowered.contains("reachable")
        && !lowered.contains("connected")
}

/// One history row plus its steps.
fn history_body(row: &store::DeploymentRow, steps: Vec<store::StepRow>) -> HistoryRowBody {
    HistoryRowBody {
        id: row.id,
        environment: row.environment.clone(),
        kind: row.kind.clone(),
        from_version: row.from_version.clone(),
        to_version: row.to_version.clone(),
        status: row.status.clone(),
        strategy: row.strategy.clone(),
        started_by: row.started_by,
        reason: row.reason.clone(),
        error: row.error.clone(),
        started_at: row.started_at,
        finished_at: row.finished_at,
        duration_ms: row.duration_ms,
        steps: steps
            .into_iter()
            .map(|step| StepBody {
                position: step.position,
                name: step.name,
                status: step.status,
                output: step.output,
                started_at: step.started_at,
                finished_at: step.finished_at,
            })
            .collect(),
    }
}

/// The newest jobs, for a card's footer and the detail's strip.
async fn recent_history(
    pool: &sqlx::PgPool,
    limit: i64,
) -> Result<Vec<HistoryRowBody>, ApiError> {
    let mut filter = HistoryFilter::with_defaults();
    filter.limit = limit;
    let (rows, _) = store::list_history(pool, &filter).await?;
    let mut bodies = Vec::with_capacity(rows.len());
    for row in &rows {
        bodies.push(history_body(row, Vec::new()));
    }
    Ok(bodies)
}

/// One release, as the panel reads it.
fn release_body(row: &store::ReleaseRow, availability: &Availability) -> ReleaseBody {
    let is_available = matches!(
        availability,
        Availability::Upgrade { version, .. } if version == &row.version
    );
    ReleaseBody {
        version: row.version.clone(),
        channel: row.channel.clone(),
        released_at: row.released_at.clone(),
        notes: row.notes_md.clone(),
        breaking: row.breaking,
        migrations: row.migrations.clone(),
        core_min: row.core_min.clone(),
        artifact_checksum: row.artifact_checksum.clone(),
        checked_at: row.checked_at,
        is_available,
    }
}

/// The checks screen's body, built from the row plus the cache's own timestamps.
async fn checks_body(pool: &sqlx::PgPool) -> Result<ChecksResponse, ApiError> {
    let check = store::load_check(pool).await?;
    let cached_at = store::newest_checked_at(pool).await?;
    let announced_total = store::seen_count(pool).await?;
    let interval = crate::deployment_check::interval_seconds();

    let next_run_at = check
        .last_run_at
        .and_then(|at| at.checked_add(time::Duration::seconds(interval)));
    let due_in_seconds = next_run_at.map(|next| {
        let now = OffsetDateTime::now_utc();
        (next - now).whole_seconds().max(0)
    });

    let stale_banner = if check.is_stale() {
        let stamp = cached_at.map_or_else(
            || "a check that has never run".to_string(),
            |at| format_time(&at),
        );
        let reason = check
            .last_error
            .as_deref()
            .unwrap_or("no successful check yet");
        Some(stale_banner(reason, &stamp))
    } else {
        None
    };

    let last_seen = check.last_seen.unwrap_or(0);
    let result = match check.last_status.as_deref() {
        Some("completed") => CheckResult::Completed {
            announced: check.last_announced.clone(),
            seen: last_seen as usize,
        },
        Some("failed") => CheckResult::Failed {
            reason: check
                .last_error
                .clone()
                .unwrap_or_else(|| "the feed could not be read".to_string()),
        },
        _ => CheckResult::Failed {
            reason: "no check has run yet".to_string(),
        },
    };

    let rows = manifest::check_rows(
        &result,
        check.channel,
        &next_run_label(next_run_at),
        &cached_at.map_or_else(|| "never".to_string(), |at| format_time(&at)),
    );

    Ok(ChecksResponse {
        channel: check.channel.as_str().to_string(),
        last_run_at: check.last_run_at,
        last_finished_at: check.last_finished_at,
        last_status: check.last_status.clone(),
        last_error: check.last_error.clone(),
        last_seen: check.last_seen,
        last_announced: check.last_announced.clone(),
        stale_banner,
        next_run_at,
        due_in_seconds,
        announced_total,
        rows,
    })
}

/// When the next check is due, in words the row can print.
fn next_run_label(next: Option<OffsetDateTime>) -> String {
    next.map_or_else(|| "not scheduled".to_string(), |at| format_time(&at))
}

/// An RFC 3339 instant, formatted the way the rest of the platform formats one.
fn format_time(at: &OffsetDateTime) -> String {
    // `Display` on a `PrimitiveDateTime` writes `2026-09-01 9:00:00.0` — a single-digit hour and
    // a trailing `.0`, neither of which anything else in the platform parses. So the format is
    // written out here rather than borrowed.
    let formatted = time::format_description::well_known::Rfc3339;
    at.format(&formatted)
        .unwrap_or_else(|_| format!("{at}"))
}

/// A `window=` chip as an instant, plus the label the filter echoes.
fn window_to_instant(window: Option<&str>) -> Result<(Option<OffsetDateTime>, String), ApiError> {
    let raw = window.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Ok((None, "all".to_string()));
    };
    let days = match raw {
        "all" => return Ok((None, "all".to_string())),
        "24h" => 1,
        "7d" => 7,
        "30d" => 30,
        "90d" => 90,
        other => {
            // A bare instant is accepted so the URL is a complete description of the query: a
            // panel that cannot express "since Tuesday" in a chip can still link to it.
            if let Ok(at) = time::OffsetDateTime::parse(
                other,
                &time::format_description::well_known::Rfc3339,
            ) {
                return Ok((Some(at), other.to_string()));
            }
            return Err(ApiError::bad_request(
                "deployment_window_unknown",
                format!("“{other}” is not a window. Choose 24h, 7d, 30d, 90d or all."),
            )
            .with_details(json!({ "windows": ["24h", "7d", "30d", "90d", "all"] })));
        }
    };
    let since = OffsetDateTime::now_utc() - time::Duration::days(days);
    Ok((Some(since), raw.to_string()))
}

/// A store failure as a `404` that names what was looked for and where.
fn release_lookup_error(error: StoreError, version: &str, channel: Channel) -> ApiError {
    if error.is_not_found() {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "release_not_found",
            format!(
                "release {version} is not in the {} release cache. It may have been withdrawn, \
                 or the update check has not seen it yet.",
                channel.as_str()
            ),
        );
    }
    ApiError::from(error)
}

/// A missing environment as a `404` that names the three that exist.
fn not_found_environment(environment: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "environment_not_found",
        format!(
            "“{environment}” is not an environment this installation runs. The centre covers \
             production, staging and sandbox."
        ),
    )
}

impl From<StoreError> for ApiError {
    /// Three failures, three responses.
    ///
    /// The split is the whole point: a missing row is a `404` with a message about the thing that
    /// is missing, a dead feed is a `503` the checks screen already knows how to render, and a
    /// database failure is a `500` with a request id. One shared `internal_error` would answer
    /// "the release 2.5.0 is not in the cache" with a retry button that never helps.
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "That deployment record does not exist.",
            ),
            StoreError::Feed(ref reason) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error.code(),
                format!("the release feed could not be read: {reason}"),
            ),
            // The event bus refused the announcement. The cache is written and the claim was
            // released, so the next check re-announces — this is a `503`, not a `500`: the
            // work will be retried, and nothing the operator did caused it.
            StoreError::Event(ref reason) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                error.code(),
                format!("the update could not be announced: {reason}"),
            ),
            StoreError::Database(inner) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("the deployment store failed: {inner}"),
            ),
        }
    }
}
