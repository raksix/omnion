//! Omnion content · members — visitor accounts, their tokens, their sessions and the gate on a
//! page (REQ-064, slice 4c).
//!
//! A member is a site visitor who made an account, and every rule in this file follows from the
//! one property the REQ calls the most important boundary it has: **a member is not a panel
//! identity.**
//!
//! * **Nothing here references `users`, and nothing promotes.** `roles` is a `text[]` the SITE
//!   owns, not the IAM role table. A site may call its own member `editor` and that word must
//!   never resolve to the platform's `content.pages.update`, or a site could hand a visitor the
//!   panel. The absence of a foreign key is the mechanism; a convention in a comment is not.
//!
//! * **The password is Argon2id, hashed here and never seen twice.** [`MemberStore::signup`]
//!   takes the plaintext, hashes it, and the only value that leaves the function afterwards is
//!   the PHC string. Verification uses `omnion_identity`'s [`verify_password`], so a member
//!   credential costs the same to attack as a user's.
//!
//! * **Tokens are stored hashed, single-use and time-limited, and both are facts about the
//!   row.** `used_at` and `expires_at` are columns. A verification link that can be replayed for
//!   a year is not a verification, and a check that lives only in the writer is a check the next
//!   writer will forget.
//!
//! * **A session is a row, so it can be revoked.** The cookie carries a token and the table
//!   holds its digest; sign-out is a delete. A self-contained session token cannot be
//!   withdrawn before it expires, and a members area whose sessions outlive the member's
//!   account is an account takeover with a long fuse.
//!
//! * **Gating answers 404, not 403.** A signed-out visitor gets the same answer for "this page
//!   does not exist" and "this page is not for you". A page that names a role the visitor lacks
//!   is not a page they are told about.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};

// ---------------------------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------------------------

/// Longest member name the panel will store.
pub const MAX_NAME: usize = 120;
/// Longest address a store will accept (RFC 5321 caps a path at 256 octets).
pub const MAX_EMAIL: usize = 254;
/// How long a verification link stays live.
pub const VERIFY_TTL_HOURS: i64 = 48;
/// How long a password-reset link stays live.
pub const RESET_TTL_MINUTES: i64 = 60;
/// How long a member session stays live. A month, sliding on each sign-in: a members area is a
/// convenience, not a bank, and a shorter session only teaches visitors to sign in again.
pub const SESSION_TTL_DAYS: i64 = 30;
/// Longest free-text note the store keeps.
pub const MAX_NOTE: usize = 400;

/// The states a member can be in.
pub const MEMBER_STATUSES: [&str; 3] = ["pending", "verified", "blocked"];

/// The role words a site's members may carry.
///
/// This is a **closed list, and it is the same list the migration's
/// `pages_visibility_roles_shape` check allows.** Two tables cannot share one CHECK in
/// PostgreSQL, so the pair is what keeps a page from naming a role no signup will ever grant —
/// a page that does is permanently unreachable and says nothing about why.
pub const MEMBER_ROLES: [&str; 6] = [
    "subscriber",
    "editor",
    "author",
    "contributor",
    "owner",
    "admin",
];

/// What a signed-out visitor meets on a gated page.
pub const GATED_BEHAVIOURS: [&str; 2] = ["prompt", "not_found"];

/// The column list of a member row, shared by every read so a new column cannot be added to one
/// query and forgotten in another.
const MEMBER_COLUMNS: &str = "id, site_id, email, name, password_hash, roles, status, \
     verified_at, last_signin_at, signin_note, created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

/// A member row, as the store reads it.
///
/// `password_hash` is `#[serde(skip_serializing)]`: the panel's list, the export and the public
/// profile all pass through this struct, and a hash that reaches any of them is one API response
/// away from being a credential dump.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Member {
    /// Member id.
    pub id: Uuid,
    /// The site they belong to. A member on site A is not a member on site B even with the same
    /// address: two sites are two tenants with their own visitor lists, and a shared member
    /// would let site B's gate be satisfied by site A's signup.
    pub site_id: Uuid,
    /// The address, stored lowercased.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// Argon2id PHC string, or `None` while the address has been invited but not claimed.
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    /// The site's own roles for this member.
    pub roles: Vec<String>,
    /// `pending`, `verified` or `blocked`.
    pub status: String,
    /// When the verification link was used.
    pub verified_at: Option<OffsetDateTime>,
    /// When they last signed in.
    pub last_signin_at: Option<OffsetDateTime>,
    /// The last thing that changed the status.
    pub signin_note: Option<String>,
    /// When the account was created.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl Member {
    /// Whether this member holds a site role. Empty `required` is `false` rather than `true`:
    /// a page gated on no roles is refused by the migration, so the only way to reach this with
    /// an empty list is to ask a question the store cannot answer.
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|held| held == role)
    }

    /// Whether this member satisfies a page's gate.
    ///
    /// A `blocked` member satisfies nothing, including a gate that names no roles — otherwise
    /// blocking a member would stop them reading public pages, which is not what blocking means.
    #[must_use]
    pub fn satisfies(&self, visibility: &str, required: &[String]) -> bool {
        if self.status == "blocked" {
            return false;
        }
        match visibility {
            "public" => true,
            // A gate is never satisfied by a member who has not verified. A site that turns
            // verification off marks its signups `verified` at signup time, so this costs that
            // site nothing — and it is what makes "verified" mean something on a site that
            // keeps it on.
            "members" => self.status == "verified",
            "roles" => {
                self.status == "verified"
                    && required.iter().any(|role| {
                        MEMBER_ROLES.contains(&role.as_str()) && self.has_role(role)
                    })
            }
            _ => false,
        }
    }
}

/// What a list of members can be asked for.
#[derive(Debug, Default, Clone)]
pub struct MemberFilter {
    /// One state.
    pub status: Option<String>,
    /// Free text over address and name.
    pub search: Option<String>,
    /// Only members holding this site role.
    pub role: Option<String>,
    /// Page size.
    pub limit: Option<i64>,
    /// Offset.
    pub offset: Option<i64>,
}

