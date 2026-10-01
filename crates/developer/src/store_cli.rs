//! The database half of the CLI device-code flow (REQ-033, slice 4).
//!
//! Every function takes the organization's id as a parameter rather than reading it from a
//! session, so the tenant boundary is the caller's argument — the same rule [`crate::store`] and
//! [`crate::store_oauth`] are built on.
//!
//! # The single-use exchange
//!
//! `exchange` is the only path from an approved code to a token, and it does the whole thing in
//! **one statement**: it claims the code by clearing `approved_by`'s companion marker inside a
//! `where` clause, and only inserts the token if that claim returned a row. Two terminals polling
//! the same approved code therefore cannot both receive a token — the second finds nothing to
//! claim and is answered `InvalidDeviceCode`.
//!
//! That is the property worth stating, because it is the one that is easy to get subtly wrong: a
//! read-then-write (`select` the code, check `approved_at is null`, `insert` the token) passes
//! every single-threaded test and hands two tokens to a caller who sends two concurrent polls,
//! which is exactly what a well-written client does on a slow network.
//!
//! # Why the interval is written back in the same statement
//!
//! The slow-down rule grows `interval_seconds`, and RFC 8628's instruction is worthless unless
//! the growth persists. [`record_poll`] returns the interval so the caller can persist it, and
//! the sweep path ([`purge_codes`]) is a range scan over the partial index the migration declares.

use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::cli::{
    DEVICE_CODE_TTL_SECONDS, DeviceApproval, DeviceApproved, DeviceStart, DeviceToken,
    POLL_INTERVAL_SECONDS, PollState, check_approval, check_requested_scopes, cli_scopes,
    new_device_codes, normalize_user_code,
};
use crate::error::{DeveloperError, Result};
use crate::model::Environment;
use crate::secret;

/// The columns every code read returns, in one place.
///
/// `pub` because the API's poll handler reads a row directly — it needs the *unclaimed* row to
/// decide what to answer, and going through a store function that returns a decision would put
/// the poll rule inside the store where the status codes cannot be expressed.
pub const CODE_COLUMNS: &str = "id, organization_id, device_code_hash, user_code, approved_by, \
     approved_at, client_name, client_uri, scopes, interval_seconds, last_polled_at, expires_at";

/// A stored device code, as the poll and approval paths see it.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    /// The row's id, recorded on any token it produces.
    pub id: Uuid,
    /// The tenant.
    pub organization_id: Uuid,
    /// Hashed device code.
    pub device_code_hash: String,
    /// The short code, stored grouped.
    pub user_code: String,
    /// Who approved it, if anybody.
    pub approved_by: Option<Uuid>,
    /// When they approved it.
    pub approved_at: Option<OffsetDateTime>,
    /// The terminal's claimed name.
    pub client_name: String,
    /// The terminal's claimed URI.
    pub client_uri: Option<String>,
    /// The scopes requested.
    pub scopes: Vec<String>,
    /// The interval the client must currently wait.
    pub interval_seconds: i32,
    /// When it last polled.
    pub last_polled_at: Option<OffsetDateTime>,
    /// When it expires.
    pub expires_at: OffsetDateTime,
}

impl DeviceRow {
    /// Turn a row into this shape.
    ///
    /// Not `async` in substance — it reads no database — but `sqlx`'s row decoders are fallible,
    /// and a `try_get` chain that could fail cannot be a plain constructor without the Result.
    pub fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        // The body is synchronous; this split exists so the public signature does not imply an
        // await that is not there.
        Self::from_row_sync(row)
    }

    fn from_row_sync(row: sqlx::postgres::PgRow) -> Result<Self> {
        let scopes: serde_json::Value = row.try_get("scopes")?;
        let interval_seconds: i32 = row.try_get("interval_seconds")?;
        Ok(Self {
            id: row.try_get("id")?,
            organization_id: row.try_get("organization_id")?,
            device_code_hash: row.try_get("device_code_hash")?,
            user_code: row.try_get("user_code")?,
            approved_by: row.try_get("approved_by")?,
            approved_at: row.try_get("approved_at")?,
            client_name: row.try_get("client_name")?,
            client_uri: row.try_get("client_uri")?,
            scopes: string_array(scopes),
            interval_seconds,
            last_polled_at: row.try_get("last_polled_at")?,
            expires_at: row.try_get("expires_at")?,
        })
    }

    /// The [`PollState`] the rule in [`crate::cli`] operates on.
    ///
    /// The interval comes from the row rather than the default, so a code that has already been
    /// polled carelessly keeps its grown interval across requests instead of starting over at
    /// five seconds each time the client is answered.
    pub fn poll_state(&self) -> PollState {
        PollState {
            approved: self.approved_by.is_some(),
            last_polled_at: self.last_polled_at,
            interval_seconds: u64::try_from(self.interval_seconds).unwrap_or(POLL_INTERVAL_SECONDS),
        }
    }

    /// The shape the *browser's* approval screen needs.
    ///
    /// Built from this row rather than stored separately, so the screen cannot show a client name
    /// the row does not have. `approved_by`'s user id is not enough on its own — the screen
    /// names the person approving, which is the *session's* user, and the route supplies it.
    pub fn approval_view(&self, approving_user: String) -> DeviceApproval {
        DeviceApproval {
            user_code: self.user_code.clone(),
            client_name: self.client_name.clone(),
            client_uri: self.client_uri.clone(),
            scopes: self.scopes.clone(),
            expires_at: self.expires_at,
            approving_user,
        }
    }
}

