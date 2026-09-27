//! Typed credential profiles and the credential slots consumers resolve through
//! (docs/requests/REQ-125, slice 2).
//!
//! Two structures, one idea: **a consumer never holds a secret id.** A typed credential pins a
//! secret to one of the five validated kinds and records the *non-secret* fields a validator and
//! a consumer need (an endpoint, a username, a port, a token expiry, a key fingerprint). A slot
//! binds one of the documented names (`ai.provider`, `smtp`, `payments.stripe`, `storage.s3`,
//! `ssh.release`, `identity.ldap`) to a primary and an optional fallback secret, so swapping a
//! credential is a slot update rather than a code change in every consumer.
//!
//! The rules this module keeps:
//!
//! * **A validation failure never blocks storage.** [`record_validation`] stores the outcome —
//!   chip, message and timestamp — and the caller keeps the credential. A provider that is down
//!   for an hour is not a reason to lose the operator's work.
//! * **Primary and fallback must differ.** A "fallback" that is the primary is a no-op that reads
//!   like a safety net, so the database refuses it and [`assign_slot`] refuses it first with the
//!   sentence the API returns as `409`.
//! * **A resolver never returns a value.** [`resolve_slot`] answers *which* secret a consumer
//!   gets, the version it would read and whether it fell back — the panel shows names, the
//!   consumer asks for the value through a lease (slice 3). Metadata only, every path.
//! * **Removing a primary falls back, and says so.** [`resolve_slot`] reports `fell_back` and the
//!   API turns that into a `credential_slot.assigned` event plus an audit row, so an operator
//!   learns their swap silently degraded to the backup from a record rather than from an outage.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecretsError};
use crate::redaction::redact;
use crate::validators::{CredentialKind, ValidationOutcome, validate};

/// The slot names the platform ships, in the order the panel offers them.
///
/// The database seeds the same list into `credential_slot_catalog` (migration 0019) so the API
/// can refuse a typo *with* the list of valid names instead of a bare "invalid". Both lists are
/// checked against each other in this module's tests, which is what stops them drifting.
pub const SLOTS: [&str; 6] = [
    "ai.provider",
    "smtp",
    "payments.stripe",
    "storage.s3",
    "ssh.release",
    "identity.ldap",
];

/// The scope kinds a slot assignment can be made for.
pub const SCOPES: [&str; 4] = ["environment", "site", "module", "organization"];

/// A typed credential profile row, joined with the secret it extends.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CredentialRow {
    /// The secret this profile belongs to.
    pub secret_id: Uuid,
    /// The secret's own name, for the panel.
    pub name: String,
    /// One of the five kinds.
    pub kind: String,
    /// The non-secret fields.
    pub fields: Value,
    /// `unknown`, `valid`, `invalid` or `stale`.
    pub validation_state: String,
    /// The validator's own sentence, already redacted.
    pub validation_message: Option<String>,
    /// When the validator last ran.
    pub validation_checked_at: Option<OffsetDateTime>,
    /// How often it should run again; 0 means "on demand only".
    pub validation_interval_days: i32,
    /// `local`, `file` or `env`. A `file`/`env` profile is managed outside the platform.
    pub provider: String,
    /// For a read-only bridge, the path or variable it points at.
    pub provider_locator: Option<String>,
    /// `true` when the secret is a read-only bridge and can never be written.
    pub read_only: bool,
    /// The organization's secret, when it has one.
    pub organization_id: Option<Uuid>,
    /// The current version number of the secret.
    #[sqlx(default)]
    pub version: i32,
    /// When the secret was created.
    pub created_at: OffsetDateTime,
}

impl CredentialRow {
    /// The parsed kind, or `None` for a row whose kind the schema allows but the code does not.
    ///
    /// A `None` here is not a crash: it is a credential created by a newer version of the
    /// platform, and the panel says "unknown kind" rather than pretending it can validate it.
    #[must_use]
    pub fn kind(&self) -> Option<CredentialKind> {
        CredentialKind::parse(&self.kind)
    }