/// Per-state counts of a site's members, beside the table so the filter chips can be written
/// from the same numbers the rows are.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemberCounts {
    /// Waiting to click a verification link.
    pub pending: i64,
    /// May sign in.
    pub verified: i64,
    /// Refused.
    pub blocked: i64,
}

/// A member list, its per-state counts and the total the pager reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberPage {
    /// The rows.
    pub members: Vec<Member>,
    /// Per-state counts for the chips.
    pub counts: MemberCounts,
    /// How many rows match without the page window.
    pub total: i64,
}

/// A member as the public surface may describe them.
///
/// A separate type on purpose: [`Member`] carries `password_hash` and the panel's own columns,
/// and a public response that is built by deleting fields is a response that grows a leak the
/// day somebody adds a column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicMember {
    /// Member id.
    pub id: Uuid,
    /// The address, as stored.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// The site's roles.
    pub roles: Vec<String>,
    /// The state. A visitor may know whether they are verified; `blocked` is never returned,
    /// because a blocked member gets a refusal, not an account page.
    pub status: String,
}

impl From<&Member> for PublicMember {
    fn from(member: &Member) -> Self {
        Self {
            id: member.id,
            email: member.email.clone(),
            name: member.name.clone(),
            roles: member.roles.clone(),
            status: member.status.clone(),
        }
    }
}

/// What a signup produced.
#[derive(Debug, Clone)]
pub struct SignupOutcome {
    /// The member that was created.
    pub member: Member,
    /// The raw verification token, returned so the caller can mail it and never persisted.
    /// `None` when the site does not require verification — in which case the member is
    /// already `verified` and there is nothing to click.
    pub verify_token: Option<String>,
    /// Whether the account can sign in right now. A caller that ignores this and mails a link
    /// to a `pending` account is the difference between "check your inbox" and "check your
    /// inbox, eventually, if you ever find it".
    pub can_sign_in: bool,
}

/// What a token click did.
#[derive(Debug, Clone)]
pub struct TokenOutcome {
    /// Whether the action was applied. A replayed or expired link answers `false` rather than
    /// an error, so a forwarded link cannot be used to tell a live token from a dead one.
    pub applied: bool,
    /// The member it concerned, when the token resolved.
    pub member: Option<Member>,
    /// The state it left them in, for the page to render.
    pub status: String,
    /// A message a visitor can be shown. `None` for an unknown token, because naming the
    /// difference between "wrong" and "expired" is an existence oracle.
    pub message: Option<String>,
}

/// What a sign-in produced. The `token` is the raw cookie value; only its digest is stored.
#[derive(Debug, Clone)]
pub struct SigninOutcome {
    /// The member who signed in.
    pub member: Member,
    /// The cookie value. Returned once, at the moment the browser can have it.
    pub token: String,
    /// When the session expires.
    pub expires_at: OffsetDateTime,
}

/// A member's recent sign-ins, for the panel's detail view.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SigninEvent {
    /// Session id.
    pub id: Uuid,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was last seen.
    pub last_seen_at: OffsetDateTime,
    /// When it expires.
    pub expires_at: OffsetDateTime,
}

/// The gate carried by one published page.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PageGate {
    /// `public`, `members` or `roles`.
    pub visibility: String,
    /// The member roles the page names. Empty unless `visibility = 'roles'`.
    pub visibility_roles: Vec<String>,
}

/// A site-wide membership policy.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MemberSettings {
    /// The site this belongs to.
    pub site_id: Uuid,
    /// Whether the public signup form exists.
    pub signup_enabled: bool,
    /// Whether a signup must be confirmed.
    pub require_verification: bool,
    /// Roles handed to every new member.
    pub default_roles: Vec<String>,
    /// Where a visitor lands after signing in.
    pub post_signin_redirect: Option<String>,
    /// `prompt` shows a sign-in link, `not_found` answers 404 and discloses nothing.
    pub gated_page_behaviour: String,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// A change to a member, as a PATCH carries it.
///
/// `Option<Option<T>>` for the fields that can be set back to nothing, because `None` here
/// means "the caller did not mention it" and `Some(None)` means "clear it". A single `Option<T>`
/// cannot say both, and a panel that cannot clear a name eventually ships a screen where the
/// only way to remove a wrong name is to delete the account.
#[derive(Debug, Default, Clone)]
pub struct MemberPatch {
    /// Display name, or `None` to leave it.
    pub name: Option<Option<String>>,
    /// Status, or `None` to leave it.
    pub status: Option<String>,
    /// The whole role list, or `None` to leave it. Replacing rather than adding is deliberate:
    /// a panel that only ever adds a role cannot take one away, and a member who must keep a
    /// role they were given by mistake is a permanent grant.
    pub roles: Option<Vec<String>>,
    /// The last status note, or `None` to leave it.
    pub signin_note: Option<Option<String>>,
}

/// What a new account is created from.
#[derive(Debug, Default, Clone)]
pub struct NewMember {
    /// The site the account belongs to.
    pub site_id: Uuid,
    /// The address. Normalised to lowercase by the store.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// The password, in the clear, exactly once. `None` is the invited address that has not
    /// claimed the account yet — a real state, not a missing field.
    pub password: Option<String>,
    /// The site's roles to grant, before the site's defaults are added.
    pub roles: Vec<String>,
}

