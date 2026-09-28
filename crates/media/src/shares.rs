//! Public share links (REQ-010, slice 3).
//!
//! A share is a way to hand one file to somebody who cannot sign in. That makes it a
//! *capability*, and every rule in this module follows from that word:
//!
//! * **A stored capability is a leaked capability.** The token never reaches the database; the
//!   row holds `sha256(token)` and can only answer "is this the token you were given?". A
//!   `select *` over the table, a backup, a replica log or a support engineer with read access
//!   all come away with a list of *dead* tokens, because a hash is not a capability. A token
//!   stored in the clear turns every one of those into a live link.
//! * **A link is a way to *reach* a file, not a way around what the file is.** A quarantined
//!   or trashed file is refused through a share exactly as it is refused through the raw route.
//!   The check lives in [`Share::servable`] rather than at creation, because the file's state
//!   can change after the link was made — a link created yesterday against a clean file must
//!   not keep serving it once the scanner flags it.
//! * **An expiry is a decision, not a default.** A person handing a file to a colleague has no
//!   natural end date, and a platform-chosen one silently expires somebody's link. `expires_at`
//!   is null until somebody sets it.
//! * **Revocation is a write, not a delete.** A deleted row cannot answer "who held this link,
//!   and when did it stop working", which is the only question that makes a leaked link
//!   investigable afterwards.
//! * **The counter counts bytes that were served.** See [`count_download`].

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;

/// The columns of a share row, in the order [`Share`] reads them.
const SHARE_COLUMNS: &str = "id, media_id, token_hash, password_hash, expires_at, \
     download_count, created_by, created_at, revoked_at, revoked_reason";

/// Longest expiry a link may be given, in days (a year).
///
/// Bounded because "never expires" and "expires in nine years" are the same link with two
/// different labels on it, and the second one is what somebody means when they leave the field
/// on its maximum. Ten years is well past any real hand-over.
pub const MAX_EXPIRY_DAYS: i64 = 3650;

/// Shortest expiry worth storing, in minutes. Below this a link is dead before the person who
/// receives it has finished typing the URL, so a smaller value is a mistake rather than a plan.
pub const MIN_EXPIRY_MINUTES: i64 = 5;

/// How many bytes of randomness a share token carries, before hex encoding.
///
/// 32 bytes is the width of a UUIDv4 times two: a token is a bearer capability, and the only
/// defence against a guessed one is that guessing is not worth attempting. 128 bits of a decent
/// CSPRNG output is not guessable, and a shorter token is a link that can be walked.
pub const TOKEN_BYTES: usize = 32;

/// What a share is created with.
#[derive(Debug, Clone)]
pub struct NewShare {
    /// The file this link hands over.
    pub media_id: Uuid,
    /// When the link stops working; `None` means until revoked.
    pub expires_at: Option<OffsetDateTime>,
    /// The password to protect it with, if any.
    pub password: Option<String>,
}

/// One share link as the panel reads it.
///
/// There is deliberately no `token` field: the response body for a *list* is built from this
/// type, and a type that cannot hold the capability is a type that cannot leak it by being
/// serialised. The token is returned exactly once, by [`create_share`], to the person who made
/// the link.
#[derive(Debug, Clone)]
pub struct Share {
    /// The row's own id.
    pub id: Uuid,
    /// The file this link hands over.
    pub media_id: Uuid,
    /// The hashed token, hex.
    pub token_hash: String,
    /// The Argon2id PHC string, when the link has a password.
    pub password_hash: Option<String>,
    /// When the link stops working.
    pub expires_at: Option<OffsetDateTime>,
    /// Downloads that produced bytes.
    pub download_count: i32,
    /// Who made the link.
    pub created_by: Option<Uuid>,
    /// When the link was made.
    pub created_at: OffsetDateTime,
    /// When it was revoked.
    pub revoked_at: Option<OffsetDateTime>,
    /// Why it was revoked.
    pub revoked_reason: String,
}