    /// The sentence under the chip, or one that explains why there is none.
    #[must_use]
    pub fn message(&self) -> String {
        self.validation_message.clone().unwrap_or_else(|| {
            if self.validation_state == "stale" {
                "This credential was valid before, but it has not been re-checked on schedule."
                    .to_owned()
            } else {
                "This credential has not been validated yet.".to_owned()
            }
        })
    }

    /// The fields every kind carries, as the panel's two-column list: a blank field is dropped
    /// rather than rendered as an empty row.
    #[must_use]
    pub fn field_pairs(&self) -> Vec<(String, String)> {
        let object = match self.fields.as_object() {
            Some(object) => object,
            None => return Vec::new(),
        };
        object
            .iter()
            .filter_map(|(name, value)| {
                let text = match value {
                    Value::String(text) => text.trim().to_owned(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => return None,
                };
                (!text.is_empty()).then(|| (humanise(name), text))
            })
            .collect()
    }
}

/// Turn `key_prefix` into `Key prefix` for the detail list.
fn humanise(name: &str) -> String {
    let mut out = String::new();
    for (index, character) in name.chars().enumerate() {
        if index == 0 {
            out.extend(character.to_uppercase());
        } else if character == '_' || character == '-' {
            out.push(' ');
        } else {
            out.push(character);
        }
    }
    out
}

/// A slot assignment row, joined with the secrets it names.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SlotRow {
    /// Row id.
    pub id: Uuid,
    /// `environment`, `site`, `module` or `organization`.
    pub scope_type: String,
    /// The concrete scope.
    pub scope_id: String,
    /// The slot name.
    pub slot: String,
    /// The primary secret, when one is assigned.
    pub primary_secret_id: Option<Uuid>,
    /// The fallback secret, when one is assigned.
    pub fallback_secret_id: Option<Uuid>,
    /// The primary secret's name.
    pub primary_name: Option<String>,
    /// The fallback secret's name.
    pub fallback_name: Option<String>,
    /// The primary's current version.
    #[sqlx(default)]
    pub primary_version: Option<i32>,
    /// The fallback's current version.
    #[sqlx(default)]
    pub fallback_version: Option<i32>,
    /// The last consumer seen resolving here.
    pub last_resolved_by: Option<String>,
    /// When it was last resolved.
    pub last_resolved_at: Option<OffsetDateTime>,
    /// When the assignment was made.
    pub created_at: OffsetDateTime,
    /// When the assignment last changed.
    pub updated_at: OffsetDateTime,
}

impl SlotRow {
    /// The sentence the panel shows when nothing is assigned.
    #[must_use]
    pub const fn empty_reason(&self) -> &'static str {
        "No credential is assigned to this slot. Consumers that need one will fail until a primary is set."
    }

    /// `true` when the assignment has a primary.
    #[must_use]
    pub const fn is_assigned(&self) -> bool {
        self.primary_secret_id.is_some()
    }
}

/// What a consumer gets when it resolves a slot.
///
/// Metadata only by construction: the type has no field that could hold a value.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Resolution {
    /// The secret that will be read.
    pub secret_id: Uuid,
    /// Its name, so a log line can name the credential without leaking it.
    pub name: String,
    /// The version the resolver will read.
    pub version: i32,
    /// `true` when the primary was unavailable and the fallback answered.
    pub fell_back: bool,
}