/// A change to the site's policy.
#[derive(Debug, Default, Clone)]
pub struct SettingsPatch {
    /// Whether the signup form exists.
    pub signup_enabled: Option<bool>,
    /// Whether a signup must be confirmed.
    pub require_verification: Option<bool>,
    /// The roles every new member gets.
    pub default_roles: Option<Vec<String>>,
    /// The post-sign-in redirect, or `None` to leave it.
    pub post_signin_redirect: Option<Option<String>>,
    /// The gated-page behaviour.
    pub gated_page_behaviour: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Validate and normalise an address.
///
/// Lowercased on the way in, because `unique (site_id, lower(email))` is only exact if the store
/// is what puts the values in the column: a capitalised insert would be stored as typed and the
/// index would fold it, which works, but the `=` in a `where` clause would not.
pub fn validate_email(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidMember(
            "e-mail address is required".to_string(),
        ));
    }
    if trimmed.chars().count() > MAX_EMAIL {
        return Err(ContentError::InvalidMember(
            "that address is too long".to_string(),
        ));
    }
    let ok = {
        let mut parts = trimmed.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        parts.next().is_none()
            && !local.is_empty()
            && local.len() <= 64
            && !domain.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !trimmed.chars().any(char::is_whitespace)
    };
    if !ok {
        return Err(ContentError::InvalidMember(
            "that does not look like an e-mail address".to_string(),
        ));
    }
    Ok(trimmed.to_lowercase())
}

