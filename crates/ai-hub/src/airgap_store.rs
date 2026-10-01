//! The air-gap switch, its allow-list, and the pre-call refusal (REQ-106, slice 2).
//!
//! Slice 1 wrote down which providers are local. This module is the switch that *reads* that
//! fact, and it exists as a separate module from [`crate::local_store`] for the reason the
//! request states: "never leaves the instance" has to be a checked fact, so the check must be
//! one function that the save path, the call path and the doctor all call — not three that can
//! disagree.
//!
//! # Locality is RE-DERIVED at the check, never read from the column
//!
//! The tempting implementation reads `ai_providers.locality`, which slice 1 already fills, and is
//! one cheap query. This module deliberately does not, and the reason is what `locality` means.
//! It is a column written **at save time** from [`crate::local_host::classify_host`], so it is
//! true *as of the moment the endpoint was registered*. Two things can make it stale, and both
//! are ordinary rather than adversarial:
//!
//! 1. An operator adds `gpu-box.internal` to the allow-list. Every provider row that pointed
//!    there was refused at save time and has no row; a provider whose `host_kind` was `NULL`
//!    for another reason now qualifies.
//! 2. A provider is edited by a different code path, or a row is restored from a backup whose
//!    allow-list did not travel with it.
//!
//! Re-deriving costs one `classify_host` call against a list of a handful of names, and it means
//! the switch is decided by the rule rather than by a cached copy of the rule's output. The
//! column stays exactly as useful as the request says it is — a screen's "why is this local?"
//! badge — and it is not what security depends on.
//!
//! # The refusal is honest, and it is a refusal rather than an error
//!
//! [`check_call`] returns `Ok(Some(Refusal))` rather than `Err`. A caller that gets `Err` cannot
//! tell "the air gap stopped this" from "the database was unreachable", and those two demand
//! opposite responses from the same screen: the first is a policy answer the operator chose, the
//! second is an outage. So the air gap is a value, and only a broken query is an error.
//!
//! The refusal carries the provider **name**, the **host** and the **setting** that stopped it,
//! because the request is explicit that it must never degrade to a generic "request failed":
//! an operator reading `blocked` learns nothing, while an operator reading "provider `OpenAI`
//! (api.openai.com) was refused because the air gap is on" learns where to go.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::local_host::{HostKind, classify_host, host_of};

/// The single row's id. Enforced by `ai_airgap_state_id_check`, and named here so the store and
/// its tests cannot disagree about it.
pub const AIRGAP_ROW_ID: i16 = 1;

/// The reason an operator must give, in characters.
///
/// Both ends are the request's ("10–500 characters, required"), and the lower bound is the one
/// that matters: a one-word reason makes the audit row useless six months later, which is the
/// only moment anybody reads it.
pub const REASON_MIN: usize = 10;
/// The upper bound, so a pasted essay cannot crowd the audit view.
pub const REASON_MAX: usize = 500;

/// The air-gap switch as the panel reads it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AirgapState {
    /// Whether non-local calls are refused right now.
    pub enabled: bool,
    /// Why it was on. Kept after it is turned off, on purpose: see the migration's header.
    pub reason: Option<String>,
    /// Who turned it on. `None` once that user is deleted, which the FK spells `on delete set
    /// null` rather than cascading an audit row away.
    pub enabled_by: Option<Uuid>,
    /// When it was turned on.
    pub enabled_at: Option<OffsetDateTime>,
    /// Whether the operator acknowledged the list of providers that will stop working.
    pub low_confidence_ack: bool,
    /// Last egress verification result, written by slice 4.
    pub egress_verified_at: Option<OffsetDateTime>,
    /// The host the last verification aimed at.
    pub egress_verify_target: Option<String>,
    /// `passed`, `failed` or NULL when it has never run.
    pub egress_verify_result: Option<String>,
    /// When the row last changed for any reason.
    pub updated_at: OffsetDateTime,
}

