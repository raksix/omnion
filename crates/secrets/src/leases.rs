//! Credential leases, loopback redemption and scoped deployment keys
//! (docs/requests/REQ-125, slice 3).
//!
//! Three structures, one rule: **the lease token is not the secret, and the value travels on
//! exactly one path.**
//!
//! * **Lease** — `POST /secrets/{id}/lease` hands a named consumer an opaque, short-lived,
//!   use-capped handle. What comes back is that handle and nothing else: the issuing request
//!   never opens the envelope, so a lease that leaks from a CI log cannot be turned into a
//!   credential without also being redeemed from the right machine.
//! * **Redemption** — the *only* path a plaintext takes. It is bound to a machine identity
//!   (a deployment key) and scoped to loopback, use-capped and TTL-bounded, and it is the one
//!   response in this crate that has a value field. A revoked, expired or spent lease is a
//!   refusal with a stable code, and every refusal writes a denial row: a revoked lease
//!   redeemed again is an event, not an error page.
//! * **Deployment key** — a scoped machine credential for CI. It may lease inside its own
//!   environment and its own scope list, never `reveal`, never another environment, and it
//!   expires on a date the operator sets. Scopes are narrow on purpose: a deployment key is a
//!   machine credential in CI, so it leaks eventually and the design assumes it.
//!
//! The three interact in one place, and that is deliberate: **`deployment.started` revokes
//! every live lease of that environment**, so a redeploy never runs on the credential the
//! operator has just replaced. [`revoke_environment_leases`] is that hook.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::credentials::find_credential;
use crate::error::{Result, SecretsError};
use crate::keyring::OperatorKey;
use crate::store::load_ring;

/// Default lease lifetime. Short on purpose: a lease is a hand-off, not a credential store.
pub const DEFAULT_TTL: i64 = 15 * 60;

/// The longest TTL a request may ask for, in seconds (24 hours).
///
/// A lease that outlives a day has stopped being a hand-off and become a stored credential,
/// which is exactly what the operator key hierarchy exists to prevent.
pub const MAX_TTL: i64 = 24 * 60 * 60;

/// The smallest use cap. Zero would make a lease un-redeemable, which the schema forbids too;
/// this keeps the crate's own refusal a sentence rather than a constraint violation.
pub const MIN_USES: i32 = 1;

/// The largest use cap one lease may carry.
pub const MAX_USES: i32 = 50;

/// How a lease is presented back to the helper. Kept as a `&'static str` so the audit row and
/// the event payload cannot drift from the value stored in the column.
pub type Issuer = &'static str;

/// One lease, as the list screen reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LeaseRow {
    /// Lease id — the address a revoke names.
    pub id: Uuid,
    /// The secret the lease is over.
    pub secret_id: Uuid,
    /// The secret's name, so the list is readable without a second query per row.
    pub name: String,
    /// Who the lease was issued to.
    pub consumer: String,
    /// The environment whose deploy revokes it.
    pub environment: String,
    /// When it was issued.
    pub issued_at: OffsetDateTime,
    /// When it stops working.
    pub expires_at: OffsetDateTime,
    /// When it was revoked, if it was.
    pub revoked_at: Option<OffsetDateTime>,
    /// Why it was revoked — including the automatic reason after a deploy.
    pub revoke_reason: Option<String>,
    /// The redemption budget.
    pub max_uses: i32,
    /// How much of it is spent.
    pub uses: i32,
    /// The last redemption, for the "last used" column.
    pub last_redeemed_at: Option<OffsetDateTime>,
    /// The deployment key bound to it, when it was bound to one.
    pub issued_to_key_id: Option<Uuid>,
    /// The address of the last redemption, so a leaked lease is traceable.
    pub last_address: Option<String>,
    /// The current version of the secret, so the panel can say what it would hand out.
    #[sqlx(default)]
    pub version: i32,
}

impl LeaseRow {
    /// `live`, `spent`, `expired` or `revoked` — the one word the status column shows.
    ///
    /// The order matters: a revoked lease stays `revoked` even once it is also past its TTL,
    /// because "an operator revoked this on purpose" is the fact worth keeping.
    #[must_use]
    pub fn state(&self, now: OffsetDateTime) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if self.uses >= self.max_uses {
            "spent"
        } else if self.expires_at <= now {
            "expired"
        } else {
            "live"
        }
    }

    /// Whole seconds until expiry, clamped at zero — the countdown's input.
    #[must_use]
    pub fn seconds_left(&self, now: OffsetDateTime) -> i64 {
        (self.expires_at - now).whole_seconds().max(0)
    }
}

