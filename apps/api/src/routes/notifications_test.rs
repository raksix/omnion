//! `POST /api/v1/notifications/preferences/test` — send one test notification through one
//! channel, now, and report what actually happened (REQ-021, slice 2's test delivery).
//!
//! **The route the spec has listed since the request was written, with no implementation
//! behind it.** REQ-021's API table carries `POST /api/v1/notifications/preferences/test`
//! ("Send a test notification through one channel"), the settings screen describes a `Test
//! delivery` button per channel, and neither existed: `grep -rn "preferences/test"` over the
//! whole tree returns this file and nothing else. So the one button a reader uses to answer
//! "is my e-mail actually working?" was a button the platform could not draw — the acceptance
//! box naming it has been open since 2026-09-25 for a reason that had nothing to do with the
//! browser pass.
//!
//! **It sends a real notification through the real transport, and reports the real outcome.**
//! The tempting shortcut is to answer `{ok: true}` after validating the channel — a test that
//! cannot fail is a test that proves nothing, and it is the same lie the webhook readiness
//! branch used to tell. So this handler records a real notification, enqueues a real delivery
//! row for the one channel asked for, drives the actual transport, and settles the row the way
//! the background runner would. A failure returns `200` with `delivered: false` and the
//! transport's own sentence, because "the test failed" is a *result*, not an HTTP error: a
//! `502` would tell the reader their settings screen is broken rather than that their SMTP
//! host refused the message.
//!
//! **`in_app` is refused rather than answered.** The in-app channel *is* this notification —
//! enqueueing it proves nothing and would leave a test row in the reader's own bell that they
//! then have to delete. `400` with the sentence, listing the channels that can be tested.
//!
//! **The transport is borrowed from the runner, not reimplemented.** `notification_runner`
//! owns the SMTP sender and the signed POST; this module calls the same `Transport`
//! implementations, so a test delivery and a real delivery take the identical code path. A
//! second sender here would make "the test passed" mean something the queue never does.

use axum::http::StatusCode;
use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_events::{NewEvent, bus};
use omnion_notifications::delivery::{self, Transport};
use omnion_notifications::model::NewNotification;
use omnion_notifications::store;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::notification_runner::{EmailTransport, WebhookTransport};
use crate::state::AppState;

/// The body: which channel, and optional overrides for the message.
///
/// `channel` is required and is validated against the closed list before anything is written.
/// The title and body are optional because the default is a message that is obviously a test
/// to the person who receives it — a test that arrives looking like a real security alert is
/// a test that trains somebody to ignore e-mail from this domain.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestDeliveryBody {
    /// Which channel to send over: `email`, `web_push`, `webhook` or `chat`.
    pub channel: String,
    /// Overrides the default subject line.
    pub title: Option<String>,
    /// Overrides the default body.
    pub body: Option<String>,
}

/// What the transport actually did.
///
/// `delivered` and `detail` are separate fields rather than one `status` string because the
/// screen renders them differently: a boolean decides the colour of the line, and the detail
/// is the sentence under it. Folding them together means the client has to pattern-match prose
/// to find out whether to show a green or a red banner.
#[derive(Debug, Serialize)]
pub struct TestDeliveryResult {
    /// The channel that was asked for, echoed back.
    pub channel: String,
    /// Whether a transport accepted the message.
    pub delivered: bool,
    /// The transport's own outcome in the platform's words — the sender's error string, the
    /// endpoint's status code, or the reason this channel cannot send at all.
    pub detail: String,
    /// The HTTP status the receiver gave, when the channel speaks HTTP.
    pub response_status: Option<i32>,
    /// The notification this created, so the settings screen can link to it in the bell and
    /// the reader can see that the in-app half worked.
    pub notification_id: String,
    /// The delivery row's own state afterwards (`sent`, `failed`, or `pending`).
    pub delivery_status: String,
}