/// A refusal the air gap decided, shaped for the wire.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Refusal {
    /// The provider that would have answered.
    pub provider: String,
    /// Its host — the thing that would have left the installation.
    pub host: String,
    /// Which rule admitted it as local, when one did. `None` is the common case and it is the
    /// interesting one: the host is public, nothing made it internal, and that is the whole
    /// reason for the refusal.
    pub host_kind: Option<String>,
}

impl Refusal {
    /// The message a client may show. Names the provider, the host and the switch.
    #[must_use]
    pub fn message(&self) -> String {
        let host = if self.host.is_empty() {
            "an unparseable host".to_owned()
        } else {
            self.host.clone()
        };
        format!(
            "the air gap is on, so the call to provider \"{}\" ({host}) was refused before any \
             request left this installation. Point the feature at a local endpoint, or turn the \
             air gap off in AI settings.",
            self.provider
        )
    }

    /// The machine-readable code. The request fixes this spelling; the UI branches on it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "ai_airgap_blocked"
    }
}

/// The allow-list, as the settings screen lists it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AirgapHost {
    /// Row id.
    pub id: Uuid,
    /// The host name as stored (lowercased).
    pub host: String,
    /// Why the operator added it.
    pub note: Option<String>,
    /// Who added it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
}

/// Read the switch. Never fails for want of a row: the migration seeds one, and a
/// missing row is repaired here rather than reported, because "the settings screen 500s" is a
/// worse answer than "the air gap is off".
pub async fn read_state(pool: &PgPool) -> Result<AirgapState> {
    // The self-call is boxed rather than made a loop. A `loop { match … }` around the select and
    // the insert is the tidier shape on paper, but the value crosses an await while it is still
    // owned by the loop body, which is the async equivalent of the borrow the loop is meant to
    // express. One box is cheaper than the two `Pin<Box<_>>`s the alternative needs.
    read_state_inner(pool).await
}

/// The body of [`read_state`], split only so the repair path can call it once.
async fn read_state_inner(pool: &PgPool) -> Result<AirgapState> {
    const SELECT: &str = "select enabled, reason, enabled_by, enabled_at, low_confidence_ack, \
                egress_verified_at, egress_verify_target, egress_verify_result, updated_at \
           from ai_airgap_state where id = $1";

    if let Some(state) = sqlx::query_as::<_, AirgapState>(SELECT)
        .bind(AIRGAP_ROW_ID)
        .fetch_optional(pool)
        .await?
    {
        return Ok(state);
    }

    // The repair re-reads rather than calling `read_state` again. A self-call here is mutual
    // recursion across two async frames, which the compiler rejects without a `Box::pin`; and the
    // re-read is not redundant anyway — between the select and the insert another writer may have
    // created the row, and returning what the database now holds is the honest answer either way.
    sqlx::query("insert into ai_airgap_state (id, enabled) values ($1, false) on conflict (id) do nothing")
        .bind(AIRGAP_ROW_ID)
        .execute(pool)
        .await?;

    sqlx::query_as::<_, AirgapState>(SELECT)
        .bind(AIRGAP_ROW_ID)
        .fetch_one(pool)
        .await
        .map_err(|err| AiHubError::Database(err)) // pragma: no cover — the row exists by now
}

/// `true` when the switch is on. The single question every call path asks first.
pub async fn is_enabled(pool: &PgPool) -> Result<bool> {
    Ok(read_state(pool).await?.enabled)
}

