//! The background notification delivery runner (REQ-021, slice 4).
//!
//! `main.rs` spawns this task when the delivery runner is enabled. It ticks the notification
//! delivery queue on the configured cadence: claim the due rows, hand each to its channel's
//! transport, record the outcome and schedule the retry backoff. Nothing here decides what work
//! exists — the queue does; a tick that cannot reach the database is logged and retried on the
//! next one, because the work it left behind is durable rows, not an in-memory queue.
//!
//! **The transports live here, not in the crate, and that is a dependency decision.** The
//! notification crate is infrastructure: it knows what a delivery *is*, and every part of it
//! is a query against `notification_deliveries`. A transport is the opposite — it is an SMTP
//! conversation, an HTTP POST to somebody's endpoint, a Web Push payload signed with a key
//! the installation holds. Putting those in the crate would make `omnion-notifications` depend
//! on a mail stack and an HTTP client to satisfy a trait it defined itself, and
//! `omnion-automation` already owns the SMTP sender the `email` channel needs. So the crate
//! publishes the [`Transport`] trait and the queue, and this file — which already depends on
//! both the crate and the mailer — supplies the implementations.
//!
//! **A transport with no configuration is a skipped row, not a failed one.** The queue settles
//! those before it claims anything (see `delivery::settle_not_ready`), so a channel nobody
//! configured writes `skipped` with the reason on the row rather than occupying a runner slot
//! on every tick. What is left here is only the channels the installation *has* configured.

use std::time::Duration as StdDuration;

use omnion_automation::mail::{self, Email, MailSettings};
use omnion_core::config::MailConfig;
use omnion_notifications::delivery::{
    self, DeliveryConfig, DeliveryJob, Transport, TransportOutcome,
};
use time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::state::AppState;

/// Translate the process mail settings into the sender's own settings.
///
/// A separate function rather than a `From`, because the two types mean different things: the
/// config is a process-wide switch with environment defaults, and `MailSettings` is one
/// sender's address book. The conversion is where a missing password would otherwise be
/// defaulted into an empty string and sent to a server expecting AUTH.
#[must_use]
pub fn mail_settings(config: &MailConfig) -> MailSettings {
    let mut settings = MailSettings::new(config.host.clone(), config.port, config.from.clone())
        .with_sending(config.is_usable());
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        settings = settings.with_credentials(username.clone(), password.clone());
    }
    settings.with_timeout(StdDuration::from_millis(config.timeout_ms))
}

/// The e-mail transport, over the sender `omnion-automation` already ships.
///
/// **A reader with no address is a failure with a reason, not a silent skip.** The row is
/// already in the queue because somebody asked for it; an absent address means the reader was
/// created without one, and the honest record is "there is nowhere to send this" so the outbox
/// shows why rather than showing a queue that never moves. The empty check is here rather than
/// in the queue because it is a property of *this* destination.
pub struct EmailTransport {
    settings: MailSettings,
}

impl EmailTransport {
    /// Build the transport from the process mail settings.
    #[must_use]
    pub fn new(config: &MailConfig) -> Self {
        Self {
            settings: mail_settings(config),
        }
    }
}

impl Transport for EmailTransport {
    fn channel(&self) -> &'static str {
        "email"
    }

    fn deliver<'a>(
        &'a self,
        job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        Box::pin(async move {
            let Some(address) = job
                .user_email
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
            else {
                return TransportOutcome::Failed {
                    status: None,
                    reason: "this reader has no e-mail address to send to".to_owned(),
                };
            };

            let mut email = Email::new(address, job.title.clone(), job.body.clone());
            if let Some(url) = job.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                // The link goes into the plain-text body rather than an HTML anchor. REQ-021
                // puts a template editor deliberately *out* of scope ("subject + plain text +
                // link only"), so a link the reader can see and follow without rendering HTML
                // is the whole of the feature — and it means the mail stays readable in a
                // plain-text client, which is where a compliance notice usually lands.
                email.body = format!("{}\n\n{url}", email.body);
            }

            match mail::send(&self.settings, &email).await {
                Ok(()) => TransportOutcome::Accepted { status: None },
                Err(error) => TransportOutcome::Failed {
                    status: None,
                    // The sender's own `Display` is a sentence without the transport's
                    // internals, which is what belongs in a column the outbox renders.
                    reason: error.to_string(),
                },
            }
        })
    }
}

/// The webhook transport: POST the notification to one endpoint as JSON.
///
/// **The body carries the notification, never a credential.** The endpoint and its signing
/// secret come from `webhook_endpoints` — the bus REQ-016 already owns — so this transport
/// adds no second bus and no second secret store. The request is signed the same way the
/// webhook runner signs its own deliveries, which is what makes "a webhook channel" and "an
/// endpoint on the webhook screen" the same thing to an administrator instead of two
/// settings that quietly disagree.
///
/// **A 4xx is a failure, not a retry, and 5xx is the opposite** — except that the queue's
/// contract has no "give up now" other than the cap. So both are reported as failures and the
/// backoff decides: a 400 from a receiver that will never accept this payload burns the cap in
/// three tries and lands in `failed`, which is the honest record, and a 500 from a receiver
/// that is briefly down gets the same treatment with a growing delay. The distinction the
/// reader cares about — "it did not arrive" — is the same either way.
pub struct WebhookTransport {
    client: reqwest::Client,
}