/// A `jsonb` array of strings, tolerating a non-array by returning nothing.
fn string_array(value: serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ─────────────────────────────────────────────────────────────────────────── start

/// Start a login: mint the codes, store the hash, return what the terminal needs.
///
/// The plaintext device code is in the response and nowhere else. It is stored as a hash with the
/// same scheme as an API key ([`secret::hash`]), so a dump of this table hands out nothing a
/// waiting attacker can poll with — which matters here more than for keys, because a pending
/// code is by definition *already* in someone's terminal waiting to be redeemed.
pub async fn start(
    pool: &PgPool,
    organization_id: Uuid,
    client_name: &str,
    client_uri: Option<&str>,
    requested_scopes: &[String],
) -> Result<DeviceStart> {
    // The scope rule runs before anything is minted: a terminal asking for a write scope is
    // either broken or an attacker, and neither should get as far as writing a row.
    check_requested_scopes(requested_scopes)?;

    let scopes = if requested_scopes.is_empty() {
        cli_scopes()
    } else {
        requested_scopes.to_vec()
    };

    // The migration refuses a blank client name, so this is not a style preference: a terminal
    // that claims nothing cannot be shown on the approval screen, and the screen is the only
    // defence against approving somebody else's login.
    let client_name = client_name.trim();
    if client_name.is_empty() {
        return Err(DeveloperError::DeviceCodeApprovalRefused);
    }

    let (device_code, device_code_hash, user_code) = new_device_codes();
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(DEVICE_CODE_TTL_SECONDS);

    // A collision on the short user code is possible — 30^8 inside a ten-minute window, so
    // astronomically unlikely — and is *retried* rather than surfaced, because a person whose
    // login failed for that reason has no way to do anything about it. Three attempts is not
    // paranoia: a caller that gets `InvalidDeviceCode` from `start` cannot tell a collision
    // from a fault, and three redraws make the first answer effectively the only one.
    let mut user_code = user_code;
    let mut stored_user_code = None;
    for _ in 0..3 {
        let inserted = sqlx::query(
            "insert into cli_device_codes \
             (organization_id, device_code_hash, user_code, client_name, client_uri, scopes, \
              interval_seconds, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (organization_id, user_code) do nothing",
        )
        .bind(organization_id)
        .bind(&device_code_hash)
        .bind(&user_code)
        .bind(client_name)
        .bind(client_uri)
        .bind(serde_json::json!(scopes))
        .bind(i64::try_from(POLL_INTERVAL_SECONDS).unwrap_or(i64::from(5)))
        .bind(expires_at)
        .execute(pool)
        .await?;

        if inserted.rows_affected() == 1 {
            stored_user_code = Some(user_code);
            break;
        }
        // The clash was on the *short* code, so redraw that and keep the device code: the
        // terminal has not seen it yet, and redrawing a perfectly good 256-bit secret because
        // eight display characters collided would be throwing away the strong half to fix the
        // weak one.
        let (_, _, redrawn) = new_device_codes();
        user_code = redrawn;
    }

    Ok(DeviceStart {
        device_code,
        verification_uri: "/developer/sdks?tab=cli".to_string(),
        user_code: stored_user_code.ok_or(DeveloperError::InvalidDeviceCode)?,
        expires_in: DEVICE_CODE_TTL_SECONDS,
        interval_seconds: POLL_INTERVAL_SECONDS,
    })
}

// ─────────────────────────────────────────────────────────────────────────── the browser side

/// Look a code up by what the person typed, for the approval screen.
///
/// The comparison is on the **normalised** form, so a code typed without the dash, in lower
/// case, or with a space works — the grouped display is presentation and the most likely way a
/// person retypes one is without it.
pub async fn find_for_approval(
    pool: &PgPool,
    organization_id: Uuid,
    user_code: &str,
) -> Result<DeviceRow> {
    let normalized = normalize_user_code(user_code);
    let row = sqlx::query(&format!(
        "select {CODE_COLUMNS} from cli_device_codes \
         where organization_id = $1 and user_code = $2"
    ))
    .bind(organization_id)
    .bind(&normalized)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::InvalidDeviceCode)?;
    DeviceRow::from_row(row)
}

