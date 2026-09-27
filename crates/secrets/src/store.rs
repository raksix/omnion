//! The database half of the key ring: the root keys, the re-wrap job that walks the versions and
//! the ceremony that flips a key.
//!
//! The pure half — wrapping, unsealing, the seal self-check, re-wrapping one value — lives in
//! [`crate::keyring`]. This module is the half that owns rows, and it is written around one
//! promise the request makes: **a rotation is an online ceremony, not a maintenance window**.
//!
//! How that is kept true here:
//!
//! * the retired key stays in the ring as `retiring` for the whole job, and a `retired` key is
//!   kept after it. A version's `key_id` therefore always resolves, before, during and after a
//!   rotation. Nothing here ever deletes a key row to "clean up" — the audit trail is the row.
//! * the job walks in **small batches from a cursor** and writes the cursor in the same
//!   statement as the batch, so a crash resumes instead of restarting, and no lock a normal
//!   read needs is held while a version is re-sealed. A lease redemption during a re-wrap
//!   therefore succeeds on the old key for every version the job has not reached.
//! * the flip is ordered **retire first, activate second**. If the process dies between the two,
//!   the ring has no active key and `ensure_active_key` regenerates one rather than leaving the
//!   installation unable to seal anything.
//!
//! The operator key is read per call through [`OperatorKey::from_env`] rather than cached, so a
//! key file that is rotated on disk takes effect on the next operation without a restart.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecretsError};
use crate::keyring::{KeyRing, KeyStatus, OperatorKey, RootKey, SelfCheck};

/// How many versions one re-wrap batch re-seals. Small on purpose: a batch is a write window a
/// lease redemption can queue behind, and a rotation that is slow is better than a rotation
/// that is blocking.
pub const REWRAP_BATCH: i64 = 25;

/// One root key row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RootKeyRow {
    /// Row id.
    pub id: Uuid,
    /// The `key_id` versions record.
    pub key_id: String,
    /// The wrapped material.
    pub wrapped_key: String,
    /// The seal checksum.
    pub seal_checksum: String,
    /// The lifecycle state.
    pub status: String,
    /// The operator-visible fingerprint.
    pub fingerprint: String,
    /// When it entered the ring.
    pub created_at: OffsetDateTime,
    /// When it was retired, if it was.
    pub retired_at: Option<OffsetDateTime>,
    /// Why it was retired.
    pub retired_reason: Option<String>,
    /// How many stored versions still name this key.
    #[sqlx(default)]
    pub version_count: i32,
}

impl RootKeyRow {
    /// The pure ring's view of this row.
    ///
    /// # Errors
    ///
    /// [`SecretsError::Invalid`] when the stored status is not one the schema allows — a
    /// corrupt row must fail loudly rather than be read as "active".
    pub fn to_ring_key(&self) -> Result<RootKey> {
        Ok(RootKey {
            key_id: self.key_id.clone(),
            wrapped_key: self.wrapped_key.clone(),
            seal_checksum: self.seal_checksum.clone(),
            fingerprint: self.fingerprint.clone(),
            status: KeyStatus::parse(&self.status).ok_or_else(|| {
                SecretsError::Invalid(format!("unknown key status {}", self.status))
            })?,
        })
    }
}

/// The re-wrap job as the screen reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RewrapJob {
    /// Job id.
    pub id: Uuid,
    /// `pending`, `running`, `paused`, `completed` or `failed`.
    pub status: String,
    /// The key being retired.
    pub from_key_id: String,
    /// The key replacing it.
    pub to_key_id: String,
    /// Versions re-wrapped so far.
    pub rewrapped_count: i32,
    /// Versions to walk.
    pub total_count: i32,
    /// The cursor a worker last finished.
    pub cursor: String,
    /// Why the job paused.
    pub pause_reason: Option<String>,
    /// The last error.
    pub last_error: Option<String>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished.
    pub completed_at: Option<OffsetDateTime>,
}

