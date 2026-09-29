//! The OAuth flow's persistence: mint, claim, settle (REQ-087 slice 3).
//!
//! The load-bearing part of this file is [`claim_flow`], and it is load-bearing for a reason
//! that only shows up under load. A `state` is single-use: the callback must be able to spend
//! it exactly once, no matter how many tabs, retries or a determined person send the same
//! callback. The naive version — `select` then `update` — is two round trips, and the window
//! between them is a race a double-clicked "Allow" button wins. So the claim is
//!
//! ```text
//! update workflow_oauth_flows set status = 'completing', completed_at = now()
//!  where organization_id = $1 and state_hash = $2 and status = 'pending'
//!  returning …
//! ```
//!
//! one statement whose `where` carries the precondition. Postgres takes a row lock for it, so
//! the second claimant's `update` matches zero rows *after* the first commits and it gets
//! `None` — which is [`FlowClaim::AlreadySpent`], the correct answer. There is no `select`
//! before it and no interval in which two callers both believe they won.
//!
//! The `code_verifier` is stored in `code_verifier_enc`, which is an *envelope* from
//! [`SecretBox`](crate::oauth::seal_local) — not a plaintext column. The verifier is half of
//! PKCE, and a half-PKCE secret in a readable column is a secret with a misleading name.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::oauth::{PkcePair, STATE_TTL, StateRejection};

/// Columns of `workflow_oauth_flows` for one `select`, in [`OAuthFlow`] order.
pub const FLOW_COLUMNS: &str = "id, organization_id, credential_id, credential_type, \
     state_hash, code_challenge, code_verifier_enc, authorize_url, redirect_uri, scopes, \
     status, subject, failure_code, failure_detail, started_at, expires_at, completed_at";

/// The states a flow can be in.
///
/// `completing` is the transient one, and it exists so the claim is atomic: a row that is
/// mid-exchange is visibly *not* `pending`, so a replay cannot re-claim it. It is never
/// something the API reports as a state a person can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowStatus {
    /// Minted, waiting for the provider to send a person back.
    Pending,
    /// A callback claimed it and is exchanging the code.
    Completing,
    /// A token set was stored.
    Completed,
    /// The provider or the exchange refused.
    Failed,
    /// The window closed before anybody came back.
    Expired,
}

impl FlowStatus {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completing => "completing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    /// Read one back, tolerating a value a future release wrote.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "completing" => Some(Self::Completing),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// One OAuth flow in flight (or finished).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OAuthFlow {
    /// Flow id.
    pub id: Uuid,
    /// Organization that owns the credential.
    pub organization_id: Uuid,
    /// The credential being connected.
    pub credential_id: Uuid,
    /// Its credential type, for the panel's own record.
    pub credential_type: String,
    /// `SHA-256` of the signed state, hex.
    pub state_hash: String,
    /// The PKCE challenge, when the type uses one.
    pub code_challenge: Option<String>,
    /// The PKCE verifier, sealed. Never returned by any read the API makes.
    pub code_verifier_enc: Option<String>,
    /// Where the person was sent.
    pub authorize_url: String,
    /// Where the provider sends them back.
    pub redirect_uri: String,
    /// The scopes asked for.
    pub scopes: Option<String>,
    /// Current state.
    pub status: String,
    /// Who it connected as, once it did.
    pub subject: Option<String>,
    /// The refusal code, once it failed.
    pub failure_code: Option<String>,
    /// The refusal, in a sentence.
    pub failure_detail: Option<String>,
    /// When it was minted.
    pub started_at: OffsetDateTime,
    /// When it stops being claimable.
    pub expires_at: OffsetDateTime,
    /// When it settled.
    pub completed_at: Option<OffsetDateTime>,
}

impl OAuthFlow {
    /// The parsed status; `None` for a value this release does not know.
    #[must_use]
    pub fn status(&self) -> Option<FlowStatus> {
        FlowStatus::parse(&self.status)
    }
}

/// A flow row to be written.
#[derive(Debug, Clone)]
pub struct NewOAuthFlow {
    /// Organization.
    pub organization_id: Uuid,
    /// Credential being connected.
    pub credential_id: Uuid,
    /// Its credential type.
    pub credential_type: String,
    /// `SHA-256` of the signed state, hex.
    pub state_hash: String,
    /// The PKCE challenge.
    pub code_challenge: Option<String>,
    /// The sealed PKCE verifier.
    pub code_verifier_enc: Option<String>,
    /// Where the person was sent.
    pub authorize_url: String,
    /// Where the provider sends them back.
    pub redirect_uri: String,
    /// The scopes asked for.
    pub scopes: Option<String>,
    /// When it stops being claimable.
    pub expires_at: OffsetDateTime,
}