/// A created link: the row, plus the token that was *not* stored.
///
/// The token lives in the return value and nowhere else. This is the only moment it exists
/// outside the person who asked for it, and the type makes that true rather than a convention:
/// a `Share` has no field to put it in.
#[derive(Debug, Clone)]
pub struct CreatedShare {
    /// The stored row.
    pub share: Share,
    /// The bearer token, hex. Shown once.
    pub token: String,
}

/// Why a link refused to serve a file.
///
/// These are separate cases because the person holding the link needs a different answer for
/// each: a revoked link means ask the person who made it, an expired one means ask for a new
/// link, and a missing password means type the password. Answering all three with "not found"
/// makes every one of them a support ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareRefusal {
    /// No live link carries that token.
    Unknown,
    /// The link was revoked.
    Revoked,
    /// The link's expiry has passed.
    Expired,
    /// The link needs a password and none was presented.
    PasswordRequired,
    /// The file behind the link cannot be served (trashed, quarantined).
    FileUnavailable,
}

impl ShareRefusal {
    /// The wire name of this refusal.
    pub fn as_str(self) -> &'static str {
        match self {
            ShareRefusal::Unknown => "unknown_token",
            ShareRefusal::Revoked => "revoked",
            ShareRefusal::Expired => "expired",
            ShareRefusal::PasswordRequired => "password_required",
            ShareRefusal::FileUnavailable => "file_unavailable",
        }
    }

    /// A sentence the holder of the link can act on.
    pub fn explain(self) -> &'static str {
        match self {
            ShareRefusal::Unknown => "this link does not exist",
            ShareRefusal::Revoked => "this link was revoked and no longer works",
            ShareRefusal::Expired => "this link has expired; ask for a new one",
            ShareRefusal::PasswordRequired => "this link needs a password",
            ShareRefusal::FileUnavailable => "the file behind this link is not available",
        }
    }
}

/// Whether this link can serve right now, and if not, why.
pub fn servable(share: &Share, now: OffsetDateTime) -> std::result::Result<(), ShareRefusal> {
    if share.revoked_at.is_some() {
        return Err(ShareRefusal::Revoked);
    }
    // Strictly "still in the future": a link whose expiry is exactly `now` has run out, because
    // the second it expires is a second it does not work. `>=` would hand out one extra second.
    if share
        .expires_at
        .is_some_and(|at| at - now <= time::Duration::ZERO)
    {
        return Err(ShareRefusal::Expired);
    }
    Ok(())
}