/// The allow-list, lowercased, for [`classify_host`].
///
/// Read once per call rather than cached: the check is on the hot path of every AI request, but
/// the list is a handful of rows and a stale allow-list is a security switch reading old rules —
/// which is the exact failure this module exists to prevent.
pub async fn allowlist(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>("select host from ai_airgap_hosts order by host")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Decide whether one provider may be dialled, and say why not when it may not.
///
/// `base_url` is passed in rather than read here so this function stays a pure decision about
/// `(url, allow-list)` — the caller has already loaded the provider row for the routing decision,
/// and re-reading it would be a second lookup that can disagree with the first.
///
/// Returns `Ok(None)` when the call may proceed, `Ok(Some(refusal))` when the air gap stopped it,
/// and `Err` only when the allow-list could not be read (a real failure, unlike the refusal).
pub async fn check_call(
    pool: &PgPool,
    provider_name: &str,
    base_url: &str,
) -> Result<Option<Refusal>> {
    if !is_enabled(pool).await? {
        return Ok(None);
    }

    // The allow-list is only read when the gap is on, so the hot path for an installation that
    // never enables it stays one row read.
    let hosts = allowlist(pool).await?;
    // A base URL the platform cannot parse has no host to classify. Refusing it is the safe
    // reading, but it is reported with an empty host rather than a wrong one, so the message
    // says what is actually wrong with the configuration.
    let kind: Option<HostKind> = match host_of(base_url) {
        Some(host) => classify_host(&host, &hosts)?,
        None => None,
    };

    if kind.is_some() {
        return Ok(None);
    }

    Ok(Some(Refusal {
        provider: provider_name.to_owned(),
        host: host_of(base_url).unwrap_or_default(),
        host_kind: None,
    }))
}

/// What changing the switch asked for.
#[derive(Debug, Clone)]
pub struct SetAirgap {
    /// The new state.
    pub enabled: bool,
    /// Required when enabling; ignored when disabling.
    pub reason: Option<String>,
    /// Whether the operator acknowledged what will stop working.
    pub low_confidence_ack: bool,
    /// The actor, for the audit row.
    pub actor: Option<Uuid>,
}

/// Flip the switch, enforcing the reason rule.
///
/// # The reason is required on the way ON and not on the way OFF
///
/// That asymmetry is the request's, not a convenience: an operator turning the gap on is making
/// a compliance claim that will be read back later, and a blank field makes the claim
/// unattributable. Turning it **off** is the emergency action, and an emergency action that can
/// be blocked by a validation rule is a control that can fail closed at the worst moment — so it
/// takes no reason and keeps the previous one on the row as the record of why the gap was ever
/// in effect.
pub async fn set_state(pool: &PgPool, change: &SetAirgap) -> Result<AirgapState> {
    if change.enabled {
        let reason = change.reason.as_deref().unwrap_or_default().trim();
        if reason.is_empty() {
            return Err(AiHubError::InvalidAirgap(
                "a reason is required to turn the air gap on, because this row is what an \
                 auditor reads later. Write 10–500 characters naming what it protects."
                    .to_owned(),
            ));
        }
        let length = reason.chars().count();
        if !(REASON_MIN..=REASON_MAX).contains(&length) {
            return Err(AiHubError::InvalidAirgap(format!(
                "the reason must be {REASON_MIN}–{REASON_MAX} characters; it is {length}."
            )));
        }
    }

    // Turning OFF deliberately does not clear `reason` / `enabled_by` / `enabled_at`: the row is
    // the history of the switch, and nulling them would make "was this ever on, and who did it"
    // unanswerable — the one question an auditor asks after the gap is back off.
    sqlx::query(
        "insert into ai_airgap_state (id, enabled, reason, enabled_by, enabled_at, \
             low_confidence_ack, updated_at) \
         values ($1, $2, $3, $4, case when $2 then now() else null end, $5, now()) \
         on conflict (id) do update set \
             enabled = excluded.enabled, \
             reason = case when excluded.enabled then excluded.reason else ai_airgap_state.reason end, \
             enabled_by = case when excluded.enabled then excluded.enabled_by else ai_airgap_state.enabled_by end, \
             enabled_at = case when excluded.enabled then now() else ai_airgap_state.enabled_at end, \
             low_confidence_ack = case when excluded.enabled then excluded.low_confidence_ack else ai_airgap_state.low_confidence_ack end, \
             updated_at = now()",
    )
    .bind(AIRGAP_ROW_ID)
    .bind(change.enabled)
    .bind(
        change
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| change.enabled && !reason.is_empty()),
    )
    .bind(change.actor)
    .bind(change.low_confidence_ack)
    .execute(pool)
    .await?;

    read_state(pool).await
}

