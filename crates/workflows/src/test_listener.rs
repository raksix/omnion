//! *Listen for a real event* — a one-shot listener armed for one node of a graph
//! (REQ-004 slice 3, criterion 5).
//!
//! The criterion is one sentence and it is three claims, and the obvious implementation gets
//! two of them right and the third wrong in a way nothing on the screen would show:
//!
//! > *"Listen for a real event" captures a real bus event into the inspector within one
//! > matcher tick, and the listener expires after 15 minutes leaving no stray token.*
//!
//! **Why this is a new table and not a third column on `automation_test_events`.** REQ-003
//! already has a one-shot listener, and reusing it would have been the smaller change. It
//! cannot answer this criterion for three reasons, and each is a shape difference rather
//! than a missing field:
//!
//! * it is **rule-shaped**. The builder's toolbar is pressed with a *node* selected, and what
//!   the author is asking is "what would *this* node receive?" — a node sees the payload its
//!   upstream produced, which is not the bus event. A rule-level row answers a different
//!   question, and answering it here would show a payload the node never gets.
//! * it has **no expiry**. The existing row waits for ever, so an author who arms a
//!   listener, closes the laptop and returns tomorrow finds a day-old row that captures an
//!   event nobody is watching and reports it as a capture.
//! * it has **no token**. "Leaving no stray token" is half the criterion, and a row with no
//!   token cannot answer it — there is nothing to leave straying.
//!
//! ## The rule that is easy to get wrong
//!
//! `is_armed` is the only definition of "armed" in this module, and it takes **both**
//! `consumed_at is null` *and* `expires_at > now()`:
//!
//! * a listener past its expiry is **not armed**, even though nothing consumed it. This is
//!   the claim the obvious filter gets wrong: "armed" reads most naturally as "not yet
//!   consumed", and a matcher that filters on `consumed_at` alone fills a dead row and
//!   reports a capture for an event nobody was watching. A row that reports as *waiting* in
//!   the panel and is invisible to the matcher is worse than either — the author presses
//!   *Listen*, waits, and nothing ever arrives with no explanation.
//! * a listener that expired is **still returned** by the read, as `expired`. The panel has
//!   to be able to say "this listener has expired" rather than have the row vanish, because
//!   a vanishing row and a never-armed row are indistinguishable to the author and only one
//!   of them is a bug.
//!
//! The predicate lives in [`listener_is_live`] and is asserted from three directions, because
//! it is also written in SQL in three places (the partial unique index, the matcher's
//! `UPDATE … where` and the sweeper). Three hand-written copies of one rule is three
//! chances to disagree, so this module is the single reader of it and the SQL quotes it.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{Result, WorkflowError};

/// How long a listener waits, as REQ-004 names it: fifteen minutes.
///
/// A `const` rather than a caller argument because every writer must agree: a panel that
/// armed for 15 minutes and a matcher that gave up after 60 would report a capture against a
/// listener the panel had already called dead, and the disagreement is invisible in the
/// captured payload — it looks exactly like a slow event.
pub const LISTENER_TTL: Duration = Duration::minutes(15);

/// Characters of CSPRNG output behind a listener token.
///
/// 32 characters of base62 is ~190 bits, and the token is a *handle* rather than a
/// capability — it names an armed listener so a caller can read it back, and it grants
/// nothing the session guard did not already grant. The hash is what is stored.
pub const TOKEN_LENGTH: usize = 32;

/// Longest a captured payload may be, enforced by the table's constraint.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// The state a stored listener is in, as the panel reads it.
///
/// This is a *derived* state, never a stored one. A `status` column would need a sweeper to
/// keep it honest and would be wrong for every row between the expiry passing and the
/// sweeper running; deriving it means a row can never claim to be armed while the matcher
/// disagrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerStatus {
    /// Waiting for its event, and still inside the window.
    Armed,
    /// An event arrived and the payload is here.
    Captured,
    /// The window closed with nothing captured. The row is kept so the panel can say so.
    Expired,
}

impl ListenerStatus {
    /// Canonical name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Captured => "captured",
            Self::Expired => "expired",
        }
    }
}