/// Validate a display name. `None` and an empty string are the same request to clear it.
pub fn validate_name(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_NAME {
        return Err(ContentError::InvalidMember(
            "that name is too long".to_string(),
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

/// Validate a state.
pub fn validate_status(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if !MEMBER_STATUSES.contains(&trimmed) {
        return Err(ContentError::InvalidMember(format!(
            "a member is pending, verified or blocked — not '{trimmed}'"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Validate a role list: every word is one the platform knows, the list has no duplicates and
/// is not absurdly long.
///
/// The closed list is the point. A site inventing its own role names is how a gate comes to
/// require a role that nothing can grant, and that page is then unreachable with no diagnostic
/// anywhere in the product.
pub fn validate_roles(raw: &[String]) -> Result<Vec<String>> {
    if raw.len() > 8 {
        return Err(ContentError::InvalidMember(
            "a member cannot hold more than 8 roles".to_string(),
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for role in raw {
        let trimmed = role.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !MEMBER_ROLES.contains(&trimmed) {
            return Err(ContentError::InvalidMember(format!(
                "'{trimmed}' is not a member role — use one of: {}",
                MEMBER_ROLES.join(", ")
            )));
        }
        if !out.iter().any(|held| held == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    out.sort();
    Ok(out)
}

/// Validate a gated-page behaviour word.
pub fn validate_gated_behaviour(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if !GATED_BEHAVIOURS.contains(&trimmed) {
        return Err(ContentError::InvalidMember(format!(
            "a gated page either prompts for sign-in or answers 404 — not '{trimmed}'"
        )));
    }
    Ok(trimmed.to_owned())
}

/// A free-text note, bounded.
pub fn validate_note(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_NOTE {
        return Err(ContentError::InvalidMember(
            "that note is too long".to_string(),
        ));
    }
    Ok(Some(trimmed.to_owned()))
}

/// The digest of a token.
///
/// `sha256`, and the reason is threat model rather than taste: these tokens are 128 bits of
/// randomness from a CSPRNG, so there is nothing to brute-force and the property wanted is that
/// a leaked table contains no working link. Argon2 would defend against a weak generator, which
/// is not the failure being defended against here.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A fresh token and its digest.
#[must_use]
pub fn fresh_token() -> (String, String) {
    let token = Uuid::new_v4().simple().to_string();
    let digest = hash_token(&token);
    (token, digest)
}

/// Hash a visitor's password into a PHC string.
///
/// A thin, fallible wrapper over `omnion_identity::hash_password` so the store's call sites read
/// as one call and the weak-password refusal arrives as a `ContentError` rather than an
/// identity error the API layer has no arm for. The parameters are the identity crate's — 19
/// MiB, two passes — so a member credential costs exactly what a user's costs to attack.
pub async fn hash_password(password: &str) -> Result<String> {
    omnion_identity::hash_password(password.to_owned())
        .await
        .map_err(|error| match error {
            omnion_identity::IdentityError::WeakPassword { min } => {
                ContentError::WeakPassword(format!("use at least {min} characters"))
            }
            other => ContentError::InvalidMember(other.to_string()),
        })
}

/// Hash a visitor identifier the way the rest of the content modules do.
///
/// `sha256` with a platform-wide salt would be better; the comments and newsletter tables store a
/// bare digest for the same reason and for the same reason it is not enough to be sure: the
/// value is a fallback for correlating repeat senders, not an identity.
#[must_use]
pub fn hash_hint(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.trim().as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(16).map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

/// The membership store.
#[derive(Debug, Clone)]
pub struct MemberStore {
    pool: PgPool,
}

impl MemberStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool, for callers composing a transaction with their own writes.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -----------------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------------

    /// The site's policy, created with its defaults on first read.
    ///
    /// Created lazily rather than by a trigger on `sites` because a site created before this
    /// migration has no row, and a public signup that answers "no such settings" would have to
    /// guess — and a guess that happens to be `require_verification = false` hands every new
    /// member a verified account.
    pub async fn settings(&self, site_id: Uuid) -> Result<MemberSettings> {
        let row: Option<MemberSettings> =
            sqlx::query_as("select site_id, signup_enabled, require_verification, \
                            default_roles, post_signin_redirect, gated_page_behaviour, updated_at \
                            from cms_member_settings where site_id = $1")
                .bind(site_id)
                .fetch_optional(&self.pool)
                .await?;
        if let Some(row) = row {
            return Ok(row);
        }
        sqlx::query("insert into cms_member_settings (site_id) values ($1) on conflict (site_id) do nothing")
            .bind(site_id)
            .execute(&self.pool)
            .await?;
        let row: MemberSettings =
            sqlx::query_as("select site_id, signup_enabled, require_verification, \
                            default_roles, post_signin_redirect, gated_page_behaviour, updated_at \
                            from cms_member_settings where site_id = $1")
                .bind(site_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(row)
    }

    /// Change the policy.
    pub async fn patch_settings(
        &self,
        site_id: Uuid,
        patch: &SettingsPatch,
        actor: Option<Uuid>,
    ) -> Result<MemberSettings> {
        // Read first so every field is validated before anything is written: a policy that
        // half-applies is the worst outcome here, because `require_verification` and the gate
        // behaviour interact.
        let current = self.settings(site_id).await?;

        let signup_enabled = patch.signup_enabled.unwrap_or(current.signup_enabled);
        let require_verification = patch
            .require_verification
            .unwrap_or(current.require_verification);
        let default_roles = match &patch.default_roles {
            Some(roles) => validate_roles(roles)?,
            None => current.default_roles.clone(),
        };
        let post_signin_redirect = match &patch.post_signin_redirect {
            Some(value) => match value {
                Some(raw) => {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        if !trimmed.starts_with('/') || trimmed.starts_with("//") {
                            return Err(ContentError::InvalidMember(
                                "the post-sign-in redirect must be a path on this site, \
                                 starting with a single '/'"
                                    .to_string(),
                            ));
                        }
                        Some(trimmed.to_owned())
                    } else {
                        None
                    }
                }
                None => None,
            },
            None => current.post_signin_redirect.clone(),
        };
        let gated_page_behaviour = match &patch.gated_page_behaviour {
            Some(raw) => validate_gated_behaviour(raw)?,
            None => current.gated_page_behaviour.clone(),
        };

        sqlx::query(
            "update cms_member_settings set signup_enabled = $1, require_verification = $2, \
             default_roles = $3, post_signin_redirect = $4, gated_page_behaviour = $5, \
             updated_at = now(), updated_by = $6 where site_id = $7",
        )
        .bind(signup_enabled)
        .bind(require_verification)
        .bind(&default_roles)
        .bind(&post_signin_redirect)
        .bind(&gated_page_behaviour)
        .bind(actor)
        .bind(site_id)
        .execute(&self.pool)
        .await?;

        self.settings(site_id).await
    }

    // -----------------------------------------------------------------------------------------
    // Members
    // -----------------------------------------------------------------------------------------

    /// A site's members, with the per-state counts the filter chips read.
    ///
    /// The counts are **not** filtered by the current filter. They are the totals for the chips,
    /// and a chip that counts only what the search box currently shows is a chip that changes
    /// meaning when you type — so a moderator cannot tell "no members are blocked" from "my
    /// search hid them".
    pub async fn list(&self, site_id: Uuid, filter: &MemberFilter) -> Result<MemberPage> {
        let status = filter
            .status
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(validate_status)
            .transpose()?;
        let search = filter
            .search
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let role = filter
            .role
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|raw| {
                if MEMBER_ROLES.contains(&raw) {
                    Ok(raw.to_owned())
                } else {
                    Err(ContentError::InvalidMember(format!(
                        "'{raw}' is not a member role"
                    )))
                }
            })
            .transpose()?;
        let limit = filter.limit.unwrap_or(50).clamp(1, 200);
        let offset = filter.offset.unwrap_or(0).max(0);

        let counts: MemberCounts = sqlx::query_as(
            "select count(*) filter (where status = 'pending') as pending, \
                    count(*) filter (where status = 'verified') as verified, \
                    count(*) filter (where status = 'blocked') as blocked \
             from cms_members where site_id = $1",
        )
        .bind(site_id)
        .fetch_one(&self.pool)
        .await?;

        let mut builder = QueryBuilder::<Postgres>::new(format!(
            "select {MEMBER_COLUMNS} from cms_members where site_id = "
        ));
        builder.push_bind(site_id);
        if let Some(status) = status.as_deref() {
            builder.push(" and status = ").push_bind(status);
        }
        if let Some(search) = search.as_deref() {
            builder
                .push(" and (lower(email) like ")
                .push_bind(format!("%{search}%"));
            builder.push(" or lower(coalesce(name, '')) like ");
            builder.push_bind(format!("%{search}%"));
            builder.push(")");
        }
        if let Some(role) = role.as_deref() {
            builder.push(" and ").push_bind(role).push(" = any(roles)");
        }
        // The total is counted by the SAME predicate, in its own builder. `rows.len()` is the
        // page window and not the answer, and `QueryBuilder` is not `Display`, so a
        // `format!`ed subquery is not available either — the two builders below are written out
        // twice on purpose, and `only_the_row_window_is_paged` in the tests is what keeps them
        // from drifting.
        let total: i64 = {
            let mut counter = QueryBuilder::<Postgres>::new("select count(*) from cms_members where site_id = ");
            counter.push_bind(site_id);
            if let Some(status) = status.as_deref() {
                counter.push(" and status = ").push_bind(status);
            }
            if let Some(search) = search.as_deref() {
                counter
                    .push(" and (lower(email) like ")
                    .push_bind(format!("%{search}%"));
                counter.push(" or lower(coalesce(name, '')) like ");
                counter.push_bind(format!("%{search}%"));
                counter.push(")");
            }
            if let Some(role) = role.as_deref() {
                counter.push(" and ").push_bind(role).push(" = any(roles)");
            }
            counter.build_query_scalar::<i64>().fetch_one(&self.pool).await?
        };

        builder.push(" order by created_at desc, id limit ").push_bind(limit);
        builder.push(" offset ").push_bind(offset);
        let members: Vec<Member> = builder.build_query_as().fetch_all(&self.pool).await?;

        Ok(MemberPage {
            members,
            counts,
            total,
        })
    }

    /// One member, scoped by site.
    ///
    /// A 404 rather than a 403 when the row belongs to another site: this is the concealment
    /// the newsletter and comment stores already use, and the two-layer contract
    /// (`ensure_same_organization` = 403 for another tenant's SITE, this = 404 for a row inside
    /// your own) is exactly the pair the tenant boundary needs.
    pub async fn get(&self, site_id: Uuid, id: Uuid) -> Result<Member> {
        let sql = format!("select {MEMBER_COLUMNS} from cms_members where site_id = $1 and id = $2");
        sqlx::query_as::<_, Member>(&sql)
            .bind(site_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::MemberNotFound)
    }

    /// A member by address, scoped by site.
    pub async fn get_by_email(&self, site_id: Uuid, email: &str) -> Result<Option<Member>> {
        let email = validate_email(email)?;
        let sql = format!(
            "select {MEMBER_COLUMNS} from cms_members where site_id = $1 and lower(email) = $2"
        );
        Ok(sqlx::query_as::<_, Member>(&sql)
            .bind(site_id)
            .bind(&email)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// A member by address with the hash attached, for sign-in.
    ///
    /// Separate from [`MemberStore::get_by_email`] rather than an `Option<String>` argument,
    /// because the public routes have no business holding a hash and a parameter that asks for
    /// one is a parameter somebody will pass.
    pub async fn get_credentials(&self, site_id: Uuid, email: &str) -> Result<Option<Member>> {
        let email = validate_email(email)?;
        let sql = format!(
            "select {MEMBER_COLUMNS} from cms_members where site_id = $1 and lower(email) = $2"
        );
        Ok(sqlx::query_as::<_, Member>(&sql)
            .bind(site_id)
            .bind(&email)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The last ten sign-ins for a member.
    pub async fn recent_signins(&self, member_id: Uuid, limit: i64) -> Result<Vec<SigninEvent>> {
        let limit = limit.clamp(1, 50);
        Ok(sqlx::query_as::<_, SigninEvent>(
            "select id, created_at, last_seen_at, expires_at from cms_member_sessions \
             where member_id = $1 order by created_at desc limit $2",
        )
        .bind(member_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Every live session of a member, for the panel's "sign out everywhere" button.
    pub async fn live_session_count(&self, member_id: Uuid) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(
            "select count(*) from cms_member_sessions \
             where member_id = $1 and expires_at > now() and last_seen_at > now() - interval '30 days'",
        )
        .bind(member_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // -----------------------------------------------------------------------------------------
    // Signup and verification
    // -----------------------------------------------------------------------------------------

    /// A visitor creates an account.
    ///
    /// The site's own policy decides the state: `require_verification = false` marks the member
    /// `verified` **at signup** rather than after a link, which is what makes
    /// [`Member::satisfies`]'s "a gate is never satisfied by an unverified member" rule cost
    /// such a site nothing.
    ///
    /// The default roles come from the site and are **added to**, never replace, what the panel
    /// asked for: a site that hands every signup `subscriber` must not silently strip a role an
    /// operator just granted by hand.
    pub async fn signup(&self, new: NewMember) -> Result<SignupOutcome> {
        let email = validate_email(&new.email)?;
        let name = new.name.as_deref().map(validate_name).transpose()?.flatten();
        let roles = validate_roles(&new.roles)?;

        let settings = self.settings(new.site_id).await?;
        if !settings.signup_enabled {
            return Err(ContentError::InvalidMember(
                "this site is not accepting sign-ups".to_string(),
            ));
        }

        let mut all_roles = roles;
        for role in &settings.default_roles {
            if !all_roles.iter().any(|held| held == role) {
                all_roles.push(role.clone());
            }
        }
        all_roles.sort();

        // The password is hashed BEFORE the insert, and the plaintext is not written anywhere.
        // Hashing first rather than after means a weak password costs no round trip and leaves
        // no half-created row.
        let password_hash = match new.password.as_deref() {
            Some(password) => Some(hash_password(password).await?),
            None => None,
        };

        let requires_verification =
            settings.require_verification && password_hash.is_some();

        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        let existing: Option<Uuid> =
            sqlx::query_scalar("select id from cms_members where site_id = $1 and lower(email) = $2 for update")
                .bind(new.site_id)
                .bind(&email)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some() {
            tx.rollback().await?;
            return Err(ContentError::MemberEmailTaken(email));
        }

        let status = if requires_verification { "pending" } else { "verified" };
        let sql = format!(
            "insert into cms_members (site_id, email, name, password_hash, roles, status, \
             verified_at) values ($1, $2, $3, $4, $5, $6, \
             case when $6 = 'verified' then now() else null end) returning id"
        );
        let id: Uuid = sqlx::query_scalar(&sql)
            .bind(new.site_id)
            .bind(&email)
            .bind(&name)
            .bind(&password_hash)
            .bind(&all_roles)
            .bind(status)
            .fetch_one(&mut *tx)
            .await?;

        let verify_token = if requires_verification {
            let (token, digest) = fresh_token();
            sqlx::query(
                "insert into cms_member_tokens (member_id, kind, token_hash, expires_at) \
                 values ($1, 'verify', $2, now() + make_interval(hours => $3::int))",
            )
            .bind(id)
            .bind(&digest)
            .bind(VERIFY_TTL_HOURS)
            .execute(&mut *tx)
            .await?;
            Some(token)
        } else {
            None
        };

        tx.commit().await?;
        let member = self.get(new.site_id, id).await?;
        Ok(SignupOutcome {
            can_sign_in: !requires_verification,
            member,
            verify_token,
        })
    }

    /// The panel's "add a member" action: an operator creates an account for somebody.
    ///
    /// Different from [`MemberStore::signup`] in the way that matters — it may say the address
    /// is already taken, because the person asking is an operator who needs to know their click
    /// did not work, and it is behind a session. It also **invites rather than requires**: with
    /// no password the member is created `pending` with a verification token, so the panel's
    /// "Send verification" button has something to work with.
    pub async fn invite(&self, new: NewMember) -> Result<SignupOutcome> {
        let email = validate_email(&new.email)?;
        let name = new.name.as_deref().map(validate_name).transpose()?.flatten();
        let roles = validate_roles(&new.roles)?;
        let settings = self.settings(new.site_id).await?;
        let password_hash = match new.password.as_deref() {
            Some(password) => Some(hash_password(password).await?),
            None => None,
        };

        let mut all_roles = roles;
        for role in &settings.default_roles {
            if !all_roles.iter().any(|held| held == role) {
                all_roles.push(role.clone());
            }
        }
        all_roles.sort();

        let mut tx = self.pool.begin().await?;
        let existing: Option<Uuid> =
            sqlx::query_scalar("select id from cms_members where site_id = $1 and lower(email) = $2 for update")
                .bind(new.site_id)
                .bind(&email)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some() {
            tx.rollback().await?;
            return Err(ContentError::MemberEmailTaken(email));
        }

        // A member created by an operator is `verified` when they arrive with a password — the
        // operator is vouching for the address — and `pending` when they do not, because there
        // is no proof yet and a verification link is how one is asked for.
        let status = if password_hash.is_some() { "verified" } else { "pending" };
        let sql = format!(
            "insert into cms_members (site_id, email, name, password_hash, roles, status, \
             verified_at) values ($1, $2, $3, $4, $5, $6, \
             case when $6 = 'verified' then now() else null end) returning id"
        );
        let id: Uuid = sqlx::query_scalar(&sql)
            .bind(new.site_id)
            .bind(&email)
            .bind(&name)
            .bind(&password_hash)
            .bind(&all_roles)
            .bind(status)
            .fetch_one(&mut *tx)
            .await?;

        let verify_token = if password_hash.is_none() {
            let (token, digest) = fresh_token();
            sqlx::query(
                "insert into cms_member_tokens (member_id, kind, token_hash, expires_at) \
                 values ($1, 'verify', $2, now() + make_interval(hours => $3::int))",
            )
            .bind(id)
            .bind(&digest)
            .bind(VERIFY_TTL_HOURS)
            .execute(&mut *tx)
            .await?;
            Some(token)
        } else {
            None
        };

        tx.commit().await?;
        let member = self.get(new.site_id, id).await?;
        Ok(SignupOutcome {
            can_sign_in: password_hash.is_some(),
            member,
            verify_token,
        })
    }

    /// A verification or reset token is clicked.
    ///
    /// `used_at` and `expires_at` are checked in the same transaction that sets them, with the
    /// row held `for update`, so two clicks landing at once cannot both win. The second answers
    /// `applied: false` rather than an error, so a forwarded link learns nothing.
    pub async fn consume_token(
        &self,
        site_id: Uuid,
        token: &str,
        kind: &str,
        new_password: Option<&str>,
    ) -> Result<TokenOutcome> {
        if !matches!(kind, "verify" | "reset") {
            return Err(ContentError::InvalidToken);
        }
        let hash = hash_token(token);
        let mut tx = self.pool.begin().await?;

        let row: Option<(Uuid, String, Option<OffsetDateTime>, Option<OffsetDateTime>)> =
            sqlx::query_as(
                "select member_id, kind, expires_at, used_at from cms_member_tokens \
                 where token_hash = $1 for update",
            )
                .bind(&hash)
                .fetch_optional(&mut *tx)
                .await?;

        // Unknown, of the wrong kind, and expired are ONE refusal to the caller. The reason is
        // available to the operator in the audit log and is deliberately not in the response.
        let Some((member_id, row_kind, expires_at, used_at)) = row.filter(|(id, k, _, _)| {
            *k == kind && *id != Uuid::nil()
        }) else {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                applied: false,
                member: None,
                status: "unknown".to_string(),
                message: None,
            });
        };

        if used_at.is_some() {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                applied: false,
                member: None,
                status: "used".to_string(),
                message: Some("that link has already been used".to_string()),
            });
        }
        if expires_at.is_some_and(|at| at <= OffsetDateTime::now_utc()) {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                applied: false,
                member: None,
                status: "expired".to_string(),
                message: Some("that link has expired — ask for a new one".to_string()),
            });
        }

        // The member must belong to THIS site. A token is unguessable, so a leaked one from site
        // A must still not verify a member of site B.
        let member = match self.get_in(&mut tx, site_id, member_id).await? {
            Some(member) => member,
            None => {
                tx.rollback().await?;
                return Ok(TokenOutcome {
                    applied: false,
                    member: None,
                    status: "unknown".to_string(),
                    message: None,
                });
            }
        };

        if row_kind == "verify" {
            if member.status == "blocked" {
                tx.rollback().await?;
                return Ok(TokenOutcome {
                    applied: false,
                    member: Some(member),
                    status: "blocked".to_string(),
                    message: Some("this account is blocked".to_string()),
                });
            }
            // A member who is already verified keeps the moment they earned. Re-verifying would
            // overwrite the real date with the date a forwarded link was clicked.
            if member.status == "verified" {
                sqlx::query("update cms_member_tokens set used_at = now() where token_hash = $1")
                    .bind(&hash)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(TokenOutcome {
                    applied: false,
                    member: Some(member),
                    status: "verified".to_string(),
                    message: Some("this account is already verified".to_string()),
                });
            }
            sqlx::query(
                "update cms_members set status = 'verified', verified_at = now(), \
                 updated_at = now() where id = $1",
            )
            .bind(member_id)
            .execute(&mut *tx)
            .await?;
        } else {
            // A reset link is the only path that sets a password, so the password is validated
            // and hashed here rather than in the route: a route that hashed it would have to be
            // trusted with the plaintext a second time.
            let Some(password) = new_password else {
                tx.rollback().await?;
                return Err(ContentError::InvalidMember(
                    "a password is required to finish a reset".to_string(),
                ));
            };
            let hash_value = hash_password(password).await?;
            sqlx::query(
                "update cms_members set password_hash = $1, updated_at = now() where id = $2",
            )
            .bind(&hash_value)
            .bind(member_id)
            .execute(&mut *tx)
            .await?;

            // Every other session of this member dies with the password change. A reset that
            // leaves the old sessions alive is a reset that hands the account back to whoever
            // was using it — which is the situation a reset exists to end.
            sqlx::query("delete from cms_member_sessions where member_id = $1")
                .bind(member_id)
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query("update cms_member_tokens set used_at = now() where token_hash = $1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let member = self.get(site_id, member_id).await?;
        let message = if row_kind == "verify" {
            "your account is verified — you can sign in now"
        } else {
            "your password has been changed — you can sign in now"
        };
        Ok(TokenOutcome {
            applied: true,
            status: member.status.clone(),
            member: Some(member),
            message: Some(message.to_string()),
        })
    }

    /// Mint a fresh verification token for a member, replacing any live one.
    ///
    /// The previous link is **used, not deleted**: a link that was mailed and is now dead should
    /// answer "already used" rather than "unknown", and an owner looking at a bounced second
    /// mail needs to be able to say which of the two mails is the live one.
    pub async fn issue_verify_token(&self, site_id: Uuid, member_id: Uuid) -> Result<String> {
        let member = self.get(site_id, member_id).await?;
        if member.status == "verified" {
            return Err(ContentError::InvalidMember(
                "this member is already verified".to_string(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update cms_member_tokens set used_at = now() \
             where member_id = $1 and kind = 'verify' and used_at is null",
        )
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
        let (token, digest) = fresh_token();
        sqlx::query(
            "insert into cms_member_tokens (member_id, kind, token_hash, expires_at) \
             values ($1, 'verify', $2, now() + make_interval(hours => $3::int))",
        )
        .bind(member_id)
        .bind(&digest)
        .bind(VERIFY_TTL_HOURS)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(token)
    }

    /// Mint a password-reset token. Works for a blocked member too: somebody who locked
    /// themselves out of their own account by being blocked needs the site's operator, and the
    /// operator is the one who clicks this.
    pub async fn issue_reset_token(&self, site_id: Uuid, member_id: Uuid) -> Result<String> {
        self.get(site_id, member_id).await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update cms_member_tokens set used_at = now() \
             where member_id = $1 and kind = 'reset' and used_at is null",
        )
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
        let (token, digest) = fresh_token();
        sqlx::query(
            "insert into cms_member_tokens (member_id, kind, token_hash, expires_at) \
             values ($1, 'reset', $2, now() + make_interval(mins => $3::int))",
        )
        .bind(member_id)
        .bind(&digest)
        .bind(RESET_TTL_MINUTES)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(token)
    }

    /// Mint a reset token by address, for the public "I forgot my password" form.
    ///
    /// Returns `None` for an address nobody holds. The route turns that into the same 202 the
    /// success case gets, because a form that answers "no account here" is a membership
    /// oracle for any address an attacker cares about.
    pub async fn issue_reset_token_by_email(&self, site_id: Uuid, email: &str) -> Result<Option<String>> {
        let Some(member) = self.get_by_email(site_id, email).await? else {
            return Ok(None);
        };
        if member.password_hash.is_none() {
            // An invited address that never claimed the account has no password to reset.
            return Ok(None);
        }
        Ok(Some(self.issue_reset_token(site_id, member.id).await?))
    }

    // -----------------------------------------------------------------------------------------
    // Sign-in and sessions
    // -----------------------------------------------------------------------------------------

    /// A member signs in.
    ///
    /// Four refusals — no such address, wrong password, not yet verified, blocked — and they
    /// are ONE error, [`ContentError::InvalidCredentials`]. A form that says "this address is
    /// not verified yet" is a list of every address on the site.
    ///
    /// An unknown address still **burns an Argon2 verification** against a throwaway hash, for
    /// the same reason: without it the response time says whether the address exists.
    pub async fn signin(
        &self,
        site_id: Uuid,
        email: &str,
        password: &str,
        ip_hint: Option<&str>,
        ua_hint: Option<&str>,
    ) -> Result<SigninOutcome> {
        let email = validate_email(email).unwrap_or_default();
        let member = self.get_credentials(site_id, &email).await?;

        let Some(member) = member.filter(|m| m.password_hash.is_some()) else {
            // Same work, same time, no row touched.
            let _ = omnion_identity::password::dummy_verify(password.to_owned()).await;
            return Err(ContentError::InvalidCredentials);
        };

        let stored = member.password_hash.clone().unwrap_or_default();
        let ok = omnion_identity::verify_password(password.to_owned(), stored)
            .await
            .map_err(|error| ContentError::InvalidMember(error.to_string()))?;
        if !ok {
            return Err(ContentError::InvalidCredentials);
        }
        if member.status != "verified" {
            // Pending and blocked are the same answer to the caller, and the operator sees which
            // one it was in the panel.
            return Err(ContentError::InvalidCredentials);
        }

        let token = Uuid::new_v4().simple().to_string();
        let expires_at = OffsetDateTime::now_utc() + time::Duration::days(SESSION_TTL_DAYS);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into cms_member_sessions (member_id, token_hash, expires_at, ip_hash, ua_hash) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(member.id)
        .bind(hash_token(&token))
        .bind(expires_at)
        .bind(ip_hint.map(hash_hint))
        .bind(ua_hint.map(hash_hint))
        .execute(&mut *tx)
        .await?;
        sqlx::query("update cms_members set last_signin_at = now(), updated_at = now() where id = $1")
            .bind(member.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let member = self.get(site_id, member.id).await?;
        Ok(SigninOutcome {
            member,
            token,
            expires_at,
        })
    }

    /// The member a member cookie belongs to, or `None`.
    ///
    /// Expired sessions are deleted on the way past rather than skipped: a table that only grows
    /// is a table somebody has to prune, and the row is worthless the moment it is found.
    pub async fn session_member(
        &self,
        site_id: Uuid,
        token: &str,
    ) -> Result<Option<Member>> {
        let hash = hash_token(token);
        let row: Option<(Uuid,)> = sqlx::query_as(
            "select member_id from cms_member_sessions \
             where token_hash = $1 and expires_at > now()",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some((member_id,)) = row else {
            return Ok(None);
        };
        // Touched at most once a minute, so a page with forty assets does not write forty rows
        // a second for every member reading it.
        sqlx::query(
            "update cms_member_sessions set last_seen_at = now() \
             where token_hash = $1 and last_seen_at < now() - interval '1 minute'",
        )
        .bind(&hash)
        .execute(&self.pool)
        .await?;
        Ok(self.get(site_id, member_id).await.ok())
    }

    /// Sign out: delete the session. This is what a row buys over a self-contained token.
    pub async fn signout(&self, token: &str) -> Result<bool> {
        let result = sqlx::query("delete from cms_member_sessions where token_hash = $1")
            .bind(hash_token(token))
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Sign out everywhere: every live session of a member.
    pub async fn signout_everywhere(&self, member_id: Uuid) -> Result<u64> {
        let result = sqlx::query("delete from cms_member_sessions where member_id = $1")
            .bind(member_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // -----------------------------------------------------------------------------------------
    // Moderation
    // -----------------------------------------------------------------------------------------

    /// Change a member.
    ///
    /// The one rule worth spelling out: **`status = 'blocked'` takes the member's sessions with
    /// it.** A block that leaves the cookie working is a block an operator believes they applied
    /// and did not, which is the worst of the three outcomes — worse than a refusal they can
    /// see.
    pub async fn patch(
        &self,
        site_id: Uuid,
        id: Uuid,
        patch: &MemberPatch,
    ) -> Result<Member> {
        let current = self.get(site_id, id).await?;

        let name = match &patch.name {
            Some(value) => value.as_deref().map(validate_name).transpose()?.flatten(),
            None => current.name.clone(),
        };
        let status = match &patch.status {
            Some(raw) => validate_status(raw)?,
            None => current.status.clone(),
        };
        let roles = match &patch.roles {
            Some(list) => validate_roles(list)?,
            None => current.roles.clone(),
        };
        let signin_note = match &patch.signin_note {
            Some(value) => value.as_deref().map(validate_note).transpose()?.flatten(),
            None => current.signin_note.clone(),
        };

        // `verified_at` and the status are one fact with a CHECK behind it, so every path that
        // writes one writes both. A member that reaches 'verified' by hand has no moment until
        // this line.
        let verified_at = match (status.as_str(), current.status.as_str()) {
            ("verified", "verified") => current.verified_at,
            ("verified", _) => Some(OffsetDateTime::now_utc()),
            _ => None,
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "update cms_members set name = $1, status = $2, roles = $3, signin_note = $4, \
             verified_at = $5, updated_at = now() where id = $6",
        )
        .bind(&name)
        .bind(&status)
        .bind(&roles)
        .bind(&signin_note)
        .bind(verified_at)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        if status == "blocked" && current.status != "blocked" {
            sqlx::query("delete from cms_member_sessions where member_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            // The live verification link dies with the block, so unblocking does not
            // auto-verify somebody who was mid-signup when the block landed.
            sqlx::query(
                "update cms_member_tokens set used_at = now() \
                 where member_id = $1 and used_at is null",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.get(site_id, id).await
    }

    /// Block a member. A named convenience over `patch` so the route and the panel read as the
    /// action they are, and so the session deletion is not re-implemented at a second call site.
    pub async fn block(&self, site_id: Uuid, id: Uuid, reason: Option<&str>) -> Result<Member> {
        self.patch(
            site_id,
            id,
            &MemberPatch {
                status: Some("blocked".to_string()),
                signin_note: Some(reason.map(str::to_owned)),
                ..MemberPatch::default()
            },
        )
        .await
    }

    /// Verify a member by hand, from the panel. The operator is vouching for the address.
    pub async fn verify(&self, site_id: Uuid, id: Uuid) -> Result<Member> {
        self.patch(
            site_id,
            id,
            &MemberPatch {
                status: Some("verified".to_string()),
                ..MemberPatch::default()
            },
        )
        .await
    }

    /// Delete a member and everything that belongs to them.
    ///
    /// The cascades do the work; the sessions are named explicitly because "delete the member"
    /// that leaves a live cookie behind is a session that still authenticates against a row
    /// that is gone — and [`MemberStore::session_member`] would answer `None` for it, which is
    /// why this is stated rather than assumed.
    pub async fn delete(&self, site_id: Uuid, id: Uuid) -> Result<()> {
        self.get(site_id, id).await?;
        sqlx::query("delete from cms_members where site_id = $1 and id = $2")
            .bind(site_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set a member's password directly, after the route has checked the current one.
    ///
    /// A named method rather than a raw `update` in the route for two reasons: the route should
    /// not hold SQL, and "the password column was written" is a fact this crate should own so
    /// the next writer cannot spell it differently.
    pub async fn set_password(&self, site_id: Uuid, id: Uuid, password: &str) -> Result<()> {
        let hash = hash_password(password).await?;
        self.get(site_id, id).await?;
        sqlx::query("update cms_members set password_hash = $1, updated_at = now() where id = $2")
            .bind(&hash)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The gate on one published page: its visibility, its state and the roles it names.
    ///
    /// `None` for an address that is not a published page here. The query filters on
    /// `status = 'published'` rather than leaving that to the caller, because the public
    /// surface's rule is that a draft and a page that does not exist are the same answer — a
    /// caller that filtered afterwards would have to remember that, and a caller that forgot
    /// would disclose a draft.
    pub async fn page_gate(&self, site_id: Uuid, slug: &str) -> Result<Option<PageGate>> {
        Ok(sqlx::query_as::<_, PageGate>(
            "select visibility, visibility_roles from pages \
             where site_id = $1 and slug = $2 and status = 'published'",
        )
        .bind(site_id)
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// How many members may sign in, for the settings screen's summary line.
    pub async fn verified_count(&self, site_id: Uuid) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(
            "select count(*) from cms_members where site_id = $1 and status = 'verified'",
        )
        .bind(site_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Read a member inside an open transaction, so a token click and its member check commit
    /// or roll back together.
    async fn get_in(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        site_id: Uuid,
        id: Uuid,
    ) -> Result<Option<Member>> {
        let sql = format!(
            "select {MEMBER_COLUMNS} from cms_members where site_id = $1 and id = $2 for update"
        );
        Ok(sqlx::query_as::<_, Member>(&sql)
            .bind(site_id)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?)
    }
}