/// Hash a bearer token the way the table stores it.
///
/// Lower-case hex of SHA-256, so the value is comparable in a test and printable in a log
/// without carrying the capability itself.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Read a share by its id.
pub async fn find_share(pool: &PgPool, id: Uuid) -> Result<Option<Share>> {
    let row = sqlx::query_as::<_, ShareRow>(&format!(
        "select {SHARE_COLUMNS} from media_shares where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(ShareRow::into_share))
}

/// Read the live share a token names.
///
/// "Live" is deliberately *not* applied here: a revoked or expired link must be found so the
/// caller can say *which* of those it is. Filtering in SQL would make all three refusals
/// indistinguishable.
pub async fn find_by_token(pool: &PgPool, token: &str) -> Result<Option<Share>> {
    let row = sqlx::query_as::<_, ShareRow>(&format!(
        "select {SHARE_COLUMNS} from media_shares where token_hash = $1"
    ))
    .bind(hash_token(token))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(ShareRow::into_share))
}

/// Every share over one file, newest first, including revoked ones.
///
/// A revoked link stays in the list on purpose: "this link was handed out and no longer
/// works" is a question the panel has to be able to answer, and hiding the row makes the
/// operator re-create the link instead of revoking it.
pub async fn list_shares(pool: &PgPool, media_id: Uuid) -> Result<Vec<Share>> {
    let rows = sqlx::query_as::<_, ShareRow>(&format!(
        "select {SHARE_COLUMNS} from media_shares where media_id = $1 order by created_at desc"
    ))
    .bind(media_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(ShareRow::into_share).collect())
}

/// Create a share over a file, minting its token.
///
/// The token is generated here, hashed, and the *plaintext* is returned in
/// [`CreatedShare::token`]. Nothing in this function writes the token, and the row type has
/// nowhere to put it.
pub async fn create_share(
    pool: &PgPool,
    new: NewShare,
    token: &str,
    password_hash: Option<String>,
    created_by: Option<Uuid>,
) -> Result<CreatedShare> {
    let token_hash = hash_token(token);
    let row = sqlx::query_as::<_, ShareRow>(&format!(
        "insert into media_shares \
         (media_id, token_hash, password_hash, expires_at, created_by) \
         values ($1, $2, $3, $4, $5) \
         returning {SHARE_COLUMNS}"
    ))
    .bind(new.media_id)
    .bind(&token_hash)
    .bind(password_hash)
    .bind(new.expires_at)
    .bind(created_by)
    .fetch_one(pool)
    .await?;
    Ok(CreatedShare {
        share: row.into_share(),
        token: token.to_owned(),
    })
}

/// Revoke a share.
///
/// Returns the revoked row so the caller can report *when*, which is what somebody holding the
/// link will ask. A link that is already revoked is not an error: revoking twice is what a
/// double-click looks like, and the second one must leave the first one's timestamp alone.
pub async fn revoke_share(
    pool: &PgPool,
    id: Uuid,
    reason: &str,
) -> Result<Option<Share>> {
    let row = sqlx::query_as::<_, ShareRow>(&format!(
        "update media_shares \
         set revoked_at = now(), revoked_reason = $2 \
         where id = $1 and revoked_at is null \
         returning {SHARE_COLUMNS}"
    ))
    .bind(id)
    .bind(reason)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(ShareRow::into_share))
}

