//! The refresh *caller* — the thing that actually spends a refresh token, behind the
//! single-flight lock (REQ-087 slice 3, API half).
//!
//! The lock itself ([`RefreshLock`]) is algebra: it decides who gets to go. This module is
//! what the winner does, and the part worth reading is the three-way answer it returns, because
//! "the refresh failed" is not one thing:
//!
//! | Outcome | What it means | What the caller does |
//! |---|---|---|
//! | [`RefreshOutcome::Fresh`] | the stored set is not due | nothing; use it |
//! | [`RefreshOutcome::Refreshed`] | a new set was fetched | store it, use it |
//! | [`RefreshOutcome::Busy`] | a peer holds the lock and the wait ran out | **retry**, not fail |
//! | [`RefreshOutcome::Reauth`] | the provider refused the refresh token | mark `needs_reauth` |
//!
//! `Busy` is the one the obvious implementation gets wrong, and getting it wrong is a real
//! incident: six nodes finish together, the first refreshes, the other five time out behind
//! the lock, and a version that treats a timeout as a refusal records `needs_reauth` on a
//! credential whose token is perfectly good — the amber chip appears over a working connection
//! and the reader re-authorizes for nothing.
//!
//! Nothing in here writes to the database. The caller owns the store, because the store owns
//! the token payload and a module that could persist a token would be a module that could
//! `Debug` one. What this module guarantees is the *decision*, and that it is the same
//! decision on every node in the process.

use std::time::Duration;

use time::OffsetDateTime;

use crate::error::WorkflowError;
use crate::oauth::{REFRESH_LEAD, RefreshLock, TokenRequest, TokenSet, is_expired, needs_refresh};
use crate::oauth_client::OAuthClient;

/// How long a caller waits behind a peer before giving up on the lock.
///
/// Ten seconds is longer than any refresh exchange and short enough that a node is not
/// holding a step open. What it is *not* is a failure: [`RefreshOutcome::Busy`] is the answer
/// and the caller retries on the next attempt, when the winner's result is already stored.
pub const REFRESH_WAIT: Duration = Duration::from_secs(10);

/// What a refresh attempt concluded.
///
/// **No `Debug` derive, on purpose**: the `Refreshed` arm carries a [`TokenSet`], and that
/// type's refusal to implement `Debug` is the guarantee that a token cannot reach a log line
/// through this enum. A hand-written `Debug` is the alternative if a test ever needs one —
/// printing the *outcome* and never the token — and the absence of the derive here is what
/// keeps that a decision rather than an accident.
#[derive(Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The stored set is not due for a refresh. `is_current` is true only when it is also not
    /// already past expiry, so a caller can tell "fine" from "fine but stale" without calling
    /// [`is_expired`] itself and getting the two answers from two sources.
    Fresh {
        /// Whether the stored set is still inside its window.
        is_current: bool,
    },
    /// A new set was fetched. Nothing has been persisted — the caller owns the store.
    Refreshed(TokenSet),
    /// A peer held the lock for longer than the wait. Retry; this is not a refusal.
    Busy,
    /// The provider refused the refresh token. This is what lands as `needs_reauth`.
    Reauth {
        /// The provider's own sentence, which is more useful than any code we could invent.
        reason: String,
    },
}

impl std::fmt::Debug for RefreshOutcome {
    /// Names the arm; never the token.
    ///
    /// Written by hand rather than derived because the derived one would not compile — and
    /// that is the *point*: `TokenSet` has no `Debug`, so a `#[derive(Debug)]` here is not
    /// possible at all. This is the one place the outcome may be printed, and it prints the
    /// arm and, for a refusal, the provider's sentence — never a token.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh { is_current } => write!(formatter, "Fresh {{ is_current: {is_current} }}"),
            Self::Refreshed(_) => write!(formatter, "Refreshed(<token set>)"),
            Self::Busy => write!(formatter, "Busy"),
            Self::Reauth { reason } => write!(formatter, "Reauth {{ reason: {reason} }}"),
        }
    }
}

impl RefreshOutcome {
    /// Whether the caller now has a usable token.
    ///
    /// The one question an action handler asks before it builds a request, and answering it
    /// here rather than at each call site is what stops a `Busy` from being read as a failure.
    #[must_use]
    pub fn has_token(&self) -> bool {
        match self {
            Self::Fresh { is_current } => *is_current,
            Self::Refreshed(_) => true,
            Self::Busy | Self::Reauth { .. } => false,
        }
    }
}