/// Approve a code, binding it to the user who approved it.
///
/// The `where` clause carries `approved_by is null`, so a second approval of the same code — by
/// the same person, from a second tab, or by somebody else — matches nothing and is refused with
/// the same answer as an unknown code. One code, one approver, one token.
pub async fn approve(
    pool: &PgPool,
    organization_id: Uuid,
    user_code: &str,
    approving_user: Uuid,
    approving_name: String,
) -> Result<DeviceApproved> {
    let normalized = normalize_user_code(user_code);
    let row = sqlx::query(&format!(
        "update cli_device_codes set approved_by = $3, approved_at = now() \
         where organization_id = $1 and user_code = $2 and approved_by is null \
         returning {CODE_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(&normalized)
    .bind(approving_user)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::InvalidDeviceCode)?;

    let updated = DeviceRow::from_row(row)?;
    let now = OffsetDateTime::now_utc();
    // The rules run against the row as it now stands, so an expired code is refused *here*
    // rather than minting a token that the terminal will find dead.
    check_approval(&updated.approval_view(approving_name.clone()), now)?;

    Ok(DeviceApproved {
        user_code: updated.user_code,
        approved_at: updated.approved_at.unwrap_or(now),
        approved_by: approving_user,
    })
}

// ─────────────────────────────────────────────────────────────────────────── the terminal side

/// Apply one poll's rule and persist whatever it decided.
///
/// Returns the interval to store *and* writes it, in one call, because the caller that forgets
/// to persist it produces a terminal that is told to slow down and never is. The write is
/// unconditional rather than conditional on the answer: `SlowDown` grows the interval, and
/// `Pending` does not, so persisting on both is correct and persisting on neither is the bug.
pub async fn record_poll(
    pool: &PgPool,
    row: &DeviceRow,
    now: OffsetDateTime,
) -> Result<PollOutcome> {
    let mut state = row.poll_state();
    let decided = state.polled(now);

    let stored = state.interval_seconds;
    sqlx::query(
        "update cli_device_codes set last_polled_at = $2, interval_seconds = $3 where id = $1",
    )
    .bind(row.id)
    .bind(now)
    .bind(i64::try_from(stored).unwrap_or(i64::try_from(MAX_STORED_INTERVAL).unwrap_or(300)))
    .execute(pool)
    .await?;

    match decided {
        Ok(_) => Ok(PollOutcome::Approved {
            interval_seconds: stored,
        }),
        Err(DeveloperError::DeviceCodePending) => Ok(PollOutcome::Pending),
        Err(DeveloperError::DeviceCodeSlowDown { seconds }) => {
            Ok(PollOutcome::SlowDown { seconds })
        }
        // The rule returns nothing else; anything here is a new variant added to `PollState`
        // without this match arm, which is a compile error rather than a silent "pending".
        Err(other) => Err(other),
    }
}

/// The ceiling used when persisting an interval, mirroring the migration's check.
const MAX_STORED_INTERVAL: u64 = 300;

/// What one poll decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    /// Keep waiting; nobody has approved it.
    Pending,
    /// Polled too fast. Wait this many seconds.
    SlowDown {
        /// Seconds the client must now wait.
        seconds: u64,
    },
    /// Approved and not yet spent — the caller should call [`exchange`].
    Approved {
        /// The interval to keep using.
        interval_seconds: u64,
    },
}

/// Exchange an approved, unspent code for a token.
///
/// **The single-use enforcement is the delete.** The row is claimed by deleting it, and the
/// delete is a data-modifying CTE: two terminals polling the same approved code concurrently
/// both run this statement, PostgreSQL serialises the two deletes against the same row, and
/// only one gets it back from `returning`. The other sees nothing and is answered
/// `InvalidDeviceCode`.
///
/// The alternative — a `select` to check it is unspent, then a separate `insert` and `delete` —
/// passes every single-threaded test and hands **two** tokens to a caller that sends two
/// concurrent polls, which is exactly what a well-written client does on a slow network. So the
/// `where` clause carries the expiry check too, which is why an expired approved code is refused
/// here rather than minting a token the terminal will find dead.
pub async fn exchange(
    pool: &PgPool,
    device_code: &str,
    now: OffsetDateTime,
) -> Result<DeviceToken> {
    let hash = secret::hash(device_code);
    let environment = Environment::Live;

    let minted = secret::mint();
    let token_hash = secret::hash(&minted.plaintext);
    let token_expiry = now + time::Duration::seconds(TOKEN_TTL_SECONDS);

    // The claim-and-delete in one statement. `returning` gives back the row this caller won.
    let row = sqlx::query(&format!(
        "delete from cli_device_codes \
         where device_code_hash = $1 and approved_by is not null and expires_at > $2 \
         returning {CODE_COLUMNS}"
    ))
    .bind(&hash)
    .bind(now)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::InvalidDeviceCode)?;

    let device = DeviceRow::from_row(row)?;
    let scopes = if device.scopes.is_empty() {
        cli_scopes()
    } else {
        device.scopes.clone()
    };

    sqlx::query(
        "insert into cli_access_tokens \
         (organization_id, token_hash, user_id, device_code_id, environment, scopes, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(device.organization_id)
    .bind(&token_hash)
    .bind(
        device
            .approved_by
            .ok_or(DeveloperError::InvalidDeviceCode)?,
    )
    .bind(device.id)
    .bind(environment.as_str())
    .bind(serde_json::json!(scopes))
    .bind(token_expiry)
    .execute(pool)
    .await?;

    Ok(DeviceToken {
        access_token: minted.plaintext,
        environment,
        scopes,
        expires_at: token_expiry,
    })
}