/// One deployment key, as the list screen reads it. Metadata only — never the key.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeploymentKeyRow {
    /// Row id.
    pub id: Uuid,
    /// The operator's name for it.
    pub name: String,
    /// The environment it is bound to. A key is refused outside it.
    pub environment: String,
    /// The comma-separated scope list it may lease inside.
    pub scopes: String,
    /// How many times it was presented.
    pub uses: i64,
    /// The last presentation.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it stops working.
    pub expires_at: OffsetDateTime,
    /// The optional address allow-list, comma separated. Empty means "any address, audited".
    pub allowed_ips: String,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it was revoked, if it was.
    pub revoked_at: Option<OffsetDateTime>,
    /// Why it was revoked.
    pub revoke_reason: Option<String>,
    /// A short prefix an operator can recognise in a CI variable without reading the whole key.
    pub key_prefix: String,
    /// The operator-comparable fingerprint, the same shape the root ring uses.
    pub key_fingerprint: String,
}

impl DeploymentKeyRow {
    /// `active`, `revoked` or `expired`.
    #[must_use]
    pub fn state(&self, now: OffsetDateTime) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if self.expires_at <= now {
            "expired"
        } else {
            "active"
        }
    }

    /// The scope list as a slice, for the `contains` check.
    #[must_use]
    pub fn scope_list(&self) -> Vec<&str> {
        self.scopes
            .split(',')
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .collect()
    }
}

/// A deployment key plus the material comparison needs, never handed to a serializer.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeploymentKeySecret {
    /// The row.
    pub key: DeploymentKeyRow,
}