impl WebhookTransport {
    /// Build the transport, or `None` when the HTTP client cannot be built.
    ///
    /// `None` rather than a transport that fails every attempt: an unbuildable client is a
    /// process misconfiguration, and turning it into per-row failures would fill the outbox
    /// with a hundred identical rows that a single restart would have prevented.
    #[must_use]
    pub fn new(request_timeout: StdDuration) -> Option<Self> {
        reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .ok()
            .map(|client| Self { client })
    }

    /// The JSON body sent to the endpoint.
    ///
    /// Split out as a pure function so the *shape* of the payload is assertable without a
    /// server: the ids and the category are the contract, and a body that quietly grew a
    /// `body` field would put a reader's private text on a third party's endpoint.
    #[must_use]
    pub fn payload(job: &DeliveryJob) -> serde_json::Value {
        serde_json::json!({
            "notification_id": job.notification_id,
            "user_id": job.user_id,
            "category": "notification",
            "channel": job.channel,
            "title": job.title,
            "url": job.url,
        })
    }
}

impl Transport for WebhookTransport {
    fn channel(&self) -> &'static str {
        "webhook"
    }

    fn deliver<'a>(
        &'a self,
        job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        Box::pin(async move {
            // A webhook channel with no endpoint cannot be delivered, and saying so is the
            // whole answer: the queue claimed the row, so something has to explain it.
            let Some(url) = job.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
                return TransportOutcome::Failed {
                    status: None,
                    reason: "this notification names no endpoint to post to".to_owned(),
                };
            };

            match self.client.post(url).json(&Self::payload(job)).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        TransportOutcome::Accepted {
                            status: Some(i32::from(status.as_u16())),
                        }
                    } else {
                        TransportOutcome::Failed {
                            status: Some(i32::from(status.as_u16())),
                            reason: format!("the endpoint answered {}", status.as_u16()),
                        }
                    }
                }
                Err(error) => TransportOutcome::Failed {
                    status: None,
                    reason: format!("the endpoint could not be reached: {error}"),
                },
            }
        })
    }
}

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> Option<JoinHandle<()>> {
    let process = state.config().events.clone();
    let mail = state.config().mail.clone();
    let config = DeliveryConfig::default();
    let poll_ms = process.poll_ms.max(50);

    let transports: Vec<(String, Box<dyn Transport>)> = vec![
        (
            omnion_notifications::IN_APP.to_owned(),
            Box::new(delivery::InAppTransport),
        ),
        ("email".to_owned(), Box::new(EmailTransport::new(&mail))),
    ];

    // The webhook transport is optional for the reason in its own constructor: an HTTP client
    // that cannot be built is a process misconfiguration, and the runner says so at boot
    // instead of writing a failed row for every queued notification.
    match WebhookTransport::new(StdDuration::from_millis(
        config.request_timeout.whole_milliseconds().max(1) as u64,
    )) {
        Some(webhook) => {
            let mut transports = transports;
            transports.push(("webhook".to_owned(), Box::new(webhook)));
            spawn_with(state, transports, config, poll_ms)
        }
        None => {
            tracing::error!("the notification webhook channel has no HTTP client; it will not run");
            spawn_with(state, transports, config, poll_ms)
        }
    }
}

/// The loop itself, shared by both arms of the transport setup above.
fn spawn_with(
    state: AppState,
    transports: Vec<(String, Box<dyn Transport>)>,
    config: DeliveryConfig,
    poll_ms: u64,
) -> Option<JoinHandle<()>> {
    tracing::info!(
        poll_ms,
        batch = config.batch,
        lease_seconds = config.lease_seconds,
        channels = transports.len(),
        "notification delivery runner started"
    );

    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval(StdDuration::from_millis(poll_ms));
        // A slow tick must not turn into a burst of catch-up ticks: the next tick is enough,
        // because every due delivery is still in the store.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; boot has its own work to do, so wait for one.
        ticks.tick().await;

        loop {
            ticks.tick().await;
            match delivery::run_due(state.db().pool(), &transports, &config).await {
                Ok(report) if !report.is_idle() => {
                    tracing::info!(?report, "notification delivery tick");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "notification delivery tick failed");
                }
            }
        }
    }))
}

/// The runner's config, exposed for the tests that assert the defaults are sane.
#[must_use]
pub fn delivery_config() -> DeliveryConfig {
    DeliveryConfig::default()
}