/// One stored listener, as the panel reads it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TestListener {
    /// Row id.
    pub id: Uuid,
    /// The rule the listener belongs to.
    pub workflow_id: Uuid,
    /// The node it was armed for.
    pub node_id: String,
    /// The event name it is waiting for.
    pub event_name: String,
    /// The account that armed it.
    pub created_by: Option<Uuid>,
    /// When it was armed.
    pub created_at: OffsetDateTime,
    /// When the window closes.
    pub expires_at: OffsetDateTime,
    /// When an event filled it in.
    pub consumed_at: Option<OffsetDateTime>,
    /// The bus event that filled it.
    pub event_id: Option<i64>,
    /// The event's name, when it differs from the one the listener was armed for.
    pub event_name_captured: Option<String>,
    /// The captured payload.
    pub payload: Option<Value>,
}

/// The single definition of "armed", shared by the panel's read and the matcher's write.
///
/// `expired` is checked **first** and the order is not cosmetic: a row can be both consumed
/// and past its expiry, and the answer the panel wants is `captured` there (it has something
/// to show) while the matcher's `consumed_at is null` predicate keeps it out anyway. Checking
/// expiry first would relabel a captured listener as expired the moment its 15 minutes ran
/// out, and the payload — the whole point of arming it — would read as a failure to capture.
#[must_use]
pub fn listener_is_live(row: &TestListener, now: OffsetDateTime) -> bool {
    row.consumed_at.is_none() && row.expires_at > now
}

/// What a stored listener is, at a point in time.
///
/// Same argument as [`ListenerStatus`]: derived, never stored, so no row can claim a state
/// the matcher disagrees with.
#[must_use]
pub fn status_of(row: &TestListener, now: OffsetDateTime) -> ListenerStatus {
    if row.consumed_at.is_some() {
        return ListenerStatus::Captured;
    }
    if row.expires_at <= now {
        return ListenerStatus::Expired;
    }
    ListenerStatus::Armed
}

/// Mint a token and return it with its hash. The cleartext is returned **once**, here, and
/// never stored.
pub fn mint_token() -> (String, String) {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

    let mut rng = rand::thread_rng();
    let token: String = (0..TOKEN_LENGTH)
        .map(|_| char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())]))
        .collect();

    let hash = hash_token(&token);
    (token, hash)
}

/// SHA-256 of a token, lowercase hex — the only form that ever reaches the database.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Arm a listener for one node of a rule, replacing any listener already armed for that node.
///
/// The event name is stored rather than derived from the rule, so the armed row says out
/// loud what it waits for: a listener armed against a trigger the author edits a minute later
/// is a listener for the *old* name, and a row that has to be joined back to the rule to
/// answer "what is this waiting for?" changes its own answer under the author.
pub async fn arm_listener(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    node_id: &str,
    event_name: &str,
    created_by: Uuid,
    now: OffsetDateTime,
) -> Result<(TestListener, String)> {
    let (token, token_hash) = mint_token();

    // A second arm of the same node replaces the first, so the panel never shows two armed
    // rows for one node and the matcher only ever has one row to fill. Scoped to the *node*,
    // not the rule: a canvas author debugging two nodes at once is the reason this feature
    // exists.
    sqlx::query(
        "delete from workflow_test_listeners \
         where workflow_id = $1 and node_id = $2 and consumed_at is null",
    )
    .bind(workflow_id)
    .bind(node_id)
    .execute(pool)
    .await?;

    let row: TestListener = sqlx::query_as(
        "insert into workflow_test_listeners \
             (workflow_id, node_id, organization_id, event_name, token_hash, created_by, \
              created_at, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning id, workflow_id, node_id, event_name, created_by, created_at, expires_at, \
                   consumed_at, event_id, event_name_captured, payload",
    )
    .bind(workflow_id)
    .bind(node_id)
    .bind(organization_id)
    .bind(event_name)
    .bind(&token_hash)
    .bind(created_by)
    .bind(now)
    .bind(now + LISTENER_TTL)
    .fetch_one(pool)
    .await?;

    Ok((row, token))
}