/// The allow-list, as the settings screen lists it.
pub async fn list_hosts(pool: &PgPool) -> Result<Vec<AirgapHost>> {
    Ok(sqlx::query_as::<_, AirgapHost>(
        "select id, host, note, created_by, created_at from ai_airgap_hosts order by host",
    )
    .fetch_all(pool)
    .await?)
}

/// Add a host to the allow-list.
///
/// Returns `Ok(None)` when the host was already there. That is not an error: the settings screen
/// is a form an operator may submit twice, and a second identical save is the same request, not a
/// mistake. It is reported as "nothing changed" so the caller can say so instead of pretending a
/// new row appeared.
pub async fn add_host(
    pool: &PgPool,
    host: &str,
    note: Option<&str>,
    actor: Option<Uuid>,
) -> Result<Option<AirgapHost>> {
    let host = normalize_host(host)?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "insert into ai_airgap_hosts (host, note, created_by) values ($1, $2, $3) \
         on conflict (host) do nothing returning id",
    )
    .bind(&host)
    .bind(note.map(str::trim).filter(|note| !note.is_empty()))
    .bind(actor)
    .fetch_optional(pool)
    .await?;

    let Some(id) = id else {
        return Ok(None);
    };
    Ok(Some(
        sqlx::query_as::<_, AirgapHost>(
            "select id, host, note, created_by, created_at from ai_airgap_hosts where id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await?,
    ))
}

/// Remove a host from the allow-list.
///
/// A host that is not on the list is an error rather than a silent success: the settings screen's
/// delete button must not report a deletion that did not happen, and a row removed twice means
/// the list changed underneath the operator who pressed it.
pub async fn remove_host(pool: &PgPool, id: Uuid) -> Result<()> {
    let removed = sqlx::query("delete from ai_airgap_hosts where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if removed.rows_affected() == 0 {
        return Err(AiHubError::AirgapHostNotFound(id));
    }
    Ok(())
}

/// Lowercase and trim a host, refusing what `classify_host` could never admit.
///
/// The same normalization [`crate::local_host::host_of`] performs, called here rather than
/// through `host_of` because an allow-list entry is a bare host with no scheme — the two shapes
/// are the same host and must normalize identically, or a list written in caps would silently
/// stop matching.
fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
    if host.is_empty() {
        return Err(AiHubError::InvalidAirgap(
            "an allow-list entry needs a host name.".to_owned(),
        ));
    }
    // A host is a name or an address. Anything carrying a scheme, a path, a port or a space is a
    // paste of the wrong thing — most often a full base URL — and it would never match, so it is
    // refused here rather than stored as a row that silently widens nothing.
    if host.contains("://")
        || host.contains('/')
        || host.contains(' ')
        || host.contains(':')
    {
        return Err(AiHubError::InvalidAirgap(format!(
            "\"{host}\" is not a bare host. Give the name or address alone — no scheme, no port, \
             no path — because that is what the allow-list matches against."
        )));
    }
    Ok(host)
}

/// Which providers would stop answering with the gap on.
///
/// The confirmation lists these, and the list is computed from the *same* rule the check uses, so
/// the operator is never shown a set that differs from what they will actually get.
pub async fn providers_that_would_block(pool: &PgPool) -> Result<Vec<(String, String)>> {
    let hosts = allowlist(pool).await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "select name, base_url from ai_providers where enabled = true order by name",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter(|(_, base_url)| match host_of(base_url) {
            Some(host) => !matches!(classify_host(&host, &hosts), Ok(Some(_))),
            // Unparseable: refused when the gap is on, so it belongs on the list.
            None => true,
        })
        .collect())
}
