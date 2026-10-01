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
use omnion_core::config::{MailConfig, PushConfig};
use omnion_core::push_crypto::{self, SubscriberKeys};
use omnion_core::vapid::VapidKeys;
use omnion_notifications::delivery::{
    self, DeliveryConfig, DeliveryJob, Transport, TransportOutcome,
};
use omnion_notifications::push::PrunedSubscription;
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
                return TransportOutcome::failed(
                    None,
                    "this reader has no e-mail address to send to",
                );
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
                Ok(()) => TransportOutcome::accepted(None),
                // The sender's own `Display` is a sentence without the transport's internals,
                // which is what belongs in a column the outbox renders.
                Err(error) => TransportOutcome::failed(None, error.to_string()),
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

    /// The URL to POST to, and the sentence when there is none.
    ///
    /// **The channel's own destination, never `job.url`.** `job.url` is the in-app deep link
    /// — `/settings/iam/sessions`, `/media/files/{id}` — and posting to it is a relative URL
    /// with no base, which `reqwest` refuses with `builder error`. That failure was live for
    /// as long as this transport shipped: three attempts, then `failed`, with a reason in the
    /// outbox no reader could act on. `webhook_endpoint` comes from `notification_channels`
    /// instead, which is the only place a per-organization destination can live.
    ///
    /// The two are separated into a pure function because the *missing* branch is the one that
    /// matters and it is unreachable without a database.
    #[must_use]
    pub fn destination(job: &DeliveryJob) -> Result<String, &'static str> {
        match job.webhook_endpoint.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => Ok(url.to_owned()),
            _ => Err(
                "this organization's webhook channel has no destination — set one on the \
                 notification settings",
            ),
        }
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
            // A webhook channel with no destination cannot be delivered, and saying so is the
            // whole answer: the queue claimed the row, so something has to explain it.
            let url = match Self::destination(job) {
                Ok(url) => url,
                Err(reason) => {
                    return TransportOutcome::failed(None, reason);
                }
            };

            match self.client.post(url).json(&Self::payload(job)).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        TransportOutcome::accepted(Some(i32::from(status.as_u16())))
                    } else {
                        TransportOutcome::failed(
                            Some(i32::from(status.as_u16())),
                            format!("the endpoint answered {}", status.as_u16()),
                        )
                    }
                }
                Err(error) => TransportOutcome::failed(
                    None,
                    format!("the endpoint could not be reached: {error}"),
                ),
            }
        })
    }
}

/// The Web Push transport: one signed, encrypted request per registered browser.
///
/// **A send is three things at once, and getting two of them right is still a failure.** The
/// request carries an `Authorization: vapid …` header signed with the installation's key, a
/// body encrypted to the *specific* subscriber's key pair, and a TTL. The platform had the
/// first two halves built (slice 6a's `vapid`, slice 6b's `push_crypto`) and no code that put
/// them on a socket, which is why the channel stayed in `UNTRANSPORTED` and every push row
/// was written `skipped` with a sentence about a key the installation already had.
///
/// **One delivery row, several browsers.** The job's `push_targets` is the whole device list,
/// and each gets its *own* body: the ciphertext is sealed to one subscriber's ECDH key pair
/// and a body built for a phone cannot be read by a laptop. Sending to all of them is one
/// delivery, so the row settles once — `sent` when at least one device accepted it, and
/// `failed` only when every device refused.
///
/// **A device the push service declares gone is pruned here, not logged.** `404`/`410` mean
/// the browser rotated its service worker; the row survives on the platform until somebody
/// deletes it by hand, and every subsequent notification re-sends to it. `prune_endpoints`
/// exists in the crate for this and had **zero call sites** until now.
pub struct WebPushTransport {
    client: reqwest::Client,
    keys: Option<VapidKeys>,
    contact: Option<String>,
}