/// Mint a flow.
///
/// Superseding is the point: starting a second connection for the same credential cancels the
/// first, because leaving both `pending` means two `state` values are both live and the second
/// one is a bearer token for a callback anybody who saw the first could forge a timing for.
pub async fn insert_flow(pool: &PgPool, new: NewOAuthFlow) -> Result<OAuthFlow> {
    // Cancel anything already pending for this credential, in its own statement, so the
    // insert cannot be blocked by a row nobody is going to claim.
    sqlx::query(
        "update workflow_oauth_flows set status = 'expired', completed_at = now(), \
           failure_code = 'credential_oauth_superseded', \
           failure_detail = 'another connection was started for this credential' \
         where organization_id = $1 and credential_id = $2 and status = 'pending'",
    )
    .bind(new.organization_id)
    .bind(new.credential_id)
    .execute(pool)
    .await?;

    let sql = format!(
        "insert into workflow_oauth_flows \
           (organization_id, credential_id, credential_type, state_hash, code_challenge, \
            code_verifier_enc, authorize_url, redirect_uri, scopes, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning {FLOW_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(new.organization_id)
        .bind(new.credential_id)
        .bind(new.credential_type)
        .bind(new.state_hash)
        .bind(new.code_challenge)
        .bind(new.code_verifier_enc)
        .bind(new.authorize_url)
        .bind(new.redirect_uri)
        .bind(new.scopes)
        .bind(new.expires_at)
        .fetch_one(pool)
        .await?)
}

/// The outcome of trying to spend a `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowClaim {
    /// This caller won the flow, and holds the verifier.
    Claimed(OAuthFlow),
    /// Somebody already spent it.
    AlreadySpent,
    /// No such state in this organization.
    Unknown,
    /// It exists, but its window closed.
    Expired,
}

impl FlowClaim {
    /// Map onto the rejection the API reports, so the store's four answers and the flow's
    /// four refusals cannot drift apart.
    ///
    /// `AlreadySpent` and `Unknown` both mean "not yours to spend", but they are told apart:
    /// a replay of a real state deserves `credential_oauth_state` with a *different* sentence
    /// from a state that never existed, because the first is an attack and the second is a
    /// stale bookmark.
    pub fn rejection(&self) -> Option<StateRejection> {
        match self {
            Self::Claimed(_) => None,
            Self::AlreadySpent => Some(StateRejection::Used),
            Self::Unknown => Some(StateRejection::Unrecognised),
            Self::Expired => Some(StateRejection::Expired),
        }
    }
}

/// Spend a `state`, exactly once.
///
/// The `where` carries `status = 'pending'`, and that clause *is* the single-use guarantee:
/// the second caller's `update` finds no `pending` row and returns nothing.
pub async fn claim_flow(
    pool: &PgPool,
    organization_id: Uuid,
    state_hash: &str,
    now: OffsetDateTime,
) -> Result<FlowClaim> {
    let sql = format!(
        "update workflow_oauth_flows set status = 'completing', completed_at = now() \
          where organization_id = $1 and state_hash = $2 and status = 'pending' \
            and expires_at > $3 \
          returning {FLOW_COLUMNS}"
    );
    if let Some(flow) = sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(state_hash)
        .bind(now)
        .fetch_optional(pool)
        .await?
    {
        return Ok(FlowClaim::Claimed(flow));
    }

    // Nothing claimed. Distinguish "gone" from "expired" so the refusal can be a sentence.
    let sql = format!(
        "select {FLOW_COLUMNS} from workflow_oauth_flows \
                       where organization_id = $1 and state_hash = $2"
    );
    let existing = sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(state_hash)
        .fetch_optional(pool)
        .await?;
    Ok(match existing {
        None => FlowClaim::Unknown,
        Some(flow) if flow.expires_at <= now => FlowClaim::Expired,
        Some(_) => FlowClaim::AlreadySpent,
    })
}

/// Seal a flow: the token set was stored.
pub async fn complete_flow(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    subject: Option<&str>,
) -> Result<Option<OAuthFlow>> {
    let sql = format!(
        "update workflow_oauth_flows set status = 'completed', subject = $3, \
           completed_at = now(), failure_code = null, failure_detail = null \
         where organization_id = $1 and id = $2 returning {FLOW_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(subject)
        .fetch_optional(pool)
        .await?)
}