/// A rule's listeners, newest first, whatever state they are in.
///
/// The read never filters by state, and that is the point: an expired listener that quietly
/// disappeared from the panel is indistinguishable from one that was never armed, and only
/// the second is a bug the author can do anything about.
pub async fn list_listeners(
    pool: &PgPool,
    workflow_id: Uuid,
    limit: i64,
) -> Result<Vec<TestListener>> {
    let rows: Vec<TestListener> = sqlx::query_as(
        "select id, workflow_id, node_id, event_name, created_by, created_at, expires_at, \
                consumed_at, event_id, event_name_captured, payload \
         from workflow_test_listeners where workflow_id = $1 \
         order by created_at desc, id desc limit $2",
    )
    .bind(workflow_id)
    .bind(limit.clamp(1, 100))
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Read one listener by its token.
///
/// The read is scoped to the caller, so a token only ever names a listener inside the
/// organization that armed it. A `None` covers both "no such token" and "not yours", and the
/// two are deliberately not told apart: a caller who can tell them apart can enumerate other
/// organizations' armed listeners one token at a time.
pub async fn find_by_token(
    pool: &PgPool,
    token_hash: &str,
    organization_id: Uuid,
) -> Result<Option<TestListener>> {
    let row: Option<TestListener> = sqlx::query_as(
        "select id, workflow_id, node_id, event_name, created_by, created_at, expires_at, \
                consumed_at, event_id, event_name_captured, payload \
         from workflow_test_listeners where token_hash = $1 and organization_id = $2",
    )
    .bind(token_hash)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Fill a live listener in with the event that fired it.
///
/// Returns the ids of the rows it wrote. `Ok(empty)` is the one-shot contract rather than an
/// error: two matchers racing the same event both call this and only the first updates a row,
/// and the second must not be reported as a failure of the bus.
///
/// **The `expires_at > $now` predicate is load-bearing and is the whole of "the listener
/// expires after 15 minutes".** Without it an expired row is still `consumed_at is null`, so
/// the matcher fills it and the panel reports a capture for an event nobody was watching —
/// a listener that answered a question asked an hour ago. It is the same predicate the
/// panel reads with, which is the point of [`listener_is_live`].
pub async fn capture(
    pool: &PgPool,
    workflow_id: Uuid,
    node_id: &str,
    event_id: i64,
    event_name: &str,
    payload: &Value,
    now: OffsetDateTime,
) -> Result<Vec<Uuid>> {
    // A payload the table's own size constraint would refuse is dropped here rather than
    // being handed to a write that fails mid-batch: an over-large event is a fact about the
    // payload, and losing the listener's arming to report it would be a worse outcome than
    // saying nothing.
    if payload.to_string().len() > MAX_PAYLOAD_BYTES {
        tracing::warn!(
            workflow_id = %workflow_id,
            node_id,
            event_id,
            "a captured payload is larger than the listener can store; the capture is dropped"
        );
        return Ok(Vec::new());
    }

    let written: Vec<Uuid> = sqlx::query_scalar(
        "update workflow_test_listeners \
            set consumed_at = $4, event_id = $5, event_name_captured = $6, payload = $3 \
          where workflow_id = $1 and node_id = $2 and consumed_at is null and expires_at > $4 \
          returning id",
    )
    .bind(workflow_id)
    .bind(node_id)
    .bind(payload)
    .bind(now)
    .bind(event_id)
    .bind(event_name)
    .fetch_all(pool)
    .await?;

    Ok(written)
}

/// Arm a listener, but refuse a node that is not on the rule's graph.
///
/// The obvious implementation arms whatever `node_id` the client sends, and the failure is
/// silent in a way that costs an author fifteen minutes: the panel says "listening", the
/// matcher never matches (there is no such node), and the listener expires with nothing to
/// show. A node id is a string in a jsonb graph, so this is the one place a client-supplied
/// node id can be checked against something — and the check is on the **stored** graph, not
/// the one the browser is holding, because the matcher reads the stored one and an arm
/// against an unsaved node would never be filled.
///
/// The refusals are three sentences because they are three different mistakes: no node at
/// all, a node that is not on the graph, and a node that exists but is not a trigger — the
/// last one because a listener is armed for an *event*, and a transform node has no event of
/// its own to wait for.
pub async fn arm_for_known_node(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    node_id: &str,
    created_by: Uuid,
    now: OffsetDateTime,
) -> Result<(TestListener, String)> {
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err(WorkflowError::invalid(
            "node_required",
            "choose a node on the canvas before listening for an event".to_owned(),
        ));
    }

    let definition = crate::graph_store::find_graph(pool, workflow_id)
        .await?
        .ok_or_else(|| WorkflowError::invalid("workflow_not_found", "this rule is not here"))?;

    let node = definition.graph.nodes.iter().find(|node| node.id == node_id);
    let Some(node) = node else {
        return Err(WorkflowError::invalid(
            "unknown_node",
            format!("{node_id:?} is not on this rule's canvas; save the graph first"),
        ));
    };

    if !crate::graph::is_trigger_type(&node.node_type) {
        return Err(WorkflowError::invalid(
            "not_a_trigger",
            format!(
                "{:?} is a {} node, and a listener waits for the event that starts a run; \
                 select a trigger node instead",
                node.label, node.node_type
            ),
        ));
    }

    // The event name is the trigger node's own `params.event`, never its label: the label is
    // what the author typed on the card ("New customer"), and arming a listener for that
    // would wait for an event nobody publishes. A trigger that names no event cannot be
    // listened to, and that is said in words rather than armed against an empty string.
    let event_name = node
        .params
        .get("event")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|event| !event.is_empty());

    let Some(event_name) = event_name else {
        return Err(WorkflowError::invalid(
            "no_event_on_trigger",
            format!(
                "{:?} does not name an event yet; set one on the trigger before listening",
                node.label
            ),
        ));
    };

    arm_listener(
        pool,
        organization_id,
        workflow_id,
        node_id,
        event_name,
        created_by,
        now,
    )
    .await
}