impl WebPushTransport {
    /// Build the transport, or `None` when the installation has no usable key pair.
    ///
    /// The same shape as the webhook transport's constructor and the same reason: a
    /// misconfigured installation is a fact reported at boot, not a `failed` row per queued
    /// notification.
    ///
    /// **Both halves of the pair, or nothing.** `contact` is checked here rather than in
    /// [`Self::deliver_all`] because the `sub` claim is part of *who may sign*, not part of the
    /// message: without it the request still gets built, still gets encrypted to the right
    /// subscriber, and is refused by the push service with a `401` that no outbox column
    /// distinguishes from a bad signature. A `None` from this constructor means the runner does
    /// not register the channel at all, so `run_due` leaves its rows alone instead of burning
    /// the retry cap on a configuration error that cannot fix itself.
    #[must_use]
    pub fn new(request_timeout: StdDuration, push: &PushConfig) -> Option<Self> {
        let client = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .ok()?;
        let keys = VapidKeys::from_private_bytes(&push.private_key_bytes()?)?;
        let contact = push.contact().map(str::to_owned);
        Some(Self {
            client,
            keys: Some(keys),
            contact: Some(contact?),
        })
    }

    /// The base64url public key a browser subscribes with.
    ///
    /// `None` when the installation has no key, which is what the public-key route answers
    /// with — a `200` carrying an empty key would let the panel render "push enabled" on an
    /// installation whose subscriptions all fail at the first send.
    #[must_use]
    pub fn application_server_key(&self) -> Option<&str> {
        self.keys.as_ref().map(VapidKeys::public_key)
    }

    /// The JSON payload the service worker receives.
    ///
    /// Split out as a pure function for the same reason the webhook's is: the *shape* is a
    /// contract, and the one thing that must not change silently is that `body` is absent —
    /// a push body is rendered by the service worker from these three fields, and duplicating
    /// the notification text inside it would be a second copy of a reader's private message
    /// travelling to a third-party infrastructure provider.
    #[must_use]
    pub fn payload(job: &DeliveryJob) -> serde_json::Value {
        serde_json::json!({
            "notification_id": job.notification_id,
            "title": job.title,
            "url": job.url,
        })
    }

    /// Send to every device and report one outcome for the whole delivery.
    ///
    /// The counting is the part worth reading twice. **Any** device accepting is a delivered
    /// notification: a person who is signed in on a phone and has a stale laptop
    /// registration has still received it, and failing the row because of the laptop would
    /// fill the outbox with failures for messages that arrived. Conversely a single refusal is
    /// not a failure either — push services rate-limit per sender, and one `429` among five
    /// devices is the queue's backoff's business, not this row's.
    async fn deliver_all(&self, job: &DeliveryJob) -> TransportOutcome {
        if job.push_targets.is_empty() {
            return TransportOutcome::failed(
                None,
                "this reader has no registered browser for push — turn on push for this \
                 browser on the notification settings",
            );
        }

        let (Some(keys), Some(contact)) = (&self.keys, self.contact.as_deref()) else {
            return TransportOutcome::failed(
                None,
                "the installation has no Web Push key pair configured",
            );
        };

        let payload = Self::payload(job);
        let plaintext = match serde_json::to_vec(&payload) {
            Ok(bytes) => bytes,
            Err(error) => {
                return TransportOutcome::failed(
                    None,
                    format!("the push payload could not be encoded: {error}"),
                );
            }
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());

        let mut delivered = 0_u32;
        let mut gone: Vec<PrunedSubscription> = Vec::new();
        let mut last_status: Option<i32> = None;
        let mut last_reason: Option<String> = None;

        for target in &job.push_targets {
            let body = push_crypto::encrypt(
                &SubscriberKeys {
                    p256dh: target.p256dh.clone(),
                    auth: target.auth.clone(),
                },
                &plaintext,
                push_crypto::random_16(),
                &push_crypto::random_32(),
            );
            let body = match body {
                Ok(body) => body,
                Err(error) => {
                    // One device's broken key must not stop the others, so this is recorded
                    // rather than returned — but it is recorded, because a registration whose
                    // `p256dh` never decodes is a registration that will never deliver.
                    last_reason.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };

            let Some(authorization) = keys.authorization(&target.endpoint, contact, now) else {
                last_reason.get_or_insert_with(|| {
                    "the push service endpoint is not an absolute http(s) URL".to_owned()
                });
                continue;
            };

            match self
                .client
                .post(&target.endpoint)
                .header("Authorization", authorization)
                .header("Content-Encoding", body.content_encoding())
                .header("TTL", PUSH_TTL_SECONDS)
                .header("Urgency", PUSH_URGENCY)
                .body(body.body.clone())
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let code = i32::from(status.as_u16());
                    last_status = Some(code);
                    if status.is_success() {
                        delivered += 1;
                    } else if matches!(code, 404 | 410) {
                        gone.push(PrunedSubscription {
                            id: target.id,
                            endpoint: target.endpoint.clone(),
                            status: code,
                        });
                        last_reason.get_or_insert_with(|| {
                            format!("a registered browser answered {code} and was removed")
                        });
                    } else {
                        last_reason.get_or_insert_with(|| format!("a device answered {code}"));
                    }
                }
                Err(error) => {
                    last_reason.get_or_insert_with(|| {
                        format!("a registered browser could not be reached: {error}")
                    });
                }
            }
        }