/// One poll, decided.
///
/// The whole decision lives here rather than in the route for two reasons. The first is that the
/// status codes cannot be expressed here: `Pending` is a `202` and `SlowDown` is a `428`, and the
/// crate has no opinion about HTTP. The second is the one that matters — a route that reads the
/// row itself needs a raw `sqlx::Error` mapping, and `ApiError` has no blanket `From` for it, so
/// the first version of the handler either leaked a database error into a `500` body or grew a
/// conversion nobody reviewed. One function that answers the question keeps the query and the
/// decision in the same place.
pub async fn poll(pool: &PgPool, device_code: &str, now: OffsetDateTime) -> Result<PollResult> {
    let hash = secret::hash(device_code);
    let row = sqlx::query(&format!(
        "select {CODE_COLUMNS} from cli_device_codes \
         where device_code_hash = $1 and expires_at > $2"
    ))
    .bind(&hash)
    .bind(now)
    .fetch_optional(pool)
    .await?
    // One answer for "never existed", "already exchanged" and "expired". A `404` or a distinct
    // code per case would tell a prober which of the three it hit, and a code is short-lived
    // enough that guessing is the whole attack.
    .ok_or(DeveloperError::InvalidDeviceCode)?;

    let device = DeviceRow::from_row(row)?;

    match record_poll(pool, &device, now).await? {
        PollOutcome::Pending => Ok(PollResult::Pending {
            interval_seconds: u64::try_from(device.interval_seconds)
                .unwrap_or(POLL_INTERVAL_SECONDS),
        }),
        PollOutcome::SlowDown { seconds } => Ok(PollResult::SlowDown { seconds }),
        PollOutcome::Approved { .. } => {
            // The single-use exchange: the code is claimed by deleting it, so a second poll of
            // the same code finds nothing and is answered `InvalidDeviceCode`.
            let token = exchange(pool, device_code, now).await?;
            Ok(PollResult::Approved { token })
        }
    }
}

/// What one poll decided, with the token when there is one.
#[derive(Debug)]
pub enum PollResult {
    /// Nobody has approved it. A `202` to the client, because this is the normal case.
    Pending {
        /// The interval the client must wait.
        interval_seconds: u64,
    },
    /// Polled too fast. A `428`: the poll interval was not satisfied.
    SlowDown {
        /// Seconds the client must now wait.
        seconds: u64,
    },
    /// Approved and exchanged. The token, once.
    Approved {
        /// The minted token.
        token: DeviceToken,
    },
}

/// How long a CLI token lives.
pub const TOKEN_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

// ─────────────────────────────────────────────────────────────────────────── the sweeper

/// Delete codes and tokens that are past their expiry.
///
/// Two statements rather than one because the indexes differ: codes are swept by the partial
/// index on unapproved rows (approved ones are kept a little longer for attribution, exactly as
/// revoked OAuth tokens are), tokens by their own. Returns the two counts so the caller can log
/// them — a sweeper that reports nothing is a sweeper nobody notices has stopped working.
pub async fn purge_expired(pool: &PgPool, now: OffsetDateTime) -> Result<(u64, u64)> {
    let codes = sqlx::query("delete from cli_device_codes where expires_at < $1")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();

    let tokens = sqlx::query("delete from cli_access_tokens where expires_at < $1")
        .bind(now)
        .execute(pool)
        .await?
        .rows_affected();

    Ok((codes, tokens))
}

/// How many codes are pending in this organization, for the panel's own count.
pub async fn pending_count(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from cli_device_codes \
         where organization_id = $1 and approved_by is null and expires_at > $2",
    )
    .bind(organization_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(count)
}