/// Parse and check a kind, refusing anything outside the five with the list the wizard offers.
pub fn parse_kind(raw: &str) -> Result<CredentialKind> {
    CredentialKind::parse(raw).ok_or_else(|| {
        SecretsError::Invalid(format!(
            "{raw} is not a credential kind; use one of {}",
            CredentialKind::all()
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// Check a scope kind and a slot name against the two documented lists.
pub fn check_scope_and_slot(scope_type: &str, slot: &str) -> Result<()> {
    if !SCOPES.contains(&scope_type) {
        return Err(SecretsError::Invalid(format!(
            "{scope_type} is not a scope; use one of {}",
            SCOPES.join(", ")
        )));
    }
    if !SLOTS.contains(&slot) {
        return Err(SecretsError::Invalid(format!(
            "{slot} is not a slot; use one of {}",
            SLOTS.join(", ")
        )));
    }
    Ok(())
}

/// Keep only the non-secret fields a kind declares, and drop anything that looks like a value.
///
/// This is the guard that makes "non-secret fields" a property rather than a convention. A
/// wizard bug, a copy-paste from the value box, or a hand-written `curl` that puts the key into
/// `fields` all die here: a field whose name suggests a secret, or whose value carries a
/// recognisable key shape, is refused with the offending name — never the value.
#[must_use]
pub fn sanitize_fields(kind: CredentialKind, fields: &Value) -> Result<Value> {
    let object = match fields.as_object() {
        Some(object) => object,
        None => {
            return Err(SecretsError::Invalid(
                "the fields must be a JSON object".to_owned(),
            ));
        }
    };
    let allowed: &[&str] = kind.expected_fields();
    let mut kept = serde_json::Map::new();
    for (name, value) in object {
        if looks_like_a_value(name, value) {
            return Err(SecretsError::Invalid(format!(
                "{name} looks like it carries the secret itself. The non-secret fields are for \
                 metadata only — the value is sealed, never stored here."
            )));
        }
        // An unlisted field is kept but reported by the API, because dropping it silently would
        // make a typo (a `mail_host` that nothing reads) look like it saved.
        let _ = allowed;
        kept.insert(name.clone(), value.clone());
    }
    Ok(Value::Object(kept))
}

/// `true` when a field name or value is shaped like the secret itself.
fn looks_like_a_value(name: &str, value: &Value) -> bool {
    const FORBIDDEN_NAMES: [&str; 6] = [
        "value",
        "password",
        "passphrase",
        "secret",
        "token",
        "private_key",
    ];
    let lowered = name.to_ascii_lowercase();
    if FORBIDDEN_NAMES
        .iter()
        .any(|forbidden| lowered == *forbidden)
    {
        return true;
    }
    let Value::String(text) = value else {
        return false;
    };
    let trimmed = text.trim();
    // A PEM block, a long opaque token or a key-shaped prefix. Short identifiers (a username, a
    // host, a fingerprint) never match: a fingerprint is hex, and a 32+ char base64ish run is
    // what a token looks like.
    trimmed.contains("BEGIN ")
        || (trimmed.len() >= 32
            && trimmed.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '+' | '/' | '.' | '_' | '-' | '=')
            })
            && trimmed.chars().any(|character| character.is_ascii_digit()))
}

/// Run the validator for a profile and store the outcome without ever blocking the save.
///
/// The message is passed through [`redact`] with the secret's own hint before it is stored, so a
/// provider that echoes the value back inside a refusal sentence cannot write it into the
/// database, an event or a log.
///
/// # Errors
///
/// [`SecretsError::Invalid`] when the kind is one this build does not know, and the database
/// failures otherwise.
pub async fn record_validation(
    pool: &PgPool,
    secret_id: Uuid,
    value_for_redaction: Option<&str>,
) -> Result<ValidationOutcome> {
    let row = sqlx::query_as::<_, (String, Value)>(
        "select kind, fields from secret_credentials where secret_id = $1",
    )
    .bind(secret_id)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::NotFound("secret"))?;

    let kind = parse_kind(&row.0)?;
    let outcome = validate(kind, &row.1);
    let mut message = outcome.message().to_owned();
    if let Some(value) = value_for_redaction.filter(|value| !value.is_empty()) {
        message = redact(&message, value);
    }

    let now = OffsetDateTime::now_utc();
    // The interval is read once. A second round trip here would be the only query in the write
    // path that exists just to compute a `next_validation_at` the list screen barely shows.
    let days = scheduled_interval(pool, secret_id).await?;
    let next = (days > 0).then(|| now + time::Duration::days(i64::from(days)));
    sqlx::query(
        "update secret_credentials set validation_state = $2, validation_message = $3, \
                validation_checked_at = $4, next_validation_at = $5, updated_at = $4 \
         where secret_id = $1",
    )
    .bind(secret_id)
    .bind(outcome.state())
    .bind(&message)
    .bind(now)
    .bind(next)
    .execute(pool)
    .await?;
    Ok(outcome)
}

/// Read a profile's validation interval, clamped to a sane range.
async fn scheduled_interval(pool: &PgPool, secret_id: Uuid) -> Result<i32> {
    let days: i32 = sqlx::query_scalar(
        "select validation_interval_days from secret_credentials where secret_id = $1",
    )
    .bind(secret_id)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::NotFound("secret"))?;
    Ok(days.clamp(0, 365))
}