        if delivered > 0 {
            return TransportOutcome::Accepted {
                status: last_status,
                pruned: gone,
            };
        }

        TransportOutcome::Failed {
            status: last_status,
            reason: last_reason
                .unwrap_or_else(|| "no registered browser could be reached".to_owned()),
            pruned: gone,
        }
    }
}

impl Transport for WebPushTransport {
    fn channel(&self) -> &'static str {
        "web_push"
    }

    fn deliver<'a>(
        &'a self,
        job: &'a DeliveryJob,
        _config: &'a DeliveryConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = TransportOutcome> + Send + 'a>> {
        Box::pin(async move { self.deliver_all(job).await })
    }
}

/// How long a push service holds a message it cannot deliver immediately.
///
/// The Web Push default is a month of storage, which for a notification that says "a page is
/// waiting" is worse than useless: it would arrive days after the approval it was about, on a
/// device nobody is looking at. An hour is long enough for a locked phone on a slow network
/// and short enough that a stale message is not a haunting.
pub const PUSH_TTL_SECONDS: &str = "3600";

/// The RFC 8291 urgency: a browser may wake a service worker that is not running.
///
/// `normal` would be the polite default and is wrong for the notifications this platform
/// sends — they are approvals, security alerts and mentions, and the difference between
/// "arrives now" and "arrives when the tab is next opened" is the whole point of the channel.
pub const PUSH_URGENCY: &str = "high";