/// Drop listeners whose window closed more than `keep` ago, and report what went.
///
/// The window is `expires_at + grace` rather than `expires_at`: a row that expired thirty
/// seconds ago is the one an author is still looking at, and deleting it turns "this listener
/// has expired" into nothing at all. A grace of a day keeps yesterday's captures readable
/// and matches what the rule-shaped test rows get.
pub async fn prune(pool: &PgPool, now: OffsetDateTime, grace: Duration) -> Result<u64> {
    let removed = sqlx::query(
        "delete from workflow_test_listeners \
         where consumed_at is null and expires_at < $1",
    )
    .bind(now - grace)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listener(expires_at: OffsetDateTime, consumed: Option<OffsetDateTime>) -> TestListener {
        let now = OffsetDateTime::now_utc();
        TestListener {
            id: Uuid::new_v4(),
            workflow_id: Uuid::new_v4(),
            node_id: "node-1".to_owned(),
            event_name: "page.published".to_owned(),
            created_by: Some(Uuid::new_v4()),
            created_at: now,
            expires_at,
            consumed_at: consumed,
            event_id: consumed.map(|_| 42),
            event_name_captured: consumed.map(|_| "page.published".to_owned()),
            payload: consumed.map(|_| json!({ "id": 1 })),
        }
    }

    #[test]
    fn a_fresh_listener_is_armed() {
        let now = OffsetDateTime::now_utc();
        let row = listener(now + LISTENER_TTL, None);
        assert!(listener_is_live(&row, now));
        assert_eq!(status_of(&row, now), ListenerStatus::Armed);
    }

    #[test]
    fn a_past_its_window_is_not_live() {
        let now = OffsetDateTime::now_utc();
        let row = listener(now - Duration::seconds(1), None);
        assert!(!listener_is_live(&row, now), "an expired row must not be live");
        assert_eq!(status_of(&row, now), ListenerStatus::Expired);
    }

    #[test]
    fn a_captured_listener_is_captured_even_after_its_window() {
        // The order in `status_of` is load-bearing: checking expiry first would relabel a
        // captured listener as expired the moment its 15 minutes ran out, and the payload —
        // the whole reason it was armed — would read as a failure to capture.
        let now = OffsetDateTime::now_utc();
        let row = listener(now - Duration::minutes(3), Some(now - Duration::minutes(2)));
        assert!(!listener_is_live(&row, now));
        assert_eq!(status_of(&row, now), ListenerStatus::Captured);
    }

    #[test]
    fn the_boundary_is_closed_on_both_sides() {
        // `expires_at > now` is a strict inequality; `expires_at == now` is expired. A
        // listener that was still live a microsecond ago must not answer a capture now.
        let now = OffsetDateTime::now_utc();
        assert!(!listener_is_live(&listener(now, None), now));
        assert!(listener_is_live(&listener(now + Duration::microseconds(1), None), now));
    }

    #[test]
    fn a_token_hashes_to_a_stable_digest_and_never_stores_itself() {
        let (token, hash) = mint_token();
        assert_eq!(token.len(), TOKEN_LENGTH);
        assert_eq!(hash.len(), 64, "sha-256 hex is 64 characters");
        assert_eq!(hash, hash_token(&token));
        assert_ne!(token, hash, "the cleartext is never what is stored");
        assert!(!hash.contains(&token));
    }

    #[test]
    fn two_tokens_never_collide() {
        let (first, first_hash) = mint_token();
        let (second, second_hash) = mint_token();
        assert_ne!(first, second);
        assert_ne!(first_hash, second_hash);
    }
}