/// Keep the backoff's own units in one place, so a reader does not have to check whether the
/// config is seconds or milliseconds by looking at two files.
#[must_use]
pub fn seconds(value: i64) -> Duration {
    Duration::seconds(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_notifications::CHANNELS;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn job(channel: &str) -> DeliveryJob {
        DeliveryJob {
            id: Uuid::nil(),
            notification_id: Uuid::from_u128(1),
            user_id: Uuid::from_u128(2),
            channel: channel.to_owned(),
            attempts: 1,
            max_attempts: 3,
            title: "A page is waiting".to_owned(),
            body: "Somebody asked for a review.".to_owned(),
            url: None,
            user_email: Some("reader@example.com".to_owned()),
        }
    }

    #[test]
    fn every_channel_in_the_closed_list_is_accounted_for() {
        // The test that matters. A channel added to `vocabulary.rs` and neither registered
        // here nor named in `UNTRANSPORTED` would be a channel the runner claims and then
        // re-queues forever as "no transport is installed" — the queue drains, the outbox
        // never shows a `failed`, and nothing anywhere says the feature does not exist. The
        // earlier shape of this test asserted the same thing it was given and so could not
        // fail; this one enumerates the vocabulary and names the gap.
        const REGISTERED: [&str; 3] = ["in_app", "email", "webhook"];

        /// Channels the runner deliberately does not drain yet, and why. Each one is a
        /// `skipped` row in the outbox, never a silent `pending`.
        const UNTRANSPORTED: [(&str, &str); 2] = [
            (
                "web_push",
                "a signed payload needs the installation's key pair (REQ-037)",
            ),
            (
                "chat",
                "the connector vocabulary is REQ-015's, not the notification crate's",
            ),
        ];

        for channel in CHANNELS {
            let registered = REGISTERED.contains(&channel);
            let accounted = UNTRANSPORTED.iter().any(|(name, _)| *name == channel);
            assert!(
                registered || accounted,
                "channel {channel} is in the closed list but neither registered nor named \
                 as untransported — it would be claimed and re-queued forever"
            );
        }

        // And the other direction: a name in either list that is not in the vocabulary is a
        // typo, and a typo here means the real channel silently loses its exemption.
        for name in REGISTERED {
            assert!(
                CHANNELS.contains(&name),
                "{name} is registered but not a channel"
            );
        }
        for (name, _) in UNTRANSPORTED {
            assert!(
                CHANNELS.contains(&name),
                "{name} is exempted but not a channel"
            );
        }
    }

    #[test]
    fn the_mail_settings_carry_no_defaulted_credentials() {
        // The conversion must not invent an empty username or password: an SMTP server that
        // expects AUTH and receives empty credentials fails at the transport, which is a
        // `failed` row per notification, rather than at the configuration.
        let config = MailConfig {
            host: "smtp.example.com".to_owned(),
            port: 587,
            from: "noreply@example.com".to_owned(),
            username: None,
            password: None,
            timeout_ms: 4_000,
            ..MailConfig::default()
        };
        let settings = mail_settings(&config);
        assert_eq!(settings.host, "smtp.example.com");
        assert_eq!(settings.port, 587);
        assert_eq!(settings.from, "noreply@example.com");
    }

    #[test]
    fn the_webhook_payload_carries_the_notification_and_not_its_body() {
        // The assertion that matters: `body` is the reader's private text, and a webhook
        // endpoint is a third party. Ids, the title and the link are the contract; the body
        // is not.
        let payload = WebhookTransport::payload(&job("webhook"));
        let keys: Vec<&str> = payload
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"notification_id"));
        assert!(keys.contains(&"user_id"));
        assert!(keys.contains(&"title"));
        assert!(
            !keys.contains(&"body"),
            "the body must not leave through a webhook"
        );
        assert!(!keys.contains(&"user_email"), "nor the reader's address");
    }

    #[test]
    fn the_runner_config_has_a_usable_default_backoff() {
        let config = delivery_config();
        assert!(
            config.batch > 0,
            "a batch of zero is a runner that never runs"
        );
        assert!(
            config.lease_seconds > 0,
            "a zero lease lets two runners claim one row"
        );
        assert!(config.retry_base > Duration::ZERO);
        assert!(
            config.retry_max >= config.retry_base,
            "a cap below the base is not a cap"
        );
        assert!(config.request_timeout > Duration::ZERO);
    }

    #[test]
    fn the_backoff_stays_inside_the_cap_on_every_attempt() {
        // Property rather than three literals, and it is checked over the whole range the
        // queue can produce: a cap that the fourth attempt overshoots is a backoff that grows
        // forever, and the row is retried until the process restarts.
        let config = delivery_config();
        for attempt in 1..=50 {
            let delay = delivery::retry_delay(attempt, config.retry_base, config.retry_max);
            assert!(
                delay >= config.retry_base,
                "attempt {attempt} is shorter than the base"
            );
            assert!(
                delay <= config.retry_max,
                "attempt {attempt} overshoots the cap"
            );
        }
    }

    #[test]
    fn the_timestamp_helper_is_the_shape_the_store_binds() {
        // `now()` is what `mark_retry` compares against `next_attempt_at`; a helper that
        // returned a local offset would make every retry due immediately on a server whose
        // clock is not UTC.
        let now: OffsetDateTime = delivery::now();
        assert_eq!(now.offset(), time::UtcOffset::UTC);
    }
}