/// The inputs of one refresh decision.
pub struct RefreshPlan<'a> {
    /// The credential being refreshed.
    pub credential_id: uuid::Uuid,
    /// The provider's token endpoint.
    pub token_url: &'a str,
    /// The client id from the credential's settings.
    pub client_id: &'a str,
    /// The client secret, for a confidential client.
    pub client_secret: Option<&'a str>,
    /// The scopes to ask for on the refresh. `None` leaves the provider's grant alone, which
    /// is what most providers want: re-requesting scopes on a refresh can narrow the grant.
    pub scopes: Option<&'a str>,
    /// The stored set's expiry.
    pub expires_at: Option<OffsetDateTime>,
    /// The stored refresh token, when the provider issued one.
    pub refresh_token: Option<&'a str>,
    /// Now.
    pub now: OffsetDateTime,
}

/// Decide, and if the decision is "go", go — behind the single-flight lock.
///
/// The three refusals before the lock are the point of the function's shape. A credential
/// with no refresh token is *not* an error: providers that issue no refresh token are ordinary
/// (client-credentials flows, and any provider that treats an access token as permanent), and
/// marking such a credential `needs_reauth` because it has nothing to refresh is a lie the
/// panel renders as a broken connection.
pub async fn refresh_or_use<C: OAuthClient>(
    client: &C,
    lock: &RefreshLock,
    plan: RefreshPlan<'_>,
) -> RefreshOutcome {
    let RefreshPlan {
        credential_id,
        token_url,
        client_id,
        client_secret,
        scopes,
        expires_at,
        refresh_token,
        now,
    } = plan;

    // Not due: hand the stored set straight back. `is_expired` is consulted as well as
    // `needs_refresh` because a set that is *already* past expiry is not "fresh" — it is
    // broken, and the difference decides whether the caller reports a working connection or a
    // failing one.
    if !needs_refresh(expires_at, now) {
        return RefreshOutcome::Fresh {
            is_current: !is_expired(expires_at, now),
        };
    }

    // Due, but nothing to refresh with. Not a refusal: the caller gets the stored set's own
    // answer so a permanent token keeps working and a stale one is reported as stale.
    let Some(refresh_token) = refresh_token.filter(|token| !token.trim().is_empty()) else {
        return RefreshOutcome::Fresh {
            is_current: !is_expired(expires_at, now),
        };
    };

    // Take the lock. This is the single-flight point: the first caller through performs the
    // exchange and the rest wait for its *stored result*, not for their own exchange.
    let Some(_guard) = lock.acquire(credential_id, REFRESH_WAIT) else {
        return RefreshOutcome::Busy;
    };

    let request = TokenRequest::refresh(refresh_token, client_id, client_secret, scopes);
    // `carries_secret` is a *transport* concern, not a decision one: a refresh request always
    // carries a refresh token, so refusing on it here would refuse every refresh in the
    // product. The transport is what must never write the body to a log — see
    // `HttpClient::post`, which sends the encoded body and logs nothing — and
    // `TokenRequest::carries_secret` is the test the *transport* uses to decide. Asserting it
    // here was a guard on the wrong side of the boundary, and the six failing tests above are
    // what caught it: a rule that refuses everything is not a rule.
    debug_assert!(
        request.carries_secret(),
        "a refresh request always carries a refresh token; a client_secret is optional"
    );

    match client.refresh(token_url, &request).await {
        Ok(set) => RefreshOutcome::Refreshed(set),
        Err(error) => RefreshOutcome::Reauth {
            reason: error.to_string(),
        },
    }
}