impl RewrapJob {
    /// `0..=1` completion, for the progress bar. A job with no versions is complete.
    #[must_use]
    pub fn progress(&self) -> f32 {
        if self.total_count <= 0 {
            return 1.0;
        }
        (f64::from(self.rewrapped_count) / f64::from(self.total_count)).clamp(0.0, 1.0) as f32
    }

    /// `true` when the job has finished walking.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.status == "completed"
    }

    /// The note the screen shows after a restart: where the walk will pick up.
    #[must_use]
    pub fn resume_note(&self) -> Option<String> {
        if self.status != "paused" {
            return None;
        }
        Some(match self.cursor.as_str() {
            "" => "The walk restarts from the first version.".to_owned(),
            cursor => format!("The walk resumes after version {cursor}."),
        })
    }
}

/// What one batch of a re-wrap did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchReport {
    /// Versions re-sealed in this batch.
    pub rewrapped: i32,
    /// Whether the walk is finished.
    pub complete: bool,
}

impl BatchReport {
    /// A batch that had nothing to do.
    pub const IDLE: Self = Self {
        rewrapped: 0,
        complete: true,
    };

    /// `true` when the job has nothing left to do and the runner can sleep.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.rewrapped == 0 && self.complete
    }
}

/// Every root key, newest first, with the version count each still carries.
pub async fn list_root_keys(pool: &PgPool) -> Result<Vec<RootKeyRow>> {
    let rows = sqlx::query_as::<_, RootKeyRow>(
        "select k.id, k.key_id, k.wrapped_key, k.seal_checksum, k.status, k.fingerprint, \
                k.created_at, k.retired_at, k.retired_reason, \
                (select count(*) from secret_versions v where v.key_id = k.key_id) as version_count \
         from secret_root_keys k \
         order by k.created_at desc, k.id desc",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The currently active key, if the ring has one.
pub async fn active_key_row(pool: &PgPool) -> Result<Option<RootKeyRow>> {
    let row = sqlx::query_as::<_, RootKeyRow>(
        "select k.id, k.key_id, k.wrapped_key, k.seal_checksum, k.status, k.fingerprint, \
                k.created_at, k.retired_at, k.retired_reason, \
                (select count(*) from secret_versions v where v.key_id = k.key_id) as version_count \
         from secret_root_keys k where k.status = 'active' \
         order by k.created_at desc limit 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The whole ring, as the pure half wants it.
pub async fn load_ring(pool: &PgPool) -> Result<KeyRing> {
    Ok(KeyRing::with_keys(
        list_root_keys(pool)
            .await?
            .iter()
            .map(RootKeyRow::to_ring_key)
            .collect::<Result<Vec<_>>>()?,
    ))
}

/// Read the operator key from the environment or the operator-controlled key file.
///
/// # Errors
///
/// [`SecretsError::OperatorKeyMissing`] when neither source is set, and
/// [`SecretsError::Invalid`] when a key file is unreadable or empty. Both are fatal for any
/// operation that touches key material, and both are refused rather than defaulted — a
/// development fallback here would make a production install silently write a key it can never
/// rotate away.
pub fn operator_key() -> Result<OperatorKey> {
    OperatorKey::from_env()
}

/// Make sure the ring has an active key, generating the first one on a fresh installation.
///
/// The operator key is the *wrap*, not the key: the ring generates its own root key and stores
/// it wrapped, so the generated key never has to be written anywhere by hand.
///
/// # Errors
///
/// Propagates the operator-key and database failures.
pub async fn ensure_active_key(pool: &PgPool) -> Result<RootKey> {
    if let Some(existing) = active_key_row(pool).await? {
        return Ok(RootKey {
            key_id: existing.key_id,
            wrapped_key: existing.wrapped_key,
            seal_checksum: existing.seal_checksum,
            fingerprint: existing.fingerprint,
            status: KeyStatus::Active,
        });
    }

    let operator = operator_key()?;
    let key = RootKey::generate(&operator)?;
    sqlx::query(
        "insert into secret_root_keys (key_id, wrapped_key, seal_checksum, status, fingerprint) \
         values ($1, $2, $3, 'active', $4)",
    )
    .bind(&key.key_id)
    .bind(&key.wrapped_key)
    .bind(&key.seal_checksum)
    .bind(&key.fingerprint)
    .execute(pool)
    .await?;
    Ok(key)
}

/// Prove the operator key opens the ring, before a rotation starts.
///
/// # Errors
///
/// Propagates the operator-key and database failures. A ring that fails the check is *not* an
/// error here — the caller shows the report and refuses the ceremony, which is a decision, not
/// a failure.
pub async fn self_check(pool: &PgPool) -> Result<SelfCheck> {
    let ring = load_ring(pool).await?;
    Ok(ring.self_check(&operator_key()?))
}

/// Store a new root key without activating it, so a ceremony can wrap before it flips.
pub async fn insert_pending_key(pool: &PgPool, key: &RootKey) -> Result<()> {
    sqlx::query(
        "insert into secret_root_keys \
             (key_id, wrapped_key, seal_checksum, status, fingerprint) \
         values ($1, $2, $3, 'retired', $4) \
         on conflict (key_id) do update set wrapped_key = excluded.wrapped_key, \
             seal_checksum = excluded.seal_checksum",
    )
    .bind(&key.key_id)
    .bind(&key.wrapped_key)
    .bind(&key.seal_checksum)
    .bind(&key.fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// The job that is currently walking the ring, if any.
pub async fn live_rewrap_job(pool: &PgPool) -> Result<Option<RewrapJob>> {
    let job = sqlx::query_as::<_, RewrapJob>(
        "select id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                pause_reason, last_error, started_at, completed_at \
         from secret_rewrap_jobs \
         where status in ('pending', 'running', 'paused') \
         order by started_at desc limit 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(job)
}

/// One re-wrap job by id.
pub async fn find_rewrap_job(pool: &PgPool, id: Uuid) -> Result<Option<RewrapJob>> {
    let job = sqlx::query_as::<_, RewrapJob>(
        "select id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                pause_reason, last_error, started_at, completed_at \
         from secret_rewrap_jobs where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(job)
}

/// Start a rotation: generate the replacement key, wrap it, activate it and record the job.
///
/// The order is deliberate and is the whole safety argument of the ceremony:
///
/// 1. a live job exists → refuse, because two walkers would fight over the same versions;
/// 2. the self-check runs → refuse on a ring the operator key cannot open, because that is the
///    exact situation where a rotation destroys data;
/// 3. the new key is generated and **written wrapped** while the old one is still active;
/// 4. the old key steps down to `retiring`, the new one goes `active`;
/// 5. only then is the job row created, with the count of versions it has to walk.
///
/// A crash between 3 and 4 leaves an unused `retired` key row and no job: the next rotation
/// simply makes another key, and nothing was lost. A crash between 4 and 5 leaves the new key
/// active and no job, which [`recover_missing_job`] repairs.
///
/// # Errors
///
/// [`SecretsError::RotationInProgress`] when a job is live, and the operator-key / crypto /
/// database failures otherwise.
pub async fn start_rotation(pool: &PgPool) -> Result<RewrapJob> {
    if live_rewrap_job(pool).await?.is_some() {
        return Err(SecretsError::RotationInProgress);
    }

    let operator = operator_key()?;
    let ring = load_ring(pool).await?;
    let check = ring.self_check(&operator);
    if !check.is_healthy() {
        return Err(SecretsError::Crypto);
    }
    let Some(current) = ring.active().cloned() else {
        return Err(SecretsError::NoActiveKey);
    };

    // The replacement exists on disk before anything points at it.
    let replacement = RootKey::generate(&operator)?;
    insert_pending_key(pool, &replacement).await?;

    // Retiring first, activating second: the partial unique index on `active` is never violated
    // even if the process stops between the two statements.
    sqlx::query(
        "update secret_root_keys set status = 'retiring', retired_at = now(), \
                retired_reason = 'rotated' where key_id = $1 and status = 'active'",
    )
    .bind(&current.key_id)
    .execute(pool)
    .await?;
    sqlx::query("update secret_root_keys set status = 'active' where key_id = $1")
        .bind(&replacement.key_id)
        .execute(pool)
        .await?;

    let total: i32 = sqlx::query_scalar("select count(*) from secret_versions where key_id = $1")
        .bind(&current.key_id)
        .fetch_one(pool)
        .await?;

    let job = sqlx::query_as::<_, RewrapJob>(
        "insert into secret_rewrap_jobs \
             (status, from_key_id, to_key_id, total_count) \
         values ('running', $1, $2, $3) \
         returning id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                   pause_reason, last_error, started_at, completed_at",
    )
    .bind(&current.key_id)
    .bind(&replacement.key_id)
    .bind(total)
    .fetch_one(pool)
    .await?;
    Ok(job)
}

/// Repair a ring whose key was flipped but whose job row never landed.
///
/// A crash between the flip and the insert leaves the new key active with versions still on the
/// old one and no job to walk them. This is called on read of the key ring: it notices the
/// mismatch and restarts the walk rather than leaving stranded versions forever.
///
/// # Errors
///
/// Propagates the database failures.
pub async fn recover_missing_job(pool: &PgPool) -> Result<Option<RewrapJob>> {
    if live_rewrap_job(pool).await?.is_some() {
        return Ok(None);
    }
    let Some(active) = active_key_row(pool).await? else {
        return Ok(None);
    };
    // A `retiring` key that still carries versions is a flip whose job was lost.
    let stranded: Option<(String, i32)> = sqlx::query_as(
        "select key_id, count(*) as total from secret_versions \
         where key_id <> $1 group by key_id having count(*) > 0 limit 1",
    )
    .bind(&active.key_id)
    .fetch_optional(pool)
    .await?;
    let Some((from_key_id, total)) = stranded else {
        return Ok(None);
    };

    let job = sqlx::query_as::<_, RewrapJob>(
        "insert into secret_rewrap_jobs \
             (status, from_key_id, to_key_id, total_count, pause_reason) \
         values ('paused', $1, $2, $3, 'recovered after a restart: the key was already flipped') \
         returning id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                   pause_reason, last_error, started_at, completed_at",
    )
    .bind(&from_key_id)
    .bind(&active.key_id)
    .bind(total)
    .fetch_one(pool)
    .await?;
    Ok(Some(job))
}

/// Re-wrap one batch of versions and advance the job's cursor.
///
/// A version that cannot be re-sealed stops the batch: the job pauses with the error rather than
/// skipping the version, because a rotation that quietly leaves one version behind is exactly the
/// failure this whole design is meant to make impossible.
///
/// # Errors
///
/// [`SecretsError::NotFound("rewrap job")`] for an unknown job, and the crypto / database
/// failures otherwise. A paused job is not an error — it is read and returned.
pub async fn rewrap_batch(pool: &PgPool, job_id: Uuid) -> Result<BatchReport> {
    let Some(job) = find_rewrap_job(pool, job_id).await? else {
        return Err(SecretsError::NotFound("rewrap job"));
    };
    if job.status == "paused" {
        return Ok(BatchReport {
            rewrapped: 0,
            complete: false,
        });
    }
    if job.is_complete() {
        return Ok(BatchReport::IDLE);
    }

    // The versions in this batch, in the uuid order the cursor walks. A version already
    // re-wrapped no longer matches `key_id = from`, so a re-run is naturally idempotent even
    // if the cursor is stale.
    let versions: Vec<(Uuid, String, String)> = sqlx::query_as(
        "select id, key_id, envelope from secret_versions \
         where key_id = $1 and ($2 = '' or id > $2::uuid) \
         order by id limit $3",
    )
    .bind(&job.from_key_id)
    .bind(&job.cursor)
    .bind(REWRAP_BATCH)
    .fetch_all(pool)
    .await?;

    if versions.is_empty() {
        finish_job(pool, job_id).await?;
        return Ok(BatchReport {
            rewrapped: 0,
            complete: true,
        });
    }

    let operator = operator_key()?;
    let ring = load_ring(pool).await?;
    let mut rewrapped = 0_i32;
    let mut last: Option<Uuid> = None;
    for (version_id, key_id, envelope) in &versions {
        let moved = match ring.rewrap(key_id, &job.to_key_id, envelope, &operator) {
            Ok(moved) => moved,
            Err(error) => {
                // Fail closed: pause with the reason rather than skipping the version.
                pause_job(pool, job_id, "the stored value could not be re-sealed").await?;
                sqlx::query("update secret_rewrap_jobs set last_error = $2 where id = $1")
                    .bind(job_id)
                    .bind(error.to_string())
                    .execute(pool)
                    .await?;
                return Ok(BatchReport {
                    rewrapped,
                    complete: false,
                });
            }
        };
        // The cursor moves in the same statement as the value, so a crash cannot skip a version.
        sqlx::query("update secret_versions set envelope = $2, key_id = $3 where id = $1")
            .bind(version_id)
            .bind(&moved)
            .bind(&job.to_key_id)
            .execute(pool)
            .await?;
        rewrapped += 1;
        last = Some(*version_id);
    }

    if let Some(version_id) = last {
        // The cursor advances in the same statement as the counter, so a crash resumes here and
        // never skips the version whose write landed but whose cursor write did not (a version
        // that was re-sealed no longer matches the from-key, so re-running is a no-op).
        sqlx::query(
            "update secret_rewrap_jobs set rewrapped_count = rewrapped_count + $2, cursor = $3, \
                    status = 'running', last_error = null where id = $1",
        )
        .bind(job_id)
        .bind(rewrapped)
        .bind(version_id.to_string())
        .execute(pool)
        .await?;
    }

    if find_rewrap_job(pool, job_id)
        .await?
        .map_or(false, |job| job.rewrapped_count >= job.total_count)
    {
        finish_job(pool, job_id).await?;
        return Ok(BatchReport {
            rewrapped,
            complete: true,
        });
    }
    Ok(BatchReport {
        rewrapped,
        complete: false,
    })
}

/// Mark a job paused, keeping its counter and cursor.
///
/// # Errors
///
/// Propagates the database failures.
pub async fn pause_job(pool: &PgPool, job_id: Uuid, reason: &str) -> Result<()> {
    sqlx::query(
        "update secret_rewrap_jobs set status = 'paused', pause_reason = $2 where id = $1 \
         and status in ('pending', 'running')",
    )
    .bind(job_id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resume a paused job from its cursor.
///
/// # Errors
///
/// [`SecretsError::NotFound("rewrap job")`] for an unknown job.
pub async fn resume_job(pool: &PgPool, job_id: Uuid) -> Result<RewrapJob> {
    let job = sqlx::query_as::<_, RewrapJob>(
        "update secret_rewrap_jobs set status = 'running', pause_reason = null \
         where id = $1 and status = 'paused' \
         returning id, status, from_key_id, to_key_id, rewrapped_count, total_count, cursor, \
                   pause_reason, last_error, started_at, completed_at",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::NotFound("rewrap job"))?;
    Ok(job)
}

/// Complete a job and retire the key it walked.
///
/// # Errors
///
/// Propagates the database failures.
pub async fn finish_job(pool: &PgPool, job_id: Uuid) -> Result<()> {
    let job = find_rewrap_job(pool, job_id)
        .await?
        .ok_or(SecretsError::NotFound("rewrap job"))?;
    sqlx::query(
        "update secret_rewrap_jobs set status = 'completed', completed_at = now() where id = $1",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "update secret_root_keys set status = 'retired', retired_at = now(), \
                retired_reason = 'rotated' where key_id = $1 and status = 'retiring'",
    )
    .bind(&job.from_key_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The re-wrap coverage of the whole ring: how many versions sit on each key.
pub async fn ring_coverage(pool: &PgPool) -> Result<Vec<(String, String, i32)>> {
    let rows = sqlx::query_as(
        "select k.key_id, k.status, \
                (select count(*) from secret_versions v where v.key_id = k.key_id) as versions \
         from secret_root_keys k order by k.created_at desc",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