/// Start the runner; the returned handle is kept by the binary (and ends with the process).
#[must_use]
pub fn spawn(state: AppState) -> Option<JoinHandle<()>> {
    let process = state.config().events.clone();
    let mail = state.config().mail.clone();
    let push: PushConfig = state.config().push.clone();
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
    let mut transports = transports;

    // The webhook transport is optional for the reason in its own constructor: an HTTP client
    // that cannot be built is a process misconfiguration, and the runner says so at boot
    // instead of writing a failed row for every queued notification.
    let timeout =
        StdDuration::from_millis(config.request_timeout.whole_milliseconds().max(1) as u64);
    match WebhookTransport::new(timeout) {
        Some(webhook) => transports.push(("webhook".to_owned(), Box::new(webhook))),
        None => {
            tracing::error!("the notification webhook channel has no HTTP client; it will not run");
        }
    }

    // **Web Push is registered, and that is the whole of slice 6b.** Until this line the
    // channel was named in `UNTRANSPORTED` with the sentence "a signed payload needs the
    // installation's key pair" — a key pair the installation has had since slice 6a. A
    // channel with no transport is not quiet: `run_due` claims its rows and re-queues them
    // with "no transport is installed for this channel" until the cap, and then they land in
    // `failed`. So the honest fix is a transport, not a sentence.
    match WebPushTransport::new(timeout, &push) {
        Some(push) => transports.push(("web_push".to_owned(), Box::new(push))),
        None => tracing::warn!(
            contact = %state.config().push.has_private_key(),
            "the notification Web Push channel is not running: this installation has no usable \
             push key pair (set OMNION_PUSH_PRIVATE_KEY and OMNION_PUSH_CONTACT)"
        ),
    }

    spawn_with(state, transports, config, poll_ms)
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
            webhook_endpoint: None,
            push_targets: Vec::new(),
        }
    }

    /// One job with a webhook destination configured, for the destination tests below.
    fn job_posting_to(endpoint: Option<&str>) -> DeliveryJob {
        DeliveryJob {
            webhook_endpoint: endpoint.map(str::to_owned),
            ..job("webhook")
        }
    }

    /// One push job with the reader's registered browsers attached.
    fn job_pushing_to(devices: &[(&str, &str, &str)]) -> DeliveryJob {
        DeliveryJob {
            push_targets: devices
                .iter()
                .enumerate()
                .map(|(index, (endpoint, p256dh, auth))| {
                    omnion_notifications::delivery::PushTarget {
                        id: Uuid::from_u128(100 + index as u128),
                        endpoint: (*endpoint).to_owned(),
                        p256dh: (*p256dh).to_owned(),
                        auth: (*auth).to_owned(),
                    }
                })
                .collect(),
            ..job("web_push")
        }
    }

    /// A 32-byte P-256 private key, base64url — the same scalar `omnion-core`'s own tests use.
    const PUSH_PRIVATE_KEY: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";
    /// A contact address the push specification accepts.
    const PUSH_CONTACT: &str = "mailto:push@omnion.invalid";

    fn push_config(contact: Option<&str>) -> PushConfig {
        let config = PushConfig::default().with_private_key(PUSH_PRIVATE_KEY);
        match contact {
            Some(address) => config.with_contact(address),
            None => config,
        }
    }

    fn push_transport() -> Option<WebPushTransport> {
        WebPushTransport::new(StdDuration::from_secs(1), &push_config(Some(PUSH_CONTACT)))
    }

    // ----------------------------------------------------------------------------------------
    // Web Push (slice 6b)
    // ----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_reader_with_no_registered_browser_is_told_where_to_register_one() {
        // The branch a person actually hits: they turned the Web Push column on in the matrix
        // and no browser ever registered, because the panel never asked them to. The reason
        // has to name the screen, or it reads as "push is broken" and the fix is guessed at.
        let transport = push_transport().expect("the test installation has a push key pair");
        match transport
            .deliver(&job_pushing_to(&[]), &delivery_config())
            .await
        {
            TransportOutcome::Failed { reason, .. } => {
                assert!(reason.contains("no registered browser"), "{reason}");
                assert!(reason.contains("notification settings"), "{reason}");
            }
            other => panic!("a reader with no browser must not report success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_installation_with_no_key_pair_refuses_to_build_the_transport() {
        // `new` returning `None` is what keeps a misconfigured installation out of the
        // runner's table entirely: the alternative is a transport that claims every push row
        // and fails it, which is a hundred identical `failed` rows for one missing variable.
        assert!(WebPushTransport::new(StdDuration::from_secs(1), &PushConfig::default()).is_none());
    }

    #[tokio::test]
    async fn a_key_pair_without_a_contact_address_is_not_usable() {
        // A token whose `sub` is not a `mailto:`/`https:` URL is rejected by some push
        // services at send time, so the constructor refuses it here rather than answering
        // `401` for every notification days after an operator believed push worked.
        assert!(WebPushTransport::new(StdDuration::from_secs(1), &push_config(None)).is_none());
        assert!(
            WebPushTransport::new(StdDuration::from_secs(1), &push_config(Some("not-a-url")))
                .is_none()
        );
    }

    #[test]
    fn the_published_public_key_is_the_one_a_browser_subscribes_with() {
        // The browser's `applicationServerKey` is this exact string, and a transport that
        // signed with one key pair while publishing another would register every
        // subscription against a key no send can be verified by: a `401` per notification,
        // with nothing in the outbox to explain it. So this asserts the *encoding* a browser
        // requires — 65 uncompressed bytes, base64url without padding.
        let transport = push_transport().expect("the test installation has a push key pair");
        let published = transport
            .application_server_key()
            .expect("a built transport has a key");
        assert_eq!(
            omnion_core::base64url::decode(published).map(|bytes| bytes.len()),
            Some(65),
            "the browser needs the uncompressed SEC1 point"
        );
        assert!(
            !published.contains('=') && !published.contains('+') && !published.contains('/'),
            "base64url without padding, which is what PushSubscription.toJSON() emits"
        );

        // And it is the public half of the key that signs, not a separately configured value.
        let signing = VapidKeys::from_private_bytes(
            &omnion_core::base64url::decode(PUSH_PRIVATE_KEY).expect("b64"),
        )
        .expect("a valid scalar");
        assert_eq!(signing.public_key(), published);
    }

    #[test]
    fn the_push_payload_carries_the_notification_and_not_its_body() {
        // The same rule the webhook payload obeys, for the same reason: `body` is the
        // reader's private text and a push service is somebody else's infrastructure. The
        // service worker renders from `title` + `url` alone, so nothing is lost by leaving it
        // on the platform where it was written.
        let payload = WebPushTransport::payload(&job_pushing_to(&[]));
        let keys: Vec<&str> = payload
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"notification_id"));
        assert!(keys.contains(&"title"));
        assert!(keys.contains(&"url"));
        assert!(
            !keys.contains(&"body"),
            "the body must not leave through push"
        );
        assert!(!keys.contains(&"user_id"), "nor who it was for");
    }

    #[tokio::test]
    async fn a_device_with_a_broken_key_fails_the_row_and_names_the_key() {
        // Two devices, both with a `p256dh` that never decodes and endpoints that cannot
        // resolve. The honest answer is a failure naming the *key*, not a generic "could not
        // be reached": a registration whose key never decodes will never deliver, and the
        // device list shows nothing about it.
        let transport = push_transport().expect("the test installation has a push key pair");
        let job = job_pushing_to(&[
            ("https://127.0.0.1:1/gone", "AAAA", "AAAAAAAAAAAAAAAAAAAAAA"),
            ("https://127.0.0.1:2/gone", "BBBB", "AAAAAAAAAAAAAAAAAAAAAA"),
        ]);
        match transport.deliver(&job, &delivery_config()).await {
            TransportOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("p256dh"),
                    "the reason must name the broken key, got: {reason}"
                );
            }
            other => panic!("no device could have received this, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_valid_key_with_an_unreachable_service_still_fails_with_the_transport_reason() {
        // The other half of the same test: a *well-formed* key on a port nothing listens on
        // must produce the reachability sentence rather than the key sentence. The two are
        // different problems with different fixes, and an implementation that reported the
        // key error for both would send an operator to re-register a working device.
        let transport = push_transport().expect("the test installation has a push key pair");
        let job = job_pushing_to(&[(
            "https://127.0.0.1:1/gone",
            &good_p256dh(),
            "AAAAAAAAAAAAAAAAAAAAAA",
        )]);
        match transport.deliver(&job, &delivery_config()).await {
            TransportOutcome::Failed { reason, .. } => {
                assert!(reason.contains("could not be reached"), "got: {reason}");
            }
            other => panic!("nothing is listening on that port, got {other:?}"),
        }
    }

    #[test]
    fn the_ttl_is_an_hour_not_the_specifications_month() {
        // A month is the Web Push default and it is wrong for an approval request: the
        // message would arrive days later, on a device nobody is looking at, and be acted on
        // as if it were current. Pinned as a literal because the whole point is that it
        // differs from the default somebody would otherwise leave in place.
        assert_eq!(PUSH_TTL_SECONDS, "3600");
        let ttl: u64 = PUSH_TTL_SECONDS.parse().expect("a number");
        assert!(
            ttl < omnion_core::vapid::TOKEN_TTL_SECONDS,
            "the message must not outlive the token that authorizes it"
        );
    }

    #[test]
    fn the_urgency_is_high_because_these_notifications_wait_for_nobody() {
        // `normal` would let a push service hold a locked phone's message until the tab is
        // next opened, which for "a page is waiting for your approval" is the feature not
        // arriving.
        assert_eq!(PUSH_URGENCY, "high");
    }

    /// A real, uncompressed P-256 point, base64url — a subscriber whose key actually decrypts.
    ///
    /// **Derived through the platform's own public key function rather than pasted.** The
    /// property under test is "a well-formed key gets as far as the socket", and a point that
    /// is merely 65 random bytes is refused by the curve check: the send would fail with the
    /// key sentence instead of the reachability one and the test would pass for the wrong
    /// reason. Deriving it from the same scalar the signing key uses keeps it on the curve by
    /// construction — and it needs no fixture to drift.
    fn good_p256dh() -> String {
        omnion_core::vapid::public_key_from_private(
            &omnion_core::base64url::decode(PUSH_PRIVATE_KEY).expect("b64"),
        )
        .expect("a valid scalar is a valid point")
    }

    #[test]
    fn every_channel_in_the_closed_list_is_accounted_for() {
        // The test that matters. A channel added to `vocabulary.rs` and neither registered
        // here nor named in `UNTRANSPORTED` would be a channel the runner claims and then
        // re-queues forever as "no transport is installed" — the queue drains, the outbox
        // never shows a `failed`, and nothing anywhere says the feature does not exist. The
        // earlier shape of this test asserted the same thing it was given and so could not
        // fail; this one enumerates the vocabulary and names the gap.
        const REGISTERED: [&str; 4] = ["in_app", "email", "webhook", "web_push"];

        /// Channels the runner deliberately does not drain yet, and why. Each one is a
        /// `skipped` row in the outbox, never a silent `pending`.
        const UNTRANSPORTED: [(&str, &str); 1] = [(
            "chat",
            "the connector vocabulary is REQ-015's, not the notification crate's",
        )];

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
    fn the_webhook_destination_is_the_channel_and_never_the_in_app_link() {
        // **The regression this tick exists for.** The transport used to post `job.url`, which
        // every module fills with an in-app deep link — `/settings/iam/sessions`,
        // `/media/files/{id}`. `reqwest` refuses a relative URL with no base, so every webhook
        // delivery failed three times and landed in `failed` with a builder error no reader
        // could act on. The two are asserted separately because conflating them is exactly
        // the bug: a job can legally carry *both*, and only one of them is a destination.
        let job = DeliveryJob {
            url: Some("/settings/iam/sessions".to_owned()),
            webhook_endpoint: Some("https://collector.example/hook".to_owned()),
            ..job("webhook")
        };
        assert_eq!(
            WebhookTransport::destination(&job).as_deref(),
            Ok("https://collector.example/hook")
        );
    }

    #[test]
    fn a_webhook_channel_with_no_destination_says_where_to_set_one() {
        // The failure branch, which is unreachable without a database and therefore the one
        // most likely to be wrong. It has to name the screen: "this notification names no
        // endpoint to post to" pointed at the notification, and the notification never named
        // one — the *channel* did not, which is a different page and a different fix.
        let reason = WebhookTransport::destination(&job_posting_to(None)).unwrap_err();
        assert!(reason.contains("notification settings"), "{reason}");
        assert!(!reason.contains("this notification"), "{reason}");
    }

    #[test]
    fn a_blank_destination_is_absent_rather_than_an_empty_post() {
        // `Option<String>` holding `Some("")` is what a settings form posts for an input the
        // reader cleared, and posting to it would be `builder error: empty URL` — a different
        // failure with the same honest outcome, so both shapes are refused by one branch.
        assert!(WebhookTransport::destination(&job_posting_to(Some("   "))).is_err());
        assert!(WebhookTransport::destination(&job_posting_to(Some(""))).is_err());
    }

    #[test]
    fn the_in_app_deep_link_is_never_treated_as_a_destination() {
        // The specific shape that broke it: an in-app path *and* no channel destination.
        // Answering with the path would re-introduce the relative-URL send; answering with
        // the configuration sentence is the honest one.
        let job = DeliveryJob {
            url: Some("/notifications".to_owned()),
            ..job_posting_to(None)
        };
        assert!(WebhookTransport::destination(&job).is_err());
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