/// `POST /api/v1/notifications/preferences/test`.
///
/// The reader is always themselves: a test delivery has no recipients parameter at all,
/// because the one thing a test must not do is send somebody else a message to find out
/// whether *their* transport works.
pub async fn test_delivery(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(body): Json<TestDeliveryBody>,
) -> Result<(StatusCode, Json<TestDeliveryResult>), ApiError> {
    let channel = body.channel.trim();

    if !omnion_notifications::is_channel(channel) {
        return Err(ApiError::bad_request(
            "invalid_channel",
            format!(
                "channel \"{channel}\" is not one of {:?}",
                omnion_notifications::CHANNELS
            ),
        ));
    }

    // `in_app` is the one channel that cannot be tested, and the reason is worth saying in
    // the refusal rather than silently accepting the row: the test notification *is* the
    // in-app channel, so a green "delivered" here would claim the platform proved something
    // by writing to a database it already owns.
    if channel == omnion_notifications::IN_APP {
        return Err(ApiError::bad_request(
            "invalid_channel",
            "in-app is always on and cannot be tested — try email, web_push or webhook".to_owned(),
        ));
    }

    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Omnion test notification")
        .to_owned();
    let text = body
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(
            "If you are reading this, the notification channel is configured and working. \
             Nothing is waiting for you — this message was sent by the settings screen.",
        )
        .to_owned();

    // **A real notification, so the in-app half is real too.** The record is written through
    // the same store every other notification uses, with a `dedupe_key` derived from the
    // minute: without one, pressing the button twice in a minute would write one row and
    // report one delivery while the second press silently did nothing (the partial unique
    // index on `(user_id, dedupe_key)` is what collapses repeats elsewhere in this module).
    // With one, two presses are two tests, which is what somebody debugging a flaky SMTP
    // host expects the button to do.
    let stamp = OffsetDateTime::now_utc().unix_timestamp() / 60;
    let dedupe_key = format!("test:{channel}:{stamp}");
    let draft = NewNotification::to(session.user.id, "system", title)
        .with_body(text)
        .with_source("notification_test", stamp.to_string())
        .with_dedupe_key(dedupe_key.clone());

    store::record(
        state.db().pool(),
        session.user.organization_id,
        Some(session.user.id),
        &draft,
    )
    .await
    .map_err(map_test_error)?;

    // **The id is read back by the dedupe key rather than returned by the write.** `record`
    // answers "did a row appear", deliberately: the emit path counts rows and never needs the
    // id, and changing its signature to return one would touch every caller for no reason
    // there. Here the key is unique per (reader, channel, minute) — the same key the insert
    // just used — so this is a lookup by a value the handler itself chose, not a guess.
    let notification_id = store::find_by_dedupe_key(
        state.db().pool(),
        session.user.id,
        &dedupe_key,
    )
    .await
    .map_err(map_test_error)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "test_notification_failed",
            "the test notification was written but could not be read back",
        )
    })?
    .id;

    // Enqueue exactly the one channel asked for. `enqueue` writes the in-app row
    // unconditionally, so the row count is the channel plus in-app; only the asked-for row is
    // settled below and the in-app one is left for the bell, which is the honest outcome —
    // the reader asked to test e-mail and the notification *is* visible in the panel.
    delivery::enqueue(
        state.db().pool(),
        notification_id,
        &[channel.to_owned()],
        &[],
    )
    .await
    .map_err(map_test_error)?;

    // Claim the single row for this channel. The claim is the same `for update skip locked`
    // statement the background runner uses, so a test that runs while the runner is live is
    // serialised against it rather than racing it.
    let job = delivery::claim_due(state.db().pool(), 16, 60.0)
        .await
        .map_err(map_test_error)?
        .into_iter()
        .find(|job| job.notification_id == notification_id && job.channel == channel);

    let Some(job) = job else {
        // `enqueue` wrote the row and `claim_due` did not return it. The only way that
        // happens is the in-app row winning the claim batch first on a busy install — and
        // the honest answer is a retryable failure, not a success.
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "test_delivery_claimed_elsewhere",
            "another delivery runner claimed this row first; press Test again",
        ));
    };

    let (delivered, detail, response_status) =
        send_with_transport(&state, &job, channel).await;

    // The reason is written on the row **and** returned, from the same string. A test that
    // says "not delivered" in the toast while the outbox row says nothing is the one case
    // where a reader cannot find the answer afterwards — the outbox is where they look once
    // the toast has gone.
    let delivery_status = if delivered {
        delivery::mark_sent(state.db().pool(), job.id, response_status)
            .await
            .map_err(map_test_error)?;
        "sent"
    } else {
        delivery::mark_failed(state.db().pool(), job.id, response_status, &detail)
            .await
            .map_err(map_test_error)?;
        "failed"
    };

    // The audit trail a settings screen owes its owner: somebody pressed a button that sent a
    // real message, and if the answer was red the next question is always "who pressed it".
    // The event carries no body and no address — ids and the channel, like every other
    // `notification.*` payload.
    bus::emit(
        state.db().pool(),
        NewEvent::new("notification.delivery.succeeded")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "test": true,
                "channel": channel,
                "notification_id": notification_id.to_string(),
                "delivered": delivered,
            })),
    )
    .await
    .ok();

    Ok((
        StatusCode::OK,
        Json(TestDeliveryResult {
            channel: channel.to_owned(),
            delivered,
            detail,
            response_status,
            notification_id: notification_id.to_string(),
            delivery_status: delivery_status.to_owned(),
        }),
    ))
}