/// Fail a flow, with the reason the panel shows.
pub async fn fail_flow(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    code: &str,
    detail: &str,
) -> Result<Option<OAuthFlow>> {
    let sql = format!(
        "update workflow_oauth_flows set status = 'failed', failure_code = $3, \
           failure_detail = $4, completed_at = now() \
         where organization_id = $1 and id = $2 returning {FLOW_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(code)
        .bind(detail)
        .fetch_optional(pool)
        .await?)
}

/// Put a claimed flow back to `pending` when the exchange could not be attempted.
///
/// Without this a transient provider timeout would leave the flow stuck in `completing`
/// forever: the `state` is spent, so a person who reloads the consent screen has nothing
/// left to return with, and the credential is un-connectable until somebody creates a new one.
/// The window is *not* extended — the flow can still expire on its own schedule.
pub async fn release_flow(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<OAuthFlow>> {
    let sql = format!(
        "update workflow_oauth_flows set status = 'pending', completed_at = null \
          where organization_id = $1 and id = $2 and status = 'completing' \
          returning {FLOW_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// The newest flow for a credential, which is what the detail screen shows.
pub async fn latest_flow(
    pool: &PgPool,
    organization_id: Uuid,
    credential_id: Uuid,
) -> Result<Option<OAuthFlow>> {
    let sql = format!(
        "select {FLOW_COLUMNS} from workflow_oauth_flows \
          where organization_id = $1 and credential_id = $2 \
          order by started_at desc limit 1"
    );
    Ok(sqlx::query_as::<_, OAuthFlow>(&sql)
        .bind(organization_id)
        .bind(credential_id)
        .fetch_optional(pool)
        .await?)
}

/// Sweep flows whose window closed without a callback.
///
/// A no-op for correctness — [`claim_flow`] already refuses an expired state — and it exists
/// so the table does not accumulate a row per abandoned consent screen forever, and so a panel
/// can say "expired" rather than "pending" about a flow that is actually long dead.
pub async fn expire_stale(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<u64> {
    let result = sqlx::query(
        "update workflow_oauth_flows set status = 'expired', completed_at = now(), \
           failure_code = 'credential_oauth_state', \
           failure_detail = 'the authorization request expired without a callback' \
         where organization_id = $1 and status = 'pending' and expires_at <= $2",
    )
    .bind(organization_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Drop every flow belonging to a credential, for the credential's own delete.
pub async fn delete_flows_for(
    pool: &PgPool,
    organization_id: Uuid,
    credential_id: Uuid,
) -> Result<u64> {
    let result = sqlx::query(
        "delete from workflow_oauth_flows where organization_id = $1 and credential_id = $2",
    )
    .bind(organization_id)
    .bind(credential_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Recover the PKCE pair a claimed flow is holding.
///
/// The verifier is sealed on the way in and opened only here, on the one code path that is
/// about to spend a code with it. Returning a pair the caller has no business having would
/// defeat the point of sealing it.
pub fn open_pkce(
    flow: &OAuthFlow,
    secret_box: &crate::oauth::LocalBox,
) -> Result<Option<PkcePair>> {
    let (Some(encoded), Some(challenge)) = (
        flow.code_verifier_enc.as_deref(),
        flow.code_challenge.as_deref(),
    ) else {
        return Ok(None);
    };
    let verifier = secret_box.open(encoded).map_err(|_| {
        WorkflowError::CredentialInvalid(format!(
            "the PKCE verifier for this flow could not be read back; start the connection again \
             (credential {})",
            flow.credential_id
        ))
    })?;
    let pair = PkcePair::derive(&verifier);
    if pair.challenge != challenge {
        return Err(WorkflowError::CredentialInvalid(format!(
            "the PKCE verifier for this flow does not match its challenge; start the connection \
             again (credential {})",
            flow.credential_id
        )));
    }
    Ok(Some(pair))
}

/// How long a flow stays claimable from now, as a `OffsetDateTime`.
#[must_use]
pub fn expiry_from(now: OffsetDateTime) -> OffsetDateTime {
    now + STATE_TTL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_round_trips_through_its_stored_spelling() {
        for status in [
            FlowStatus::Pending,
            FlowStatus::Completing,
            FlowStatus::Completed,
            FlowStatus::Failed,
            FlowStatus::Expired,
        ] {
            assert_eq!(FlowStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(
            FlowStatus::parse("completing"),
            Some(FlowStatus::Completing)
        );
    }

    #[test]
    fn a_status_this_release_does_not_know_is_none_rather_than_a_panic() {
        // A release that writes a sixth state must not make this one unable to read the row.
        assert_eq!(FlowStatus::parse("revoked"), None);
        assert_eq!(FlowStatus::parse(""), None);
    }

    #[test]
    fn a_claimed_flow_has_no_refusal_and_every_other_outcome_names_one() {
        // Winning is the only outcome that is not a refusal, and it must say so by having
        // nothing to report — a caller that maps a refusal onto an HTTP status must not have
        // to special-case the success.
        assert!(FlowClaim::Claimed(flow_stub()).rejection().is_none());

        let refusals = [
            FlowClaim::AlreadySpent,
            FlowClaim::Unknown,
            FlowClaim::Expired,
        ]
        .map(|claim| claim.rejection().expect("a lost claim always has a story"));
        assert_eq!(refusals[0], StateRejection::Used);
        assert_eq!(refusals[1], StateRejection::Unrecognised);
        assert_eq!(refusals[2], StateRejection::Expired);
        // A replay and a state that never existed are different events and get different words.
        assert_ne!(refusals[0], refusals[1]);
    }

    #[test]
    fn a_claim_expiry_is_the_states_own_window() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(expiry_from(now) - now, STATE_TTL);
    }

    fn flow_stub() -> OAuthFlow {
        OAuthFlow {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            credential_id: Uuid::new_v4(),
            credential_type: "oauth2".into(),
            state_hash: "hash".into(),
            code_challenge: Some("challenge".into()),
            code_verifier_enc: Some("sealed".into()),
            authorize_url: "https://auth.example.com/authorize".into(),
            redirect_uri: "https://app.test/callback".into(),
            scopes: Some("read".into()),
            status: "completing".into(),
            subject: None,
            failure_code: None,
            failure_detail: None,
            started_at: OffsetDateTime::now_utc(),
            expires_at: OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    #[test]
    fn a_flow_with_no_pkce_pair_opens_to_none_rather_than_failing() {
        let box_key = crate::oauth::LocalBox::from_key_material(b"a-test-key-for-the-pkce-32bytes");
        let mut flow = flow_stub();
        flow.code_challenge = None;
        flow.code_verifier_enc = None;
        assert_eq!(open_pkce(&flow, &box_key).unwrap(), None);
    }

    #[test]
    fn a_sealed_verifier_opens_back_to_the_same_challenge() {
        let box_key = crate::oauth::LocalBox::from_key_material(b"a-test-key-for-the-pkce-32bytes");
        let pair = PkcePair::generate();
        let mut flow = flow_stub();
        flow.code_challenge = Some(pair.challenge.clone());
        flow.code_verifier_enc = Some(crate::oauth::seal_local(&box_key, pair.verifier()));
        let opened = open_pkce(&flow, &box_key)
            .unwrap()
            .expect("a pkce flow has a pair");
        assert_eq!(opened.challenge, pair.challenge);
        assert_eq!(opened.verifier(), pair.verifier());
    }

    #[test]
    fn a_verifier_that_does_not_match_its_challenge_is_refused_not_silently_used() {
        // The whole point of sealing: a row edited in the database must not produce a token
        // request that a provider accepts. Refusing here is what stops that.
        let box_key = crate::oauth::LocalBox::from_key_material(b"a-test-key-for-the-pkce-32bytes");
        let mut flow = flow_stub();
        let other = PkcePair::generate();
        flow.code_challenge = Some("a-challenge-nobody-derived".into());
        flow.code_verifier_enc = Some(crate::oauth::seal_local(&box_key, other.verifier()));
        let error = open_pkce(&flow, &box_key).unwrap_err();
        assert!(
            error.to_string().contains("does not match its challenge"),
            "the message must say which half is wrong: {error}"
        );
    }

    #[test]
    fn a_sealed_verifier_written_with_another_key_is_refused() {
        let writer = crate::oauth::LocalBox::from_key_material(b"one-installation-key-32-bytes!!");
        let reader = crate::oauth::LocalBox::from_key_material(b"another-installation-key-32b!!");
        let pair = PkcePair::generate();
        let mut flow = flow_stub();
        flow.code_challenge = Some(pair.challenge.clone());
        flow.code_verifier_enc = Some(crate::oauth::seal_local(&writer, pair.verifier()));
        let error = open_pkce(&flow, &reader).unwrap_err();
        assert!(error.to_string().contains("could not be read back"));
    }
}