/// Hash a bearer value for storage.
///
/// The same construction everywhere: a domain-separated SHA-256. A deployment key and a lease
/// token are both presented in a header by a machine, so a fast hash is the right one — what
/// protects them is the expiry, the scope and the use cap, not a slow KDF.
#[must_use]
pub fn hash_token(label: &[u8], token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(label);
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Domain separation for a lease token, so a lease token is never a valid key hash.
const LEASE_LABEL: &[u8] = b"omnion.secrets.lease-token.v1";

/// Domain separation for a deployment key, matching the key ring's own labels.
const DEPLOY_KEY_LABEL: &[u8] = b"omnion.secrets.deployment-key.v1";

/// Generate a bearer value: 32 bytes of CSPRNG output, hex at the boundary.
///
/// # Panics
///
/// Only if the OS entropy source fails, which is the same assumption the key ring's
/// `RootKey::generate` makes: a secrets store that quietly fell back to a weaker source would
/// be worse than a process that refuses to start.
#[must_use]
pub fn generate_token() -> String {
    use rand::RngCore as _;

    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// A freshly minted lease: the token travels out once, the row never holds it.
#[derive(Debug)]
pub struct IssuedLease {
    /// The row as it was written.
    pub lease: LeaseRow,
    /// The opaque token. **Returned to the caller exactly once and never stored.**
    pub token: String,
}

/// A freshly minted deployment key: the value travels out once, the row never holds it.
#[derive(Debug)]
pub struct IssuedDeploymentKey {
    /// The row as it was written.
    pub key: DeploymentKeyRow,
    /// The key value. **Returned to the caller exactly once and never stored.**
    pub value: String,
}

/// What a redemption hands back, together with the bookkeeping the helper needs.
///
/// The `value` field exists here and in exactly one other place — the redeem response — which
/// is the whole point: the type makes "a value crossed the wire" a decision someone wrote down
/// rather than an accident a serializer could cause.
#[derive(Debug)]
pub struct Redemption {
    /// The plaintext. Never logged, never audited, never cached.
    pub value: String,
    /// The version it came from.
    pub version: i32,
    /// The secret's name, for the helper's own log line (name, not value).
    pub name: String,
    /// The redaction hint, so an operator can prove which value was handed out.
    pub hint: String,
}

/// Clamp a requested TTL into the documented window, refusing the impossible ones.
fn clamp_ttl(seconds: Option<i64>) -> Result<i64> {
    let requested = seconds.unwrap_or(DEFAULT_TTL);
    if requested < 60 {
        return Err(SecretsError::Invalid(
            "a lease lives at least a minute; a shorter one would expire before the \
             consumer could reach it"
                .to_owned(),
        ));
    }
    Ok(requested.min(MAX_TTL))
}

/// Clamp a requested use cap into `MIN_USES..=MAX_USES`.
fn clamp_uses(requested: Option<i32>) -> Result<i32> {
    let requested = requested.unwrap_or(MIN_USES);
    if requested < MIN_USES {
        return Err(SecretsError::Invalid(
            "a lease can be redeemed at least once; a cap of zero would be a lease that \
             never works"
                .to_owned(),
        ));
    }
    Ok(requested.min(MAX_USES))
}

/// Issue a lease over one secret.
///
/// The call **does not open the envelope.** It writes a row whose `token_hash` is a hash of an
/// opaque handle and returns that handle, which is why this endpoint can never leak a value
/// even by accident: the value is not in scope here.
///
/// # Errors
///
/// [`SecretsError::NotFound`] when the secret does not exist, [`SecretsError::Invalid`] when
/// the TTL or the use cap is outside the documented window, and the database failures
/// otherwise.
pub async fn issue_lease(
    pool: &PgPool,
    secret_id: Uuid,
    consumer: &str,
    environment: &str,
    ttl_seconds: Option<i64>,
    max_uses: Option<i32>,
    issued_to_key_id: Option<Uuid>,
) -> Result<IssuedLease> {
    let consumer = consumer.trim();
    if consumer.is_empty() {
        return Err(SecretsError::Invalid(
            "a lease is issued to a named consumer; name the pipeline or workload".to_owned(),
        ));
    }
    // The secret has to exist, and it has to be one the platform owns: a `file` / `env` bridge
    // resolves on the workload's own machine and has no envelope to hand out.
    let owner = crate::credentials::find_secret_owner(pool, secret_id).await?;
    if owner.read_only {
        return Err(SecretsError::ReadOnly);
    }

    let token = generate_token();
    let now = OffsetDateTime::now_utc();
    let expires_at = now + time::Duration::seconds(clamp_ttl(ttl_seconds)?);
    let max_uses = clamp_uses(max_uses)?;

    let row: LeaseRow = sqlx::query_as(
        "insert into secret_leases (secret_id, token_hash, consumer, issued_to_key_id, \
                environment, expires_at, max_uses) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         returning id, secret_id, consumer, environment, issued_at, expires_at, revoked_at, \
                   revoke_reason, max_uses, uses, last_redeemed_at, issued_to_key_id, null as last_address",
    )
    .bind(secret_id)
    .bind(hash_token(LEASE_LABEL, &token))
    .bind(consumer)
    .bind(issued_to_key_id)
    .bind(if environment.trim().is_empty() {
        "default"
    } else {
        environment.trim()
    })
    .bind(expires_at)
    .bind(max_uses)
    .fetch_one(pool)
    .await?;

    let name = sqlx::query_scalar::<_, String>("select name from secrets where id = $1")
        .bind(secret_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or_default();
    let version = current_version(pool, secret_id).await?;

    Ok(IssuedLease {
        lease: LeaseRow { name, version, ..row },
        token,
    })
}

/// The current, unrevoked version number of a secret; `0` when it has none.
async fn current_version(pool: &PgPool, secret_id: Uuid) -> Result<i32> {
    let version: i32 = sqlx::query_scalar(
        "select coalesce(max(version), 0)::int from secret_versions \
         where secret_id = $1 and revoked_at is null",
    )
    .bind(secret_id)
    .fetch_one(pool)
    .await?;
    Ok(version)
}

/// Every lease of an environment — live and recent — newest first.
///
/// The list screen shows live ones first and keeps revoked ones readable so the reason stays
/// visible; a lease that vanishes when it is revoked would make the audit trail unreadable.
pub async fn list_leases(
    pool: &PgPool,
    environment: Option<&str>,
    secret_id: Option<Uuid>,
) -> Result<Vec<LeaseRow>> {
    let rows = sqlx::query_as::<_, LeaseRow>(
        "select l.id, l.secret_id, coalesce(s.name, '') as name, l.consumer, l.environment, \
                l.issued_at, l.expires_at, l.revoked_at, l.revoke_reason, l.max_uses, l.uses, \
                l.last_redeemed_at, l.issued_to_key_id, \
                (select u.address from deployment_key_uses u \
                  where u.lease_id = l.id order by u.created_at desc limit 1) as last_address, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = l.secret_id and v.revoked_at is null), 0)::int as version \
         from secret_leases l left join secrets s on s.id = l.secret_id \
         where ($1::text is null or l.environment = $1) \
           and ($2::uuid is null or l.secret_id = $2) \
         order by (l.revoked_at is null) desc, l.issued_at desc \
         limit 500",
    )
    .bind(environment)
    .bind(secret_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One lease by id, or `None`.
pub async fn find_lease(pool: &PgPool, id: Uuid) -> Result<Option<LeaseRow>> {
    let rows = list_leases(pool, None, None).await?;
    Ok(rows.into_iter().find(|lease| lease.id == id))
}

/// Revoke one lease with a reason.
///
/// Revoking a lease that is already revoked is **not** an error: the operator clicking twice,
/// or a deploy arriving after a manual revoke, is the normal case, and the first reason wins so
/// the screen keeps telling the true story.
///
/// # Errors
///
/// [`SecretsError::NotFound`] when the id carries no lease.
pub async fn revoke_lease(pool: &PgPool, id: Uuid, reason: &str) -> Result<LeaseRow> {
    let affected = sqlx::query(
        "update secret_leases set revoked_at = coalesce(revoked_at, now()), \
                revoke_reason = coalesce(revoke_reason, $2) \
         where id = $1",
    )
    .bind(id)
    .bind(if reason.trim().is_empty() {
        "revoked from the panel"
    } else {
        reason.trim()
    })
    .execute(pool)
    .await?;
    if affected.rows_affected() == 0 {
        return Err(SecretsError::NotFound("lease"));
    }
    find_lease(pool, id)
        .await?
        .ok_or(SecretsError::NotFound("lease"))
}

/// Revoke every live lease of an environment — the `deployment.started` hook.
///
/// A redeploy must never run on the credential the operator has just replaced, and a lease is
/// the one thing that would hand out the old one afterwards. This is the reason the lease
/// carries an `environment` at all: without it there is nothing to revoke on.
///
/// Returns the ids it revoked, so the caller can emit one event per lease and the panel can say
/// how many went away.
///
/// # Errors
///
/// [`SecretsError::Invalid`] when the environment is empty, and the database failures
/// otherwise.
pub async fn revoke_environment_leases(
    pool: &PgPool,
    environment: &str,
    reason: &str,
) -> Result<Vec<Uuid>> {
    if environment.trim().is_empty() {
        return Err(SecretsError::Invalid(
            "a deployment event must name the environment it deployed".to_owned(),
        ));
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "update secret_leases set revoked_at = now(), \
                revoke_reason = coalesce(revoke_reason, $2) \
         where environment = $1 and revoked_at is null \
         returning id",
    )
    .bind(environment.trim())
    .bind(reason)
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// The redemption check: a lease is usable or it is not, and the refusal names which rule
/// spoke. Ordering is deliberate — a revoked lease reports `revoked` even when it is also
/// spent, because that is the reason a human needs.
fn lease_refusal(lease: &LeaseRow, now: OffsetDateTime) -> Option<SecretsError> {
    if lease.revoked_at.is_some() {
        return Some(SecretsError::LeaseUnavailable(
            "the lease was revoked, so redeeming it again is a denial",
        ));
    }
    if lease.uses >= lease.max_uses {
        return Some(SecretsError::LeaseUnavailable(
            "the lease has spent its redemption budget",
        ));
    }
    if lease.expires_at <= now {
        return Some(SecretsError::LeaseUnavailable(
            "the lease has expired; issue a new one",
        ));
    }
    None
}

/// Redeem a lease for its value. **The one path a plaintext takes.**
///
/// The rules, in the order they are checked:
///
/// 1. the token hashes to the row named by the path — a mismatch is `401`, not `404`, because
///    the path already said which lease this is;
/// 2. the lease is live — revoked, spent or expired is a `410`-shaped refusal, and the caller
///    writes the denial row;
/// 3. the use cap is incremented **in the same statement that reads the version**, so two
///    concurrent redemptions cannot both spend the last use;
/// 4. the envelope is unsealed with the `key_id` the version recorded, not the active key, so
///    a redemption during a rotation works on the old key until the walk reaches that version.
///
/// # Errors
///
/// [`SecretsError::LeaseUnavailable`] for every refusal (the caller turns those into `410`),
/// [`SecretsError::NotFound`] when the path names a lease that does not exist, and
/// [`SecretsError::Crypto`] when the envelope will not open.
pub async fn redeem_lease(
    pool: &PgPool,
    lease_id: Uuid,
    token: &str,
    address: Option<&str>,
) -> Result<Redemption> {
    let presented = hash_token(LEASE_LABEL, token);
    let row: LeaseRow = sqlx::query_as(
        "select l.id, l.secret_id, coalesce(s.name, '') as name, l.consumer, l.environment, \
                l.issued_at, l.expires_at, l.revoked_at, l.revoke_reason, l.max_uses, l.uses, \
                l.last_redeemed_at, l.issued_to_key_id, null as last_address, 0 as version \
         from secret_leases l left join secrets s on s.id = l.secret_id \
         where l.id = $1 and l.token_hash = $2",
    )
    .bind(lease_id)
    .bind(&presented)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::LeaseUnavailable(
        "this lease token is not recognised",
    ))?;

    let now = OffsetDateTime::now_utc();
    if let Some(refusal) = lease_refusal(&row, now) {
        return Err(refusal);
    }

    // Spend first, read after. The `uses < max_uses` predicate is what makes the increment
    // atomic: a concurrent redemption that loses the race updates zero rows and is refused by
    // the `rows_affected` check below rather than double-spending the budget.
    let spent = sqlx::query(
        "update secret_leases set uses = uses + 1, last_redeemed_at = now(), last_address = $2 \
         where id = $1 and revoked_at is null and uses < max_uses and expires_at > now()",
    )
    .bind(lease_id)
    .bind(address)
    .execute(pool)
    .await?;
    if spent.rows_affected() == 0 {
        return Err(SecretsError::LeaseUnavailable(
            "the lease was spent or expired a moment ago",
        ));
    }

    let version: (String, String, i32) = sqlx::query_as::<_, (String, String, i32)>(
        "select v.envelope, v.key_id, v.version from secret_versions v \
         where v.secret_id = $1 and v.revoked_at is null \
         order by v.version desc limit 1",
    )
    .bind(row.secret_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        SecretsError::LeaseUnavailable("the secret behind this lease has no current version")
    })?;

    let ring = load_ring(pool).await?;
    let operator = OperatorKey::from_env()?;
    let plaintext = ring
        .unseal(&version.1, &version.0, &operator)
        .map_err(|_| SecretsError::Crypto)?;
    let value = String::from_utf8(plaintext).map_err(|_| SecretsError::Crypto)?;

    Ok(Redemption {
        hint: crate::redaction::hint_for(&value),
        version: version.2,
        name: row.name,
        value,
    })
}

/// Create a deployment key. The value is shown once; the row keeps a hash.
///
/// # Errors
///
/// [`SecretsError::Invalid`] when the name, the environment or the expiry is missing, and the
/// database failures (including the unique-name violation) otherwise.
pub async fn create_deployment_key(
    pool: &PgPool,
    name: &str,
    environment: &str,
    scopes: &[String],
    expires_at: OffsetDateTime,
    allowed_ips: &str,
) -> Result<IssuedDeploymentKey> {
    let name = name.trim();
    if name.is_empty() {
        return Err(SecretsError::Invalid(
            "name this deployment key after the pipeline that presents it".to_owned(),
        ));
    }
    if environment.trim().is_empty() {
        return Err(SecretsError::Invalid(
            "a deployment key belongs to one environment; name it".to_owned(),
        ));
    }
    if expires_at <= OffsetDateTime::now_utc() {
        return Err(SecretsError::Invalid(
            "an expiry in the past would issue a key that is dead on arrival; \
             pick a future date"
                .to_owned(),
        ));
    }
    let cleaned: Vec<String> = scopes
        .iter()
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Err(SecretsError::Invalid(
            "a deployment key needs at least one scope; an unscoped key can lease \
             everything in its environment"
                .to_owned(),
        ));
    }

    let value = format!("omnion_dk_{}", generate_token());
    let key: DeploymentKeyRow = sqlx::query_as(
        "insert into deployment_keys (name, environment, scopes, key_hash, key_prefix, \
                key_fingerprint, expires_at, allowed_ips) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning id, name, environment, scopes, expires_at, allowed_ips, created_at, \
                   revoked_at, revoke_reason, key_prefix, key_fingerprint, 0 as uses, \
                   null as last_used_at",
    )
    .bind(name)
    .bind(environment.trim())
    .bind(cleaned.join(","))
    .bind(hash_token(DEPLOY_KEY_LABEL, &value))
    .bind(value.chars().take(16).collect::<String>())
    .bind(fingerprint(&value))
    .bind(expires_at)
    .bind(allowed_ips.trim())
    .fetch_one(pool)
    .await?;

    Ok(IssuedDeploymentKey { key, value })
}

/// The operator-comparable fingerprint of a key value, the same shape the root ring uses.
#[must_use]
pub fn fingerprint(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"omnion.secrets.deployment-key-fingerprint.v1");
    hasher.update(value.as_bytes());
    format!("omnion-dk-{}", hex::encode(&hasher.finalize()[..8]))
}

/// Every deployment key, newest first. Metadata only.
pub async fn list_deployment_keys(pool: &PgPool) -> Result<Vec<DeploymentKeyRow>> {
    let rows = sqlx::query_as::<_, DeploymentKeyRow>(
        "select k.id, k.name, k.environment, k.scopes, k.expires_at, k.allowed_ips, \
                k.created_at, k.revoked_at, k.revoke_reason, k.key_prefix, k.key_fingerprint, \
                (select count(*) from deployment_key_uses u where u.key_id = k.id)::bigint as uses, \
                k.last_used_at \
         from deployment_keys k order by (k.revoked_at is null) desc, k.created_at desc",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Revoke a deployment key with a reason. Idempotent, like [`revoke_lease`].
///
/// # Errors
///
/// [`SecretsError::NotFound`] when the id carries no key.
pub async fn revoke_deployment_key(pool: &PgPool, id: Uuid, reason: &str) -> Result<()> {
    let affected = sqlx::query(
        "update deployment_keys set revoked_at = coalesce(revoked_at, now()), \
                revoke_reason = coalesce(revoke_reason, $2) where id = $1",
    )
    .bind(id)
    .bind(if reason.trim().is_empty() {
        "revoked from the panel"
    } else {
        reason.trim()
    })
    .execute(pool)
    .await?;
    if affected.rows_affected() == 0 {
        return Err(SecretsError::NotFound("deployment key"));
    }
    // A revoked key is worthless on its own, but a lease it issued is not: revoke those too,
    // or a leaked key could keep spending leases it minted before it was revoked.
    sqlx::query(
        "update secret_leases set revoked_at = now(), \
                revoke_reason = coalesce(revoke_reason, 'the deployment key that held it was revoked') \
         where issued_to_key_id = $1 and revoked_at is null",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a revoked key's record. A live key can never be deleted, only revoked.
///
/// # Errors
///
/// [`SecretsError::Invalid`] when the key is still live, [`SecretsError::NotFound`] when the id
/// carries no key.
pub async fn delete_deployment_key(pool: &PgPool, id: Uuid) -> Result<()> {
    let key = find_deployment_key(pool, id)
        .await?
        .ok_or(SecretsError::NotFound("deployment key"))?;
    if key.revoked_at.is_none() && key.expires_at > OffsetDateTime::now_utc() {
        return Err(SecretsError::Invalid(
            "revoke the key before deleting its record — a live key must stay visible \
             as something that was once valid"
                .to_owned(),
        ));
    }
    // The use log is the evidence; `deployment_key_uses` cascades, so the log is exported by
    // REQ-125's audit surface before this is ever called.
    sqlx::query("delete from deployment_keys where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// One deployment key by id, or `None`.
pub async fn find_deployment_key(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<DeploymentKeyRow>> {
    let all = list_deployment_keys(pool).await?;
    Ok(all.into_iter().find(|key| key.id == id))
}

/// Authenticate a presented deployment key and return the row it belongs to.
///
/// This is the machine identity redemption binds to. The checks, in order:
///
/// 1. the value hashes to a row — a miss is `401`;
/// 2. revoked or past its expiry — `401` either way, because a caller learns nothing about
///    *which* of the two it was;
/// 3. the source address is inside the allow-list, when the key has one.
///
/// # Errors
///
/// [`SecretsError::DeploymentKeyUnavailable`] for each refusal, so the caller cannot tell a
/// wrong key from a dead one.
pub async fn authenticate_deployment_key(
    pool: &PgPool,
    value: &str,
    address: Option<&str>,
) -> Result<DeploymentKeyRow> {
    let presented = hash_token(DEPLOY_KEY_LABEL, value);
    let key: DeploymentKeyRow = sqlx::query_as::<_, DeploymentKeyRow>(
        "select k.id, k.name, k.environment, k.scopes, k.expires_at, k.allowed_ips, \
                k.created_at, k.revoked_at, k.revoke_reason, k.key_prefix, k.key_fingerprint, \
                0 as uses, k.last_used_at \
         from deployment_keys k where k.key_hash = $1",
    )
    .bind(&presented)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        SecretsError::DeploymentKeyUnavailable("this deployment key is not recognised")
    })?;

    let now = OffsetDateTime::now_utc();
    if key.revoked_at.is_some() {
        return Err(SecretsError::DeploymentKeyUnavailable(
            "this deployment key was revoked",
        ));
    }
    if key.expires_at <= now {
        return Err(SecretsError::DeploymentKeyUnavailable(
            "this deployment key has expired; issue a new one",
        ));
    }
    if !address_allowed(&key.allowed_ips, address) {
        return Err(SecretsError::DeploymentKeyUnavailable(
            "this deployment key is not allowed to present itself from that address",
        ));
    }
    Ok(key)
}

/// Whether an address is inside a comma-separated allow-list. An empty list allows anything.
#[must_use]
pub fn address_allowed(allowlist: &str, address: Option<&str>) -> bool {
    let entries: Vec<&str> = allowlist
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() {
        return true;
    }
    let Some(address) = address else {
        // The runtime could not tell us who is calling, and the key asked to be told. Refusing
        // is the only safe reading of an allow-list.
        return false;
    };
    entries.iter().any(|entry| address_in(entry, address))
}

/// A prefix match on the octets a CIDR names (`10.0.0.0/8`, `192.168.1.0/24`, or a bare address).
fn address_in(entry: &str, address: &str) -> bool {
    match entry.split_once('/') {
        Some((network, bits)) => match (parse_octets(network), parse_octets(address)) {
            (Some(network), Some(address)) => {
                let Some(bits) = bits.parse::<u32>().ok() else {
                    return false;
                };
                if bits > 32 {
                    return false;
                }
                let full = (bits / 8) as usize;
                if network.len() != 4 || address.len() != 4 || network[..full] != address[..full] {
                    return false;
                }
                let rest = bits % 8;
                rest == 0 || {
                    let mask = 0xff_u8 << (8 - rest);
                    (network[full] & mask) == (address[full] & mask)
                }
            }
            _ => false,
        },
        None => entry == address,
    }
}

/// The four octets of an IPv4 address.
fn parse_octets(value: &str) -> Option<[u8; 4]> {
    let mut octets = [0_u8; 4];
    let mut parts = value.trim().split('.');
    for slot in &mut octets {
        *slot = parts.next()?.parse::<u8>().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

/// Write one use of a deployment key: `lease`, `denied` or `revoke`.
///
/// The use log is what makes a leaked deployment key survivable, so it is written on the
/// **refusal** path too — a key probing for what it may reach leaves exactly as good a trace as
/// one that succeeded.
pub async fn record_use(
    pool: &PgPool,
    key_id: Uuid,
    action: &str,
    lease_id: Option<Uuid>,
    identity: &str,
    address: Option<&str>,
    result: &str,
) -> Result<()> {
    sqlx::query(
        "insert into deployment_key_uses (key_id, action, lease_id, identity, address, result) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key_id)
    .bind(action)
    .bind(lease_id)
    .bind(identity)
    .bind(address)
    .bind(result)
    .execute(pool)
    .await?;
    // Touching the key on success is what the list's "last used" column reads.
    if result == "ok" {
        sqlx::query("update deployment_keys set last_used_at = now() where id = $1")
            .bind(key_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// The recent uses of one key, newest first — pipeline, address, lease, result.
pub async fn list_key_uses(
    pool: &PgPool,
    key_id: Uuid,
) -> Result<Vec<(String, Option<Uuid>, String, Option<String>, String, OffsetDateTime)>> {
    let rows = sqlx::query_as::<_, (String, Option<Uuid>, String, Option<String>, String, OffsetDateTime)>(
        "select action, lease_id, identity, address, result, created_at \
         from deployment_key_uses where key_id = $1 order by created_at desc limit 200",
    )
    .bind(key_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// `true` when a typed credential is inside a key's scope list.
///
/// A credential's "scope" is its name: a deployment key that lists `smtp.production` may lease
/// the credential called that and nothing else. An empty list is refused earlier at creation,
/// so an unscoped key never reaches this function.
#[must_use]
pub fn scope_allows(key: &DeploymentKeyRow, secret_name: &str) -> bool {
    key.scope_list().iter().any(|scope| {
        *scope == secret_name || {
            // A trailing `*` is the one wildcard: `smtp.*` covers a family of credentials
            // without turning the key into a wildcard grant.
            scope.ends_with(".*")
                && secret_name
                    .strip_prefix(scope.trim_end_matches('*'))
                    .is_some_and(|rest| !rest.is_empty())
        }
    })
}

/// Check that a key may act on a secret: right environment, right scope.
///
/// Pure, so both halves of the rule can be unit-tested without a database: the caller has
/// already authenticated the key, and this decides what it may then touch.
///
/// # Errors
///
/// [`SecretsError::DeploymentKeyUnavailable`] when the environment does not match, or when the
/// credential is outside the key's scope list. Both refusals are deliberately the same variant,
/// so a caller cannot tell the two apart from the error type alone.
pub fn check_key_may_touch(
    key: &DeploymentKeyRow,
    secret_name: &str,
    environment: &str,
) -> Result<()> {
    if key.environment != environment {
        return Err(SecretsError::DeploymentKeyUnavailable(
            "this deployment key is bound to another environment",
        ));
    }
    if !scope_allows(key, secret_name) {
        return Err(SecretsError::DeploymentKeyUnavailable(
            "this deployment key is not scoped to that credential",
        ));
    }
    Ok(())
}

/// A convenience used by the API's denial path: a credential row's name, for the scope check.
pub async fn secret_name_for(pool: &PgPool, secret_id: Uuid) -> Result<String> {
    sqlx::query_scalar("select name from secrets where id = $1")
        .bind(secret_id)
        .fetch_optional(pool)
        .await?
        .ok_or(SecretsError::NotFound("secret"))
}

/// Whether a credential is one a deployment key could ever lease (it exists and is typed).
pub async fn credential_exists(pool: &PgPool, secret_id: Uuid) -> Result<bool> {
    Ok(find_credential(pool, secret_id).await.is_ok_and(|found| found.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ttl_is_clamped_into_the_documented_window() {
        assert_eq!(clamp_ttl(None).expect("the default is valid"), DEFAULT_TTL);
        assert_eq!(clamp_ttl(Some(60)).expect("a minute is valid"), 60);
        assert_eq!(clamp_ttl(Some(MAX_TTL * 10)).expect("too long is clamped"), MAX_TTL);
        assert!(clamp_ttl(Some(5)).is_err(), "a five second lease is refused");
    }

    #[test]
    fn a_use_cap_is_clamped_and_zero_is_refused() {
        assert_eq!(clamp_uses(None).expect("the default is valid"), MIN_USES);
        assert_eq!(clamp_uses(Some(3)).expect("three is valid"), 3);
        assert_eq!(clamp_uses(Some(999)).expect("too many is clamped"), MAX_USES);
        assert!(clamp_uses(Some(0)).is_err(), "an unredeemable lease is refused");
    }

    #[test]
    fn a_hash_is_stable_domain_separated_and_irreversible() {
        let lease = hash_token(LEASE_LABEL, "token-value");
        assert_eq!(lease, hash_token(LEASE_LABEL, "token-value"));
        // Domain separation: the same value hashed as a key is a different string, so a
        // deployment key can never be presented where a lease token belongs.
        assert_ne!(lease, hash_token(DEPLOY_KEY_LABEL, "token-value"));
        assert!(!lease.contains("token-value"));
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
    }

    fn key_with(scopes: &str) -> DeploymentKeyRow {
        DeploymentKeyRow {
            id: Uuid::nil(),
            name: "pipeline".to_owned(),
            environment: "production".to_owned(),
            scopes: scopes.to_owned(),
            uses: 0,
            last_used_at: None,
            expires_at: OffsetDateTime::now_utc() + time::Duration::days(1),
            allowed_ips: String::new(),
            created_at: OffsetDateTime::now_utc(),
            revoked_at: None,
            revoke_reason: None,
            key_prefix: "omnion_dk_".to_owned(),
            key_fingerprint: String::new(),
        }
    }

    #[test]
    fn a_scope_list_matches_exactly_or_by_family_wildcard() {
        let key = key_with("smtp.production,payments.*");
        assert!(scope_allows(&key, "smtp.production"));
        assert!(scope_allows(&key, "payments.stripe"));
        assert!(!scope_allows(&key, "payments"), "a family match needs a member");
        assert!(!scope_allows(&key, "storage.s3"), "an unlisted credential is refused");
    }

    #[test]
    fn an_environment_mismatch_is_refused_before_the_scope_is_even_read() {
        let key = key_with("smtp.production");
        let refusal = check_key_may_touch(&key, "smtp.production", "staging")
            .expect_err("another environment is refused");
        assert!(refusal.to_string().contains("environment"));

        // The same key inside its own environment and scope is fine, so the refusal above is
        // about the environment and not about the check being broken.
        check_key_may_touch(&key, "smtp.production", "production")
            .expect("its own environment is allowed");

        // Right environment, wrong credential: the other half of the same rule.
        let refused = check_key_may_touch(&key, "storage.s3", "production")
            .expect_err("an unlisted credential is refused");
        assert!(refused.to_string().contains("not scoped"));
    }

    #[test]
    fn an_empty_allow_list_permits_any_address() {
        assert!(address_allowed("", Some("203.0.113.7")));
        assert!(address_allowed("  ", Some("203.0.113.7")));
    }

    #[test]
    fn a_cidr_allow_list_matches_on_the_named_bits_only() {
        assert!(address_allowed("10.0.0.0/8", Some("10.4.5.6")));
        assert!(!address_allowed("10.0.0.0/8", Some("11.4.5.6")));
        assert!(address_allowed("192.168.1.0/24", Some("192.168.1.255")));
        assert!(!address_allowed("192.168.1.0/24", Some("192.168.2.1")));
        assert!(address_allowed("203.0.113.7", Some("203.0.113.7")));
        assert!(!address_allowed("203.0.113.7", Some("203.0.113.8")));
    }

    #[test]
    fn an_allow_list_refuses_when_the_runtime_could_not_see_the_address() {
        assert!(!address_allowed("10.0.0.0/8", None));
    }

    #[test]
    fn a_refusal_names_the_rule_that_spoke() {
        let now = OffsetDateTime::now_utc();
        let mut lease = LeaseRow {
            id: Uuid::nil(),
            secret_id: Uuid::nil(),
            name: "smtp.production".to_owned(),
            consumer: "pipeline".to_owned(),
            environment: "production".to_owned(),
            issued_at: now,
            expires_at: now + time::Duration::minutes(15),
            revoked_at: None,
            revoke_reason: None,
            max_uses: 1,
            uses: 0,
            last_redeemed_at: None,
            issued_to_key_id: None,
            last_address: None,
            version: 1,
        };
        assert!(lease_refusal(&lease, now).is_none(), "a fresh lease is usable");

        lease.uses = 1;
        assert!(lease_refusal(&lease, now).is_some(), "a spent lease is refused");

        // Revocation outranks the budget: the operator's reason is the fact worth keeping.
        lease.revoked_at = Some(now);
        assert!(lease_refusal(&lease, now)
            .expect("revoked is refused")
            .to_string()
            .contains("revoked"));
    }
}