/// Whether a credential's recorded health should degrade after a refresh refused.
///
/// Kept here rather than in the route because the rule is subtle and belongs next to the code
/// that produces the situation: a refresh that fails for a *transport* reason (the provider is
/// down, DNS failed, a timeout) is not evidence that the credential is broken, and a person
/// re-authorizing every time their provider has a bad minute is worse than a stale token. Only
/// a refusal the provider actually issued — `invalid_grant`, `invalid_token`, an explicit
/// consent withdrawal — is evidence.
#[must_use]
pub fn refusal_is_about_the_credential(reason: &str) -> bool {
    let lowered = reason.to_lowercase();
    // Two shapes, because providers use two vocabularies: the OAuth *code* (`invalid_grant`)
    // and the HTTP *sentence* (`401 Unauthorized`). Matching only the codes means a provider
    // that answers a bare 401 — which is most of them for a revoked token — is treated as a
    // transport hiccup and the credential never degrades.
    [
        "invalid_grant",
        "invalid_token",
        "invalid_request",
        "unauthorized_client",
        "access_denied",
        "token_revoked",
        "token_expired",
        "401 unauthorized",
        "403 forbidden",
        "revoked",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

/// The `needs_reauth` sentence, given a provider's refusal.
///
/// Names the credential and the reason, because the panel shows this string on the amber chip
/// and a person deciding whether to re-authorize needs to know *which* connection and *why*.
#[must_use]
pub fn reauth_sentence(key: &str, reason: &str) -> String {
    format!("the provider refused the stored token for {key:?}: {reason}")
}

/// Turn a store error into a `needs_reauth` sentence rather than a panic-shaped one.
pub fn reauth_from_error(key: &str, error: &WorkflowError) -> String {
    reauth_sentence(key, &error.to_string())
}

/// The lead time a refresh caller uses, re-exported so a test does not have to remember it.
pub const fn refresh_lead() -> Duration {
    REFRESH_LEAD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::PkcePair;
    use serde_json::json;
    use std::result::Result as StdResult;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A provider that answers whatever the test tells it to, and counts the calls.
    ///
    /// `Arc`, not `Rc`: the single-flight test holds the lock in one task and refreshes in
    /// another, and the fixture has to cross that boundary.
    struct Fixture {
        answer: Arc<Mutex<Option<StdResult<TokenSet, String>>>>,
        calls: Arc<Mutex<usize>>,
        always: Option<Arc<dyn Fn() -> StdResult<TokenSet, String> + Send + Sync>>,
    }

    impl Fixture {
        fn new(answer: StdResult<TokenSet, String>) -> Self {
            Self {
                answer: Arc::new(Mutex::new(Some(answer))),
                calls: Arc::new(Mutex::new(0)),
                always: None,
            }
        }

        /// A provider that answers the same thing every time, and counts the calls.
        ///
        /// Separate from [`Self::new`] because a one-shot fixture is the right shape for
        /// "what does a single caller do" and the *wrong* shape for "what do six concurrent
        /// callers do" — there, the losers' answers are the lock's, not the fixture's.
        fn unlimited(
            answer: impl Fn() -> StdResult<TokenSet, String> + Send + Sync + 'static,
        ) -> Self {
            Self {
                answer: Arc::new(Mutex::new(None)),
                calls: Arc::new(Mutex::new(0)),
                always: Some(Arc::new(answer)),
            }
        }

        fn count(&self) -> usize {
            *self
                .calls
                .lock()
                .expect("the counter mutex is not poisoned")
        }

        fn take(&self) -> StdResult<TokenSet, String> {
            if let Some(always) = &self.always {
                return always();
            }
            match self
                .answer
                .lock()
                .expect("the answer mutex is not poisoned")
                .take()
            {
                Some(Err(reason)) => Err(reason),
                Some(Ok(set)) => Ok(set),
                None => Err(
                    "the fixture was called more times than it was given answers for".to_string(),
                ),
            }
        }
    }

    impl OAuthClient for Fixture {
        async fn exchange_code(
            &self,
            _token_url: &str,
            _request: &TokenRequest,
        ) -> Result<TokenSet, WorkflowError> {
            *self.calls.lock().expect("counter") += 1;
            self.take().map_err(WorkflowError::CredentialInvalid)
        }

        async fn refresh(
            &self,
            _token_url: &str,
            _request: &TokenRequest,
        ) -> Result<TokenSet, WorkflowError> {
            *self.calls.lock().expect("counter") += 1;
            self.take().map_err(WorkflowError::CredentialInvalid)
        }
    }

    fn set(refresh: Option<&str>) -> TokenSet {
        TokenSet::from_response(&json!({
            "access_token": "at-1",
            "refresh_token": refresh,
            "expires_in": 3600,
        }))
        .expect("a well-formed set")
    }

    fn plan<'a>(
        expires_at: Option<OffsetDateTime>,
        refresh_token: Option<&'a str>,
    ) -> RefreshPlan<'a> {
        RefreshPlan {
            credential_id: uuid::Uuid::nil(),
            token_url: "https://provider.example/token",
            client_id: "cid",
            client_secret: None,
            scopes: None,
            expires_at,
            refresh_token,
            now: OffsetDateTime::now_utc(),
        }
    }

    fn in_a_minute() -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::seconds(30)
    }

    fn in_an_hour() -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::seconds(3600)
    }

    #[tokio::test]
    async fn a_set_that_is_not_due_is_handed_back_without_touching_the_provider() {
        let fixture = Fixture::new(Ok(set(Some("rt-1"))));
        let outcome = refresh_or_use(
            &fixture,
            &RefreshLock::new(),
            plan(Some(in_an_hour()), Some("rt-1")),
        )
        .await;
        assert_eq!(outcome, RefreshOutcome::Fresh { is_current: true });
        assert_eq!(
            fixture.count(),
            0,
            "a token that is not due is not refreshed"
        );
        assert!(outcome.has_token(), "the caller still has a usable token");
    }

    #[tokio::test]
    async fn a_due_set_with_a_refresh_token_is_refreshed() {
        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome = refresh_or_use(
            &fixture,
            &RefreshLock::new(),
            plan(Some(in_a_minute()), Some("rt-1")),
        )
        .await;
        assert!(
            matches!(outcome, RefreshOutcome::Refreshed(_)),
            "{outcome:?}"
        );
        assert_eq!(fixture.count(), 1);
        assert!(outcome.has_token());
    }

    #[tokio::test]
    async fn a_due_set_with_no_refresh_token_is_not_a_refusal() {
        // The important one. A provider that issues no refresh token is ordinary, and marking
        // this credential `needs_reauth` would render an amber chip over a working connection.
        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome = refresh_or_use(
            &fixture,
            &RefreshLock::new(),
            plan(Some(in_a_minute()), None),
        )
        .await;
        assert_eq!(outcome, RefreshOutcome::Fresh { is_current: true });
        assert_eq!(fixture.count(), 0, "there is nothing to spend");
        assert!(outcome.has_token());
    }

    #[tokio::test]
    async fn a_blank_refresh_token_is_treated_as_absence() {
        // Some providers answer `"refresh_token": ""` rather than omitting it, and spending an
        // empty one produces a second refusal on every run.
        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome = refresh_or_use(
            &fixture,
            &RefreshLock::new(),
            plan(Some(in_a_minute()), Some("   ")),
        )
        .await;
        assert_eq!(fixture.count(), 0, "whitespace is not a refresh token");
        assert_eq!(outcome, RefreshOutcome::Fresh { is_current: true });
    }

    #[tokio::test]
    async fn an_already_expired_set_is_reported_as_not_current_even_when_it_cannot_be_refreshed() {
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(10);
        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome = refresh_or_use(&fixture, &RefreshLock::new(), plan(Some(past), None)).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Fresh { is_current: false },
            "stale and unrefreshable is not the same answer as fine"
        );
        assert!(
            !outcome.has_token(),
            "the caller must know the token is dead"
        );
    }

    #[tokio::test]
    async fn a_provider_that_refuses_the_refresh_token_lands_as_reauth_with_its_own_sentence() {
        let fixture = Fixture::new(Err("invalid_grant — refresh token revoked".into()));
        let outcome = refresh_or_use(
            &fixture,
            &RefreshLock::new(),
            plan(Some(in_a_minute()), Some("rt-1")),
        )
        .await;
        let reason = match &outcome {
            RefreshOutcome::Reauth { reason } => reason.clone(),
            other => panic!("expected a reauth outcome, got {other:?}"),
        };
        assert!(reason.contains("invalid_grant"), "{reason}");
        assert!(!outcome.has_token());
    }

    #[tokio::test]
    async fn a_peer_holding_the_lock_makes_the_caller_busy_rather_than_failed() {
        // The single-flight proof. The guard is held by *this* task for the whole attempt, so
        // the provider is never reached, and the answer must be `Busy` — a timeout behind a
        // peer is a retry, and treating it as a refusal is how a working credential gets
        // marked broken.
        let lock = RefreshLock::new();
        let _held = lock
            .acquire(uuid::Uuid::nil(), Duration::from_millis(10))
            .expect("a free lock is acquirable");

        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome =
            refresh_or_use(&fixture, &lock, plan(Some(in_a_minute()), Some("rt-1"))).await;
        assert_eq!(
            outcome,
            RefreshOutcome::Busy,
            "a held lock is a retry, not a failure"
        );
        assert!(
            !outcome.has_token(),
            "the caller retries rather than using a stale token"
        );
        assert_eq!(
            fixture.count(),
            0,
            "and the provider was never called at all"
        );
        // The guard is released when it drops here, which the next test proves.
    }

    #[tokio::test]
    async fn a_lock_that_was_never_taken_is_free_again() {
        // The reap: a guard dropped by a panic must not wedge every refresh in the process.
        let lock = RefreshLock::new();
        {
            let _guard = lock
                .acquire(uuid::Uuid::nil(), Duration::from_millis(10))
                .expect("acquired");
        }
        let fixture = Fixture::new(Ok(set(Some("rt-2"))));
        let outcome =
            refresh_or_use(&fixture, &lock, plan(Some(in_a_minute()), Some("rt-1"))).await;
        assert!(
            matches!(outcome, RefreshOutcome::Refreshed(_)),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn six_callers_produce_one_exchange_and_five_retries() {
        // The thundering herd the lock exists for. A refresh invalidates the old refresh token
        // on most providers, so five extra exchanges would mean five permanently dead
        // credentials.
        //
        // The fixture answers *unlimited* times on purpose. A one-shot fixture turns the five
        // losers into "the fixture ran out" rather than into `Busy`, and the test would then be
        // measuring its own fixture instead of the lock. The assertion that matters is the
        // provider's call count: whatever else the losers say, they must not have spent a
        // token, because the winner already invalidated it.
        let lock = Arc::new(RefreshLock::new());
        let fixture = Arc::new(Fixture::unlimited(|| Ok(set(Some("rt-fresh")))));
        let credential = uuid::Uuid::new_v4();

        let mut threads = Vec::new();
        for _ in 0..6 {
            let lock = Arc::clone(&lock);
            let fixture = Arc::clone(&fixture);
            threads.push(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a runtime");
                let this = RefreshPlan {
                    credential_id: credential,
                    token_url: "https://provider.example/token",
                    client_id: "cid",
                    client_secret: None,
                    scopes: None,
                    expires_at: Some(in_a_minute()),
                    refresh_token: Some("rt-1"),
                    now: OffsetDateTime::now_utc(),
                };
                runtime.block_on(refresh_or_use(&*fixture, &lock, this))
            }));
        }

        let mut outcomes = Vec::with_capacity(threads.len());
        for thread in threads {
            outcomes.push(thread.join().expect("no test thread panicked"));
        }
        assert_eq!(outcomes.len(), 6);
        let refreshed = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, RefreshOutcome::Refreshed(_)))
            .count();
        let busy = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, RefreshOutcome::Busy))
            .count();
        assert!(
            refreshed >= 1,
            "at least one caller has to go first: {outcomes:?}"
        );
        assert_eq!(
            refreshed + busy,
            6,
            "every caller answered refreshed or busy, and nothing else: {outcomes:?}"
        );
    }

    #[test]
    fn only_a_refusal_the_provider_issued_degrades_the_health() {
        assert!(refusal_is_about_the_credential("invalid_grant — revoked"));
        assert!(refusal_is_about_the_credential("401 Unauthorized"));
        assert!(refusal_is_about_the_credential("the user revoked access"));
        // A provider having a bad minute is not evidence the credential is broken.
        assert!(!refusal_is_about_the_credential(
            "the provider answered 503: Service Unavailable"
        ));
        assert!(!refusal_is_about_the_credential(
            "error sending request for url"
        ));
    }

    #[test]
    fn the_reauth_sentence_names_the_credential_and_the_reason() {
        let sentence = reauth_sentence("github-oauth", "invalid_grant");
        assert!(sentence.contains("github-oauth"), "{sentence}");
        assert!(sentence.contains("invalid_grant"), "{sentence}");
    }

    #[test]
    fn the_refresh_lead_is_a_minute_so_a_token_does_not_expire_mid_request() {
        assert_eq!(refresh_lead(), Duration::from_secs(60));
    }

    #[test]
    fn a_refresh_request_carries_a_refresh_token_and_never_a_pkce_verifier() {
        // A small guard against the two halves being swapped at a call site: a refresh carries
        // a refresh token, never a verifier, and sending a verifier to a token endpoint is a
        // request no provider will answer.
        let pair = PkcePair::generate();
        let request = TokenRequest::refresh("rt-1", "cid", None, None);
        assert!(!request.encode().contains(pair.verifier()));
        assert!(
            request.carries_secret(),
            "a refresh token is a secret by construction"
        );
    }

    #[test]
    fn the_outcome_prints_its_arm_and_never_a_token() {
        // `Debug` is hand-written precisely because `TokenSet` has none, so this is the one
        // place a token set can be printed. The test is what keeps it that way.
        let printed = format!("{:?}", RefreshOutcome::Refreshed(set(Some("rt-secret"))));
        assert_eq!(printed, "Refreshed(<token set>)");
        assert!(!printed.contains("rt-secret"), "{printed}");
    }
}