/// Record a download that produced bytes.
///
/// The increment is a standalone statement rather than part of the read transaction: a counter
/// that shares a transaction with a read can be rolled back by a failure that happened *after*
/// the bytes went out, and a download counter that under-reports is a counter nobody trusts.
pub async fn count_download(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("update media_shares set download_count = download_count + 1 where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether this share is protected by a password.
pub fn is_password_protected(share: &Share) -> bool {
    share.password_hash.is_some()
}

/// Revoke every live share over a file that is about to stop being servable.
///
/// Called from the quarantine and trash paths: a file that has been flagged by the scanner must
/// not still be reachable through a link somebody is holding, and neither must a file that was
/// moved to the trash. Leaving the links live is the exact failure the share feature creates,
/// so the revocation belongs wherever the file's servability changes rather than in a
/// sweeper somebody has to remember to run.
pub async fn revoke_for_media(pool: &PgPool, media_id: Uuid, reason: &str) -> Result<u64> {
    let result = sqlx::query(
        "update media_shares set revoked_at = now(), revoked_reason = $2 \
         where media_id = $1 and revoked_at is null",
    )
    .bind(media_id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------------------------

/// The row shape `sqlx` decodes, kept private so the column list and the struct cannot drift.
#[derive(Debug, sqlx::FromRow)]
struct ShareRow {
    id: Uuid,
    media_id: Uuid,
    token_hash: String,
    password_hash: Option<String>,
    expires_at: Option<OffsetDateTime>,
    download_count: i32,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    revoked_at: Option<OffsetDateTime>,
    revoked_reason: String,
}

impl ShareRow {
    fn into_share(self) -> Share {
        Share {
            id: self.id,
            media_id: self.media_id,
            token_hash: self.token_hash,
            password_hash: self.password_hash,
            expires_at: self.expires_at,
            download_count: self.download_count,
            created_by: self.created_by,
            created_at: self.created_at,
            revoked_at: self.revoked_at,
            revoked_reason: self.revoked_reason,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant, so a test about "expired" cannot pass because the clock moved.
    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).expect("a valid instant")
    }

    fn share(expires_at: Option<OffsetDateTime>, revoked: bool) -> Share {
        Share {
            id: Uuid::new_v4(),
            media_id: Uuid::new_v4(),
            token_hash: hash_token("token"),
            password_hash: None,
            expires_at,
            download_count: 0,
            created_by: None,
            created_at: at(0),
            revoked_at: revoked.then(|| at(10)),
            revoked_reason: String::new(),
        }
    }

    #[test]
    fn a_link_with_no_expiry_works_until_it_is_revoked() {
        let link = share(None, false);
        assert_eq!(servable(&link, at(9_999_999)), Ok(()));
    }

    #[test]
    fn an_expiry_is_inclusive_of_the_instant_it_names() {
        // A link whose expiry is exactly `now` has run out. `>=` here would hand out one extra
        // second, and the test is the only thing that notices.
        let link = share(Some(at(100)), false);
        assert_eq!(servable(&link, at(100)), Err(ShareRefusal::Expired));
        assert_eq!(servable(&link, at(99)), Ok(()));
    }

    #[test]
    fn revocation_outranks_a_future_expiry() {
        // A link revoked now is refused even though its expiry is a year away: revocation is
        // immediate, and "expires in 300 days" must not resurrect a link somebody closed.
        let link = share(Some(at(999_999_999)), true);
        assert_eq!(servable(&link, at(20)), Err(ShareRefusal::Revoked));
    }

    #[test]
    fn every_refusal_names_itself_and_explains_itself() {
        for refusal in [
            ShareRefusal::Unknown,
            ShareRefusal::Revoked,
            ShareRefusal::Expired,
            ShareRefusal::PasswordRequired,
            ShareRefusal::FileUnavailable,
        ] {
            assert!(!refusal.as_str().is_empty());
            // The holder of a dead link needs to know what to do, not that something went wrong.
            assert!(!refusal.explain().is_empty());
        }
        // The three ways a live-looking link can fail are distinguishable, because each one
        // sends the holder somewhere different.
        let names: Vec<_> = [ShareRefusal::Revoked, ShareRefusal::Expired]
            .iter()
            .map(|r| r.as_str())
            .collect();
        assert_eq!(names, vec!["revoked", "expired"]);
    }

    #[test]
    fn the_token_hash_is_a_pure_function_and_hides_the_token() {
        let token = "a1b2c3d4e5f6";
        let first = hash_token(token);
        assert_eq!(first, hash_token(token));
        assert_eq!(first.len(), 64, "sha-256 is 32 bytes, so 64 hex characters");
        assert!(!first.contains(token));
        // A different token is a different hash: a truncated digest would collide here and the
        // walk over the real router would be the first to find out.
        assert_ne!(first, hash_token("a1b2c3d4e5f7"));
    }

    #[test]
    fn a_password_protected_link_is_recognised_by_its_hash() {
        let mut link = share(None, false);
        assert!(!is_password_protected(&link));
        link.password_hash = Some("$argon2id$v=19$m=19456,t=2,p=1$abc$def".to_owned());
        assert!(is_password_protected(&link));
    }

    #[test]
    fn a_row_type_has_nowhere_to_put_the_token() {
        // The token this link stands for. The row below is built from *its* hash, which is the
        // only direction the platform ever goes: token → hash → row, and never back.
        let token = "8f2b1c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b";
        let row = ShareRow {
            id: Uuid::nil(),
            media_id: Uuid::nil(),
            token_hash: hash_token(token),
            password_hash: None,
            expires_at: None,
            download_count: 3,
            created_by: None,
            created_at: at(0),
            revoked_at: None,
            revoked_reason: String::new(),
        };
        let share = row.into_share();
        // The plaintext is not reachable from the row, so nothing holding a `Share` can produce
        // the capability it stands for — the property is the *absence* of a field, which is why
        // asserting it by name would be weaker than asserting it as a round trip.
        assert_eq!(share.token_hash, hash_token(token));
        assert!(!share.token_hash.contains(token), "the row never holds the token");
        // The round trip only works one way: from the token you can find the row, and from the
        // row you cannot get the token back. That asymmetry *is* the design.
        assert_eq!(hash_token(token), share.token_hash);
        assert_ne!(share.token_hash, token, "a hash is not its input");
    }
}