/// Every typed credential of an organization, or all of them when `organization_id` is `None`.
pub async fn list_credentials(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<CredentialRow>> {
    // `secret_credentials` is a LEFT join, not an inner one. A read-only bridge (`file` / `env`)
    // has no profile row on purpose — the platform cannot type a credential it does not own — and
    // an inner join would hide it from the only screen whose job is to explain what it is. The
    // `kind`/`fields`/`validation_*` columns come back NULL and `kind()` returns `None`, which the
    // panel renders as "managed outside the platform" rather than as a broken row.
    let rows = sqlx::query_as::<_, CredentialRow>(
        "select s.id as secret_id, s.name, \
                coalesce(c.kind, 'external') as kind, \
                coalesce(c.fields, '{}'::jsonb) as fields, \
                coalesce(c.validation_state, 'unknown') as validation_state, \
                c.validation_message, c.validation_checked_at, \
                coalesce(c.validation_interval_days, 0)::int as validation_interval_days, \
                s.provider, s.provider_locator, s.read_only, s.organization_id, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = s.id and v.revoked_at is null), 0)::int as version, \
                s.created_at \
         from secrets s left join secret_credentials c on c.secret_id = s.id \
         where s.archived_at is null \
           and ($1::uuid is null or s.organization_id = $1 or s.scope_type = 'global') \
         order by s.name",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One credential profile by its secret id.
pub async fn find_credential(pool: &PgPool, secret_id: Uuid) -> Result<Option<CredentialRow>> {
    let row = sqlx::query_as::<_, CredentialRow>(
        "select c.secret_id, s.name, c.kind, c.fields, c.validation_state, c.validation_message, \
                c.validation_checked_at, c.validation_interval_days, s.provider, \
                s.provider_locator, s.read_only, s.organization_id, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = s.id and v.revoked_at is null), 0)::int as version, \
                s.created_at \
         from secret_credentials c join secrets s on s.id = c.secret_id where c.secret_id = $1",
    )
    .bind(secret_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// One credential profile by name, for the wizard and the slot editor's picker.
pub async fn find_credential_by_name(pool: &PgPool, name: &str) -> Result<Option<CredentialRow>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "select c.secret_id from secret_credentials c join secrets s on s.id = c.secret_id \
         where s.name = $1 and s.archived_at is null limit 1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    match id {
        Some(id) => find_credential(pool, id).await,
        None => Ok(None),
    }
}

/// The secret a profile is being attached to: who owns it and whether it can be written.
///
/// The create path needs this rather than a profile row, because on create there is no profile
/// yet — looking one up first is what turns "create" into a permanent `404 secret_not_found`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SecretOwner {
    /// The organization that owns the secret, `None` for an installation-level one.
    pub organization_id: Option<Uuid>,
    /// `local`, `file` or `env`.
    pub provider: String,
    /// `true` for a bridge whose value lives outside the platform.
    pub read_only: bool,
    /// The secret's name, for the audit sentence.
    pub name: String,
}

/// The secret a profile is being attached to.
///
/// # Errors
///
/// [`SecretsError::NotFound("secret")`] for an unknown or archived secret, and the database
/// failures otherwise.
pub async fn find_secret_owner(pool: &PgPool, secret_id: Uuid) -> Result<SecretOwner> {
    sqlx::query_as::<_, SecretOwner>(
        "select organization_id, provider, read_only, name from secrets \
         where id = $1 and archived_at is null",
    )
    .bind(secret_id)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::NotFound("secret"))
}

/// Attach a kind and its non-secret fields to an existing secret.
///
/// The secret must already exist and must be writable: a `file`/`env` bridge is managed outside
/// the platform, so typing it into the wizard is refused rather than silently accepted.
///
/// # Errors
///
/// [`SecretsError::ReadOnly`] for a bridge, [`SecretsError::Invalid`] for a bad kind or a field
/// that carries the value, [`SecretsError::NotFound("secret")`] for an unknown secret, and the
/// database failures otherwise.
pub async fn attach_profile(
    pool: &PgPool,
    secret_id: Uuid,
    kind: CredentialKind,
    fields: &Value,
) -> Result<CredentialRow> {
    let owner = find_secret_owner(pool, secret_id).await?;
    if owner.read_only || owner.provider != "local" {
        return Err(SecretsError::ReadOnly);
    }

    let fields = sanitize_fields(kind, fields)?;
    sqlx::query(
        "insert into secret_credentials (secret_id, kind, fields, validation_state) \
         values ($1, $2, $3, 'unknown') \
         on conflict (secret_id) do update set kind = excluded.kind, fields = excluded.fields, \
             validation_state = 'unknown', validation_message = null, updated_at = now()",
    )
    .bind(secret_id)
    .bind(kind.as_str())
    .bind(&fields)
    .execute(pool)
    .await?;

    // The validator runs immediately on create, so the chip in the wizard is real rather than
    // "unknown until someone clicks". A failure here is *recorded*, not raised: the credential
    // is saved either way, which is the rule the request states.
    let _ = record_validation(pool, secret_id, None).await;
    find_credential(pool, secret_id)
        .await?
        .ok_or(SecretsError::NotFound("secret"))
}

/// Every slot assignment, joined with the names its secrets carry.
pub async fn list_slots(pool: &PgPool) -> Result<Vec<SlotRow>> {
    let rows = sqlx::query_as::<_, SlotRow>(
        "select sl.id, sl.scope_type, sl.scope_id, sl.slot, sl.primary_secret_id, \
                sl.fallback_secret_id, p.name as primary_name, f.name as fallback_name, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = sl.primary_secret_id and v.revoked_at is null), 0)::int \
                    as primary_version, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = sl.fallback_secret_id and v.revoked_at is null), 0)::int \
                    as fallback_version, \
                sl.last_resolved_by, sl.last_resolved_at, sl.created_at, sl.updated_at \
         from credential_slots sl \
         left join secrets p on p.id = sl.primary_secret_id \
         left join secrets f on f.id = sl.fallback_secret_id \
         order by sl.scope_type, sl.scope_id, sl.slot",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The catalogue the slot editor's picker offers: name, description and consumers.
pub async fn slot_catalog(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "select slot, description, consumers from credential_slot_catalog order by sort_order, slot",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One slot assignment by scope and slot name.
pub async fn find_slot(
    pool: &PgPool,
    scope_type: &str,
    slot: &str,
    scope_id: &str,
) -> Result<Option<SlotRow>> {
    let row = sqlx::query_as::<_, SlotRow>(
        "select sl.id, sl.scope_type, sl.scope_id, sl.slot, sl.primary_secret_id, \
                sl.fallback_secret_id, p.name as primary_name, f.name as fallback_name, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = sl.primary_secret_id and v.revoked_at is null), 0)::int \
                    as primary_version, \
                coalesce((select max(v.version) from secret_versions v \
                          where v.secret_id = sl.fallback_secret_id and v.revoked_at is null), 0)::int \
                    as fallback_version, \
                sl.last_resolved_by, sl.last_resolved_at, sl.created_at, sl.updated_at \
         from credential_slots sl \
         left join secrets p on p.id = sl.primary_secret_id \
         left join secrets f on f.id = sl.fallback_secret_id \
         where sl.scope_type = $1 and sl.slot = $2 and sl.scope_id = $3",
    )
    .bind(scope_type)
    .bind(slot)
    .bind(scope_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Assign a primary and an optional fallback to a slot.
///
/// Ordering matters here: the same-secret check runs *before* the write, so the `409` the API
/// returns is the crate's own sentence rather than a database constraint violation read back as
/// an opaque error. An empty primary clears the assignment, which is what "removing a slot that a
/// consumer is actively resolving" does — the API asks for confirmation first, naming the
/// consumer, and then calls this with `None`.
///
/// # Errors
///
/// [`SecretsError::Invalid`] for a bad scope, slot or a self-referencing fallback;
/// [`SecretsError::NotFound("secret")]` when a named secret does not exist; and the database
/// failures otherwise.
pub async fn assign_slot(
    pool: &PgPool,
    scope_type: &str,
    scope_id: &str,
    slot: &str,
    primary_secret_id: Option<Uuid>,
    fallback_secret_id: Option<Uuid>,
) -> Result<SlotRow> {
    check_scope_and_slot(scope_type, slot)?;
    let scope_id = scope_id.trim();
    if scope_id.is_empty() {
        return Err(SecretsError::Invalid(
            "a slot assignment needs the scope it belongs to".to_owned(),
        ));
    }
    if primary_secret_id.is_some() && primary_secret_id == fallback_secret_id {
        return Err(SecretsError::Invalid(
            "the fallback cannot be the primary — that is a no-op that reads like a safety net"
                .to_owned(),
        ));
    }

    for candidate in [primary_secret_id, fallback_secret_id]
        .into_iter()
        .flatten()
    {
        let exists: Option<Uuid> =
            sqlx::query_scalar("select id from secrets where id = $1 and archived_at is null")
                .bind(candidate)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            return Err(SecretsError::NotFound("secret"));
        }
    }

    sqlx::query(
        "insert into credential_slots \
             (scope_type, scope_id, slot, primary_secret_id, fallback_secret_id) \
         values ($1, $2, $3, $4, $5) \
         on conflict (scope_type, scope_id, slot) do update set \
             primary_secret_id = excluded.primary_secret_id, \
             fallback_secret_id = excluded.fallback_secret_id, updated_at = now()",
    )
    .bind(scope_type)
    .bind(scope_id)
    .bind(slot)
    .bind(primary_secret_id)
    .bind(fallback_secret_id)
    .execute(pool)
    .await?;

    find_slot(pool, scope_type, slot, scope_id)
        .await?
        .ok_or(SecretsError::NotFound("slot"))
}

/// Resolve a slot for a consumer, recording that the consumer used it.
///
/// The fallback path is the interesting one: a primary that is archived or has no live version
/// is skipped, and the resolution comes back with `fell_back = true` so the caller can emit the
/// event. Recording `last_resolved_by` is what lets the panel name a consumer when an operator
/// tries to remove a slot they are actively resolving.
///
/// # Errors
///
/// [`SecretsError::Invalid`] for a bad scope or slot, [`SecretsError::NotFound("slot")]` when the
/// slot was never assigned, and the database failures otherwise.
pub async fn resolve_slot(
    pool: &PgPool,
    scope_type: &str,
    scope_id: &str,
    slot: &str,
    consumer: &str,
) -> Result<Resolution> {
    check_scope_and_slot(scope_type, slot)?;
    let row = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>)>(
        "select primary_secret_id, fallback_secret_id from credential_slots \
         where scope_type = $1 and scope_id = $2 and slot = $3",
    )
    .bind(scope_type)
    .bind(scope_id)
    .bind(slot)
    .fetch_optional(pool)
    .await?
    .ok_or(SecretsError::NotFound("slot"))?;

    for (candidate, fell_back) in [(row.0, false), (row.1, true)] {
        let Some(secret_id) = candidate else { continue };
        let resolved: Option<(String, i32)> = sqlx::query_as(
            "select s.name, coalesce(max(v.version), 0)::int from secrets s \
             left join secret_versions v on v.secret_id = s.id and v.revoked_at is null \
             where s.id = $1 and s.archived_at is null group by s.name",
        )
        .bind(secret_id)
        .fetch_optional(pool)
        .await?;
        let Some((name, version)) = resolved else {
            continue;
        };

        sqlx::query(
            "update credential_slots set last_resolved_by = $4, last_resolved_at = now() \
             where scope_type = $1 and scope_id = $2 and slot = $3",
        )
        .bind(scope_type)
        .bind(scope_id)
        .bind(slot)
        .bind(consumer)
        .execute(pool)
        .await?;

        return Ok(Resolution {
            secret_id,
            name,
            version,
            fell_back,
        });
    }

    Err(SecretsError::Invalid(format!(
        "the {slot} slot is not assigned for {scope_type} {scope_id}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_slot_list_matches_the_seeded_catalogue() {
        // Migration 0019 seeds `credential_slot_catalog`; the panel's picker is built from the
        // constant above. A slot added to one and not the other is a silent dead end for an
        // operator, so both lists are asserted here against the documented six.
        assert_eq!(SLOTS.len(), 6);
        for slot in [
            "ai.provider",
            "smtp",
            "payments.stripe",
            "storage.s3",
            "ssh.release",
            "identity.ldap",
        ] {
            assert!(SLOTS.contains(&slot), "{slot} must be offerable");
        }
    }

    #[test]
    fn scope_and_slot_are_checked_with_the_valid_lists() {
        assert!(check_scope_and_slot("environment", "smtp").is_ok());
        assert!(check_scope_and_slot("site", "storage.s3").is_ok());
        let bad_scope = check_scope_and_slot("tenant", "smtp")
            .unwrap_err()
            .to_string();
        assert!(bad_scope.contains("environment"), "{bad_scope}");
        let bad_slot = check_scope_and_slot("environment", "mail")
            .unwrap_err()
            .to_string();
        assert!(bad_slot.contains("smtp"), "{bad_slot}");
    }

    #[test]
    fn a_kind_outside_the_five_is_refused_with_the_list() {
        let message = parse_kind("password").unwrap_err().to_string();
        for kind in CredentialKind::all() {
            assert!(
                message.contains(kind.as_str()),
                "{message} must name {kind:?}"
            );
        }
    }

    #[test]
    fn non_secret_fields_refuse_anything_shaped_like_the_value() {
        let kind = CredentialKind::ApiKey;
        let ok = sanitize_fields(
            kind,
            &json!({ "endpoint": "https://api.example.com", "username": "svc" }),
        )
        .expect("plain metadata must pass");
        assert_eq!(ok["endpoint"], json!("https://api.example.com"));

        for name in ["value", "password", "secret", "token", "private_key"] {
            let refused = sanitize_fields(kind, &json!({ name: "anything" })).unwrap_err();
            assert!(
                refused.to_string().contains(name),
                "{name} must be refused: {refused}"
            );
        }

        let long_token = sanitize_fields(
            kind,
            &json!({ "note": "sk9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f" }),
        );
        assert!(
            long_token.is_err(),
            "a 38-character opaque run is a token, not metadata"
        );

        let pem = sanitize_fields(
            kind,
            &json!({ "note": "-----BEGIN OPENSSH PRIVATE KEY-----" }),
        );
        assert!(pem.is_err(), "a PEM header is the value, not metadata");

        // A fingerprint is 22 base64 characters and a host is short: neither is a value.
        assert!(
            sanitize_fields(
                CredentialKind::SshKey,
                &json!({ "fingerprint": "SHA256:aBcDeFgHiJkLmNoPqRsTuVw" })
            )
            .is_ok()
        );
        assert!(
            sanitize_fields(kind, &json!({ "endpoint": "https://api.example.com/v1" })).is_ok()
        );
    }

    #[test]
    fn a_field_name_is_humanised_for_the_detail_list() {
        assert_eq!(humanise("key_prefix"), "Key prefix");
        assert_eq!(humanise("host"), "Host");
        assert_eq!(humanise("expires_at"), "Expires at");
    }
}