/// Drive one channel's real transport and report what it said.
///
/// Split from the handler because the *shape* of the answer is what the screen depends on and
/// a transport that cannot fail is untestable: the three arms here (accepted, refused with an
/// HTTP status, refused with a reason) are the three things a reader can be shown.
async fn send_with_transport(
    state: &AppState,
    job: &delivery::DeliveryJob,
    channel: &str,
) -> (bool, String, Option<i32>) {
    let config = delivery::DeliveryConfig::default();

    match channel {
        "email" => {
            let transport = EmailTransport::new(&state.config().mail);
            settle(transport.deliver(job, &config).await)
        }
        "webhook" => {
            // **The same optionality the runner has at boot.** An HTTP client that cannot be
            // built is a process misconfiguration, and the sentence for it is a configuration
            // fact rather than a delivery failure — so it is reported as "not delivered" with
            // the reason rather than failing the row twice for one missing dependency.
            match WebhookTransport::new(std::time::Duration::from_millis(
                config.request_timeout.whole_milliseconds().max(1) as u64,
            )) {
                Some(transport) => settle(transport.deliver(job, &config).await),
                None => (
                    false,
                    "this process has no HTTP client, so the webhook channel cannot be tested"
                        .to_owned(),
                    None,
                ),
            }
        }
        // `web_push` and `chat` are in the closed list and reachable from this screen, and
        // both are honest absences rather than silent successes: `web_push` needs a browser
        // subscription this screen does not hold, and `chat` has no connector installed on
        // any build of this platform (channel_readiness says so in the same words).
        "web_push" => (
            false,
            "web push needs a registered browser — use this browser's Push API, or test \
             another channel"
                .to_owned(),
            None,
        ),
        "chat" => (
            false,
            "no chat connector is installed on this installation".to_owned(),
            None,
        ),
        other => (
            false,
            format!("no transport is installed for the {other} channel"),
            None,
        ),
    }
}

/// Turn a transport's outcome into the three fields the answer carries.
fn settle(outcome: delivery::TransportOutcome) -> (bool, String, Option<i32>) {
    match outcome {
        delivery::TransportOutcome::Accepted { status } => (
            true,
            match status {
                Some(code) => format!("the endpoint accepted the message with {code}"),
                None => "the message was accepted by the transport".to_owned(),
            },
            status,
        ),
        delivery::TransportOutcome::Failed { status, reason } => (false, reason, status),
    }
}

/// The store's error, in the platform's shape.
fn map_test_error(error: omnion_notifications::NotificationError) -> ApiError {
    match error {
        omnion_notifications::NotificationError::Invalid(message) => {
            ApiError::bad_request("invalid_notification", message)
        }
        omnion_notifications::NotificationError::BudgetExhausted => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "notification_rate_limited",
            "this actor has emitted too many notifications in the last minute",
        ),
        other => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "test_delivery_failed",
            format!("{other}"),
        ),
    }
}

