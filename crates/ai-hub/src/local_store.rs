//! Local endpoints and the models they serve (REQ-106, slice 1).
//!
//! [`crate::local_host`] decides what a local endpoint *is*. This module is where the rows live
//! and where the two facts that are easy to fake — "this endpoint is local" and "this model is
//! there" — are written down.
//!
//! # Locality is written by this module, never by the caller
//!
//! The endpoint save path takes a `locality` value from the form and then **overwrites** it with
//! [`crate::local_host::classify_host`]'s answer. The parameter is what the operator asked for and
//! the stored value is what the platform verified, and a caller that asked for `local` on a public
//! host gets an error rather than a row the air-gap check would trust. If the two could disagree,
//! the air gap would be a checkbox: the column a security switch reads would be the one value in
//! the platform nobody derives.
//!
//! # A pull is a claim, and the row it claims on is the model row
//!
//! `begin_pull` is an `insert … on conflict do nothing` whose `rows_affected` *is* the answer to
//! "may I download this?" — `1` means this caller won the race and starts the pull, `0` means the
//! model is already known to the endpoint (available, pulling or in error) and the second click is
//! a no-op with the current state returned. Check-then-insert would leave a window in which two
//! processes both read "missing" and both start downloading the same weights to the same disk.
//!
//! The refusal is not silent: [`begin_pull`] returns the row it found, so the panel can say *why*
//! the drawer did not open ("already pulling", "already available") instead of appearing to do
//! nothing.
//!
//! # Nothing here calls the local server
//!
//! Every function in this module is database work. The HTTP lives in the route, which owns the
//! stub-server shape the tests use, and that split is what lets the whole store be proven against
//! a real database with no network in the picture — and it means a slow local server can never
//! hold a database connection open while it thinks.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::local_host::{HostKind, classify_host, host_of};

/// The statuses an `ai_local_models` row can hold.
pub const MODEL_STATUSES: &[&str] = &["available", "pulling", "missing", "error"];

/// A local endpoint, as the panel reads it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LocalEndpoint {
    /// Provider id — a local endpoint *is* a provider row, not a parallel table.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Base URL as stored (normalized on save).
    pub base_url: String,
    /// The host part, precomputed for the screen so it does not re-parse URLs.
    pub host: String,
    /// `local` or `remote`.
    pub locality: String,
    /// Why the platform believes it: `loopback`, `private`, `allowlisted`, or NULL.
    pub host_kind: Option<String>,
    /// Wire protocol.
    pub protocol: String,
    /// `false` when the operator switched the endpoint off.
    pub enabled: bool,
    /// When a probe last took a sample here. NULL means "never probed", not "down".
    pub last_seen_at: Option<OffsetDateTime>,
    /// How many model rows this endpoint has.
    pub model_count: i64,
    /// How many of them are `available` — the number the stat tile shows.
    pub available_count: i64,
}

/// A model a local endpoint serves.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LocalModel {
    /// Model row id.
    pub id: Uuid,
    /// Owning endpoint.
    pub provider_id: Uuid,
    /// Endpoint name, joined for the table's "Endpoint" column.
    pub endpoint_name: String,
    /// The key the endpoint answers to, and the key a pull/remove names.
    pub model_key: String,
    /// Friendly name when the server reports one.
    pub display_name: Option<String>,
    /// Download size in bytes, when the server reports it.
    pub size_bytes: Option<i64>,
    /// Parameter count, when the server reports it.
    pub parameter_count: Option<i64>,
    /// Quantization label (`q4_K_M`, …), when the server reports it.
    pub quantization: Option<String>,
    /// Context window in tokens.
    pub context_window: Option<i32>,
    /// The model can call tools.
    pub supports_tools: bool,
    /// The model accepts images.
    pub supports_vision: bool,
    /// The model produces embeddings.
    pub supports_embeddings: bool,
    /// The model reranks.
    pub supports_rerank: bool,
    /// Embedding width, for the dimension check in the doctor.
    pub embedding_dimension: Option<i32>,
    /// `available`, `pulling`, `missing` or `error`.
    pub status: String,
    /// 0–100 while pulling.
    pub pull_progress: i32,
    /// The server's own progress/error line, verbatim.
    pub pull_message: Option<String>,
    /// The server holds the weights in memory.
    pub resident: bool,
    /// When this model last served a request.
    pub last_used_at: Option<OffsetDateTime>,
    /// When the row last changed.
    pub updated_at: OffsetDateTime,
}

/// A local endpoint to register.
#[derive(Debug, Clone)]
pub struct NewLocalEndpoint {
    /// Display name.
    pub name: String,
    /// Base URL, normalized by the caller.
    pub base_url: String,
    /// Wire protocol; `openai_compatible` is what every documented local server speaks.
    pub protocol: String,
    /// What the operator asked for. Verified, never trusted — see the module docs.
    pub requested_local: bool,
    /// A key, when the local server wants one (llama.cpp often does not).
    pub api_key: Option<String>,
}

/// A model to record, as read off a local server's own model list.
#[derive(Debug, Clone)]
pub struct ServedModel {
    /// The key the endpoint answers to.
    pub model_key: String,
    /// Friendly name.
    pub display_name: Option<String>,
    /// Download size in bytes.
    pub size_bytes: Option<i64>,
    /// Parameter count.
    pub parameter_count: Option<i64>,
    /// Quantization label.
    pub quantization: Option<String>,
    /// Context window in tokens.
    pub context_window: Option<i32>,
    /// Capability flags as the endpoint reports them.
    pub supports_tools: bool,
    /// Capability flag.
    pub supports_vision: bool,
    /// Capability flag.
    pub supports_embeddings: bool,
    /// Capability flag.
    pub supports_rerank: bool,
    /// Embedding width.
    pub embedding_dimension: Option<i32>,
}

/// The outcome of asking an endpoint to pull a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// This caller claimed the pull and should start it.
    Started,
    /// The endpoint already serves the model; nothing to do.
    AlreadyAvailable,
    /// Another pull of this model is already running.
    AlreadyPulling,
}

impl PullOutcome {
    /// Wire name for the route's response body.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::AlreadyAvailable => "already_available",
            Self::AlreadyPulling => "already_pulling",
        }
    }
}

/// Read every provider, with locality and the two model counts the screen shows.
///
/// **All providers, not only local ones.** The list is "endpoints with their locality badge" per
/// the request, and an operator looking at `/ai/local` to find out what their installation talks
/// to needs to see the remote ones *labelled remote* — a list containing only local endpoints
/// cannot answer "is anything still leaving this machine?", which is the question the screen
/// exists for. The air-gap check reads `locality` per provider, so this being a superset costs
/// nothing.
pub async fn list_endpoints(pool: &PgPool) -> Result<Vec<LocalEndpoint>> {
    let rows = sqlx::query_as::<_, LocalEndpoint>(
        "select p.id, p.name, p.base_url, split_part(trim(both '/' from \
           split_part(p.base_url, '://', 2)), '/', 1) as host, \
           p.locality, p.host_kind, p.protocol, p.enabled, p.last_seen_at, \
           count(m.id) as model_count, \
           count(m.id) filter (where m.status = 'available') as available_count \
         from ai_providers p \
         left join ai_local_models m on m.provider_id = p.id \
         group by p.id \
         order by (p.locality = 'local') desc, lower(p.name)",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Read one endpoint by id.
///
/// `Err` rather than `Ok(None)` so the route's "not found" arm is a single branch, and so this
/// cannot be confused with "there are no endpoints" — the same distinction the guard store draws
/// between a missing row and an empty list.
pub async fn get_endpoint(pool: &PgPool, id: Uuid) -> Result<LocalEndpoint> {
    sqlx::query_as::<_, LocalEndpoint>(
        "select p.id, p.name, p.base_url, split_part(trim(both '/' from \
           split_part(p.base_url, '://', 2)), '/', 1) as host, \
           p.locality, p.host_kind, p.protocol, p.enabled, p.last_seen_at, \
           count(m.id) as model_count, \
           count(m.id) filter (where m.status = 'available') as available_count \
         from ai_providers p \
         left join ai_local_models m on m.provider_id = p.id \
         where p.id = $1 \
         group by p.id",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(AiHubError::ProviderNotFound)
}

/// Classify a base URL and refuse the ones that are not local.
///
/// **This is the one place locality is decided.** It is `pub` rather than private because the
/// air-gap check (slice 2) and the doctor (slice 4) must classify with the *same* function, and
/// three copies of a range check would drift — see the module docs in `local_host`.
///
/// # Errors
///
/// [`AiHubError::InvalidProvider`] with a message naming the host: a metadata endpoint is refused
/// with its reason, and a public host is refused because the caller asked for a *local* endpoint.
/// The message names the host because "invalid base URL" is the difference between a support
/// ticket and a five-second fix.
pub fn require_local(base_url: &str, allowlist: &[String]) -> Result<HostKind> {
    let host = host_of(base_url).ok_or_else(|| {
        AiHubError::InvalidProvider(format!(
            "\"{base_url}\" has no host to check. A local endpoint needs an absolute URL such as \
             http://127.0.0.1:11434/v1 or http://192.168.1.10:8000/v1."
        ))
    })?;
    match classify_host(&host, allowlist)? {
        Some(kind) => Ok(kind),
        None => Err(AiHubError::InvalidProvider(format!(
            "\"{host}\" is not a local host. A local endpoint must point at loopback \
             (localhost, 127.0.0.1, ::1), a private network address (10/8, 172.16/12, 192.168/16, \
             100.64/10, fc00::/7), a container host name (host.docker.internal), or a host on \
             this installation's internal-host allow-list. Registering a public address as local \
             would make the air-gap switch report that nothing leaves this machine while it does."
        ))),
    }
}

/// Register a local endpoint, with locality derived rather than taken.
///
/// Returns the new provider id.
///
/// # Errors
///
/// [`AiHubError::InvalidProvider`] when the host is not local (see [`require_local`]), and the
/// existing provider-name/URL errors otherwise.
pub async fn create_endpoint(pool: &PgPool, new: NewLocalEndpoint) -> Result<Uuid> {
    let host_kind = require_local(&new.base_url, &[])?;
    // The asked-for value is only allowed to *narrow* what the check found: asking for a remote
    // endpoint that happens to point at loopback is stored as remote, because "remote" is the
    // honest label for an endpoint whose traffic this installation did not classify.
    let locality = if new.requested_local {
        "local"
    } else {
        "remote"
    };
    let (id,): (Uuid,) = sqlx::query_as::<_, (Uuid,)>(
        "insert into ai_providers (id, name, protocol, base_url, api_key, kind, locality, \
           host_kind, enabled, created_at, updated_at) \
         values ($1, $2, $3, $4, $5, 'local', $6, $7, true, now(), now()) \
         returning id",
    )
    .bind(Uuid::new_v4())
    .bind(new.name.trim())
    .bind(new.protocol.trim())
    .bind(new.base_url.trim())
    .bind(
        new.api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty()),
    )
    .bind(locality)
    .bind(host_kind.as_str())
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Set the derived locality columns on an existing provider.
///
/// Used by the endpoint form's edit path and by slice 2's air-gap re-check, so both go through
/// the same write rather than one of them remembering to update `host_kind`.
pub async fn set_locality(pool: &PgPool, id: Uuid, kind: Option<HostKind>) -> Result<()> {
    sqlx::query(
        "update ai_providers set locality = $2, host_kind = $3, updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(if kind.is_some() { "local" } else { "remote" })
    .bind(kind.map(HostKind::as_str))
    .execute(pool)
    .await?;
    Ok(())
}

/// Note that an endpoint answered, so the screen's "last checked" is a fact and not a guess.
pub async fn touch_endpoint(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("update ai_providers set last_seen_at = now() where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The filters `/ai/local/models` accepts.
#[derive(Debug, Clone, Default)]
pub struct ModelFilter {
    /// Restrict to one endpoint.
    pub provider_id: Option<Uuid>,
    /// Restrict to one status.
    pub status: Option<String>,
    /// Restrict to one capability (`tools`, `vision`, `embeddings`, `rerank`).
    pub capability: Option<String>,
    /// Free text over the key and the display name.
    pub search: Option<String>,
}

/// List the models the endpoints serve.
///
/// **The capability filter is a `where`, not a post-filter.** An earlier shape built the whole
/// list and filtered it in Rust, which meant a table with 400 local models shipped every row to
/// the panel to draw 12 — and, worse, made the count in the header describe the filtered list
/// while the empty-state check described the unfiltered one.
pub async fn list_models(pool: &PgPool, filter: &ModelFilter) -> Result<Vec<LocalModel>> {
    let capability = filter.capability.as_deref().and_then(|value| match value {
        "tools" => Some("m.supports_tools"),
        "vision" => Some("m.supports_vision"),
        "embeddings" => Some("m.supports_embeddings"),
        "rerank" => Some("m.supports_rerank"),
        _ => None,
    });
    // The column name is chosen from a closed set above and can never carry a `?`, so the
    // capability filter is safe to splice in; every value-bound filter goes through `$n`.
    let capability_clause = capability.map_or(String::new(), |column| format!(" and {column}"));
    let sql = format!(
        "select m.id, m.provider_id, p.name as endpoint_name, m.model_key, m.display_name, \
           m.size_bytes, m.parameter_count, m.quantization, m.context_window, \
           m.supports_tools, m.supports_vision, m.supports_embeddings, m.supports_rerank, \
           m.embedding_dimension, m.status, m.pull_progress, m.pull_message, m.resident, \
           m.last_used_at, m.updated_at \
         from ai_local_models m \
         join ai_providers p on p.id = m.provider_id \
         where ($1::uuid is null or m.provider_id = $1) \
           and ($2::text is null or m.status = $2) \
           and ($3::text is null or m.model_key ilike '%' || $3 || '%' \
                or coalesce(m.display_name, '') ilike '%' || $3 || '%') \
           {capability_clause} \
         order by (m.status = 'available') desc, lower(m.model_key)"
    );
    let rows = sqlx::query_as::<_, LocalModel>(&sql)
        .bind(filter.provider_id)
        .bind(filter.status.as_deref())
        .bind(
            filter
                .search
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Read one model row by (endpoint, key).
pub async fn get_model(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
) -> Result<Option<LocalModel>> {
    let row = sqlx::query_as::<_, LocalModel>(
        "select m.id, m.provider_id, p.name as endpoint_name, m.model_key, m.display_name, \
           m.size_bytes, m.parameter_count, m.quantization, m.context_window, \
           m.supports_tools, m.supports_vision, m.supports_embeddings, m.supports_rerank, \
           m.embedding_dimension, m.status, m.pull_progress, m.pull_message, m.resident, \
           m.last_used_at, m.updated_at \
         from ai_local_models m join ai_providers p on p.id = m.provider_id \
         where m.provider_id = $1 and m.model_key = $2",
    )
    .bind(provider_id)
    .bind(model_key.trim())
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Record (or refresh) what an endpoint says it serves — a **replacing** snapshot, not an append.
///
/// `upsert` rather than insert-or-ignore: the endpoint is the source of truth for these rows, so
/// a re-scan must **overwrite** the previous answer. A local server that dropped a model must stop
/// appearing in the table, and a model whose status moved from `error` to `available` after a
/// successful pull must not keep the failure.
///
/// A row in `pulling` is not overwritten back to `missing` by a scan that ran before the pull
/// finished: the scan is a snapshot of a moment, and the pull's own writes are newer facts. The
/// `on conflict` clause therefore leaves a `pulling` row alone.
///
/// # Why the stale rows go away
///
/// This is the one write in the module that can *remove* a row, and the reason is that the server
/// is the truth: an operator who runs `ollama rm` (or uninstalls a model from LM Studio) makes that
/// model stop existing. An upsert-only snapshot keeps saying `available` forever, so the panel
/// would offer to route to a model that is not on the machine — the sort of wrong answer that
/// surfaces much later as a 404 from the provider itself.
///
/// The delete is deliberately narrow, and each exclusion is a control rather than a detail:
///
/// * **Rows not in the incoming list.** That is the whole point of a snapshot.
/// * **`status = 'pulling'` is never deleted.** The server is downloading those weights *right now*;
///   deleting the row would leave a download running with nothing recording it — the exact defect
///   `reset_for_pull` had. A model that finishes pulling after the scan simply stays, and the next
///   scan will agree with reality.
/// * **An empty scan deletes nothing.** An empty list is ambiguous — it means either "this
///   endpoint serves nothing" or "the scan failed and returned no models" — and those two answers
///   warrant opposite behaviour. Wiping the table on a failed scan would destroy the very record an
///   operator needs to debug it. So emptiness is a no-op and says so.
pub async fn upsert_served_models(
    pool: &PgPool,
    provider_id: Uuid,
    models: &[ServedModel],
) -> Result<usize> {
    if models.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    // One transaction: a partially-applied snapshot would show a table that never existed on any
    // single server, which is the one thing a "what does my endpoint serve" screen must not do.
    let mut tx = pool.begin().await?;
    for model in models {
        let changed = sqlx::query(
            "insert into ai_local_models (id, provider_id, model_key, display_name, size_bytes, \
               parameter_count, quantization, context_window, supports_tools, supports_vision, \
               supports_embeddings, supports_rerank, embedding_dimension, status, updated_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'available', now()) \
             on conflict (provider_id, model_key) do update set \
               display_name = excluded.display_name, \
               size_bytes = excluded.size_bytes, \
               parameter_count = excluded.parameter_count, \
               quantization = excluded.quantization, \
               context_window = excluded.context_window, \
               supports_tools = excluded.supports_tools, \
               supports_vision = excluded.supports_vision, \
               supports_embeddings = excluded.supports_embeddings, \
               supports_rerank = excluded.supports_rerank, \
               embedding_dimension = excluded.embedding_dimension, \
               status = case when ai_local_models.status = 'pulling' then 'pulling' \
                             else 'available' end, \
               pull_progress = case when ai_local_models.status = 'pulling' \
                                    then ai_local_models.pull_progress else 0 end, \
               updated_at = now()",
        )
        .bind(Uuid::new_v4())
        .bind(provider_id)
        .bind(model.model_key.trim())
        .bind(model.display_name.as_deref())
        .bind(model.size_bytes)
        .bind(model.parameter_count)
        .bind(model.quantization.as_deref())
        .bind(model.context_window)
        .bind(model.supports_tools)
        .bind(model.supports_vision)
        .bind(model.supports_embeddings)
        .bind(model.supports_rerank)
        .bind(model.embedding_dimension)
        .execute(&mut *tx)
        .await?;
        written += changed.rows_affected() as usize;
    }
    // Retire what this endpoint no longer serves. Built from the *bound* keys rather than from the
    // caller list so the trim cannot disagree with the upsert loop above about what "current" is.
    let keys: Vec<String> = models
        .iter()
        .map(|model| model.model_key.trim().to_owned())
        .collect();
    sqlx::query(
        "delete from ai_local_models \
          where provider_id = $1 and status <> 'pulling' and model_key <> all($2)",
    )
    .bind(provider_id)
    .bind(&keys)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(written)
}

/// Claim the right to pull a model, or report why not.
///
/// The claim is the row: `insert … on conflict do nothing` plus `rows_affected`. A caller that
/// gets [`PullOutcome::Started`] owns the download and is the only one that should call the
/// server; every other caller gets the row so the panel can explain the no-op.
///
/// # Errors
///
/// An empty key is [`AiHubError::InvalidProvider`] rather than a silently skipped row: a pull with
/// no key would start a download of nothing.
pub async fn begin_pull(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
) -> Result<(PullOutcome, Option<LocalModel>)> {
    let key = model_key.trim();
    if key.is_empty() {
        return Err(AiHubError::InvalidProvider(
            "a pull needs the model's key, for example `llama3.1:8b`".to_owned(),
        ));
    }
    // The claim. `on conflict do nothing` means an existing row in *any* state — available,
    // pulling, error, missing — blocks a second pull, which is what makes the button idempotent
    // rather than a way to start a download that is already running.
    let claimed = sqlx::query(
        "insert into ai_local_models (id, provider_id, model_key, status, pull_progress, updated_at) \
         values ($1, $2, $3, 'pulling', 0, now()) \
         on conflict (provider_id, model_key) do nothing",
    )
    .bind(Uuid::new_v4())
    .bind(provider_id)
    .bind(key)
    .execute(pool)
    .await?;
    if claimed.rows_affected() == 1 {
        let row = get_model(pool, provider_id, key).await?;
        return Ok((PullOutcome::Started, row));
    }
    // Did not win the claim, so the row exists and its status is the reason.
    let row = get_model(pool, provider_id, key).await?;
    let outcome = match row.as_ref().map(|model| model.status.as_str()) {
        Some("available") => PullOutcome::AlreadyAvailable,
        Some("pulling") => PullOutcome::AlreadyPulling,
        // `error` blocks a retry here and is cleared by `mark_pull_failed`/a fresh scan rather
        // than by a second click: the panel offers "Retry" as an explicit action that first
        // resets the row, so a failing pull cannot be retried into a second failing pull by a
        // double-click.
        _ => PullOutcome::AlreadyPulling,
    };
    Ok((outcome, row))
}

/// Move a pulling row to `available`.
pub async fn finish_pull(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
    message: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "update ai_local_models set status = 'available', pull_progress = 100, \
           pull_message = $3, updated_at = now() \
         where provider_id = $1 and model_key = $2",
    )
    .bind(provider_id)
    .bind(model_key.trim())
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Move a pulling row to `error`, keeping the server's message verbatim.
///
/// The row is **kept**, not deleted: the request says a failed pull "shows the server's error
/// verbatim plus the fix hint", and a row that vanished on failure would leave the table empty and
/// the operator with no way to see what broke.
pub async fn mark_pull_failed(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
    message: &str,
) -> Result<()> {
    sqlx::query(
        "update ai_local_models set status = 'error', pull_message = $3, updated_at = now() \
         where provider_id = $1 and model_key = $2",
    )
    .bind(provider_id)
    .bind(model_key.trim())
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Update a pulling row's progress from the server's own progress line.
pub async fn update_progress(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
    progress: i32,
    message: Option<&str>,
) -> Result<()> {
    // The clamp is here rather than trusted from the server: `pull_progress` has a check
    // constraint, and a server that reported `137` (a percentage expressed as a fraction, which
    // Ollama has done) would fail the whole update and lose the message with it.
    let clamped = progress.clamp(0, 100);
    sqlx::query(
        "update ai_local_models set status = 'pulling', pull_progress = $3, \
           pull_message = coalesce($4, pull_message), updated_at = now() \
         where provider_id = $1 and model_key = $2 and status = 'pulling'",
    )
    .bind(provider_id)
    .bind(model_key.trim())
    .bind(clamped)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// Restart a failed download: turn the `error` row back into a `pulling` one.
///
/// # Why this UPDATES rather than deletes and re-inserts
///
/// The first cut of this function deleted the `error` row and returned [`PullOutcome::Started`]
/// without ever calling [`begin_pull`], on the reasoning that the row would be "cleared". That is
/// the bug this doc comment exists for: a delete leaves **nothing** to record the new download, so
/// the panel showed the model as `missing` while the server was writing weights to disk — the one
/// state where an operator cannot tell a running download from an absence. The walk
/// `a_failed_pull_keeps_its_row_and_the_server_own_words` caught it by asking for the row back
/// and finding `None` where a `pulling` row was supposed to be.
///
/// The row's identity is also the claim. `update … where status = 'error'` *is* that claim: two
/// operators clicking Retry on the same failed model produce exactly one winner, and the loser is
/// told [`PullOutcome::AlreadyPulling`] rather than starting a second download of the same weights
/// onto the same disk. Same shape as `claim_next_run` in the agent queue: a queue's correctness is
/// decided by which writer won, not by who checked last.
///
/// Separate from [`begin_pull`] because a retry is a **decision** the operator makes by clicking a
/// labelled control, not something a double-click should trigger: the request's QA plan cancels a
/// second pull and removes a model, and neither may leave a row silently re-downloading.
pub async fn reset_for_pull(
    pool: &PgPool,
    provider_id: Uuid,
    model_key: &str,
) -> Result<PullOutcome> {
    let key = model_key.trim();
    let restarted = sqlx::query(
        "update ai_local_models \
           set status = 'pulling', pull_progress = 0, pull_message = null, updated_at = now() \
         where provider_id = $1 and model_key = $2 and status = 'error'",
    )
    .bind(provider_id)
    .bind(key)
    .execute(pool)
    .await?;
    if restarted.rows_affected() == 1 {
        return Ok(PullOutcome::Started);
    }
    // Either there is no row at all (the model was never pulled, so this is an ordinary first
    // pull) or a row exists in a state Retry must not overwrite — an in-flight download, or an
    // `available` model the operator never failed. `begin_pull` is the one place that knows how to
    // claim a missing model and how to explain a refusal, so the answer comes from there.
    let (outcome, _) = begin_pull(pool, provider_id, key).await?;
    Ok(outcome)
}

/// Remove a model from its endpoint.
///
/// # Errors
///
/// [`AiHubError::InvalidProvider`] when the model is pulling: a delete that lands mid-download
/// leaves the server writing into a row nobody is watching, and the download continues anyway.
pub async fn remove_model(pool: &PgPool, provider_id: Uuid, model_key: &str) -> Result<()> {
    let key = model_key.trim();
    let row = get_model(pool, provider_id, key).await?;
    let Some(row) = row else {
        return Err(AiHubError::InvalidProvider(format!(
            "no model `{key}` on this endpoint to remove"
        )));
    };
    if row.status == "pulling" {
        return Err(AiHubError::InvalidProvider(format!(
            "`{key}` is being pulled. Cancel the pull first — removing the row now would leave the \
             download running with nothing recording its progress."
        )));
    }
    sqlx::query("delete from ai_local_models where provider_id = $1 and model_key = $2")
        .bind(provider_id)
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Reset a cancelled pull back to `missing`, so the panel shows it as not installed.
pub async fn cancel_pull(pool: &PgPool, provider_id: Uuid, model_key: &str) -> Result<()> {
    sqlx::query(
        "update ai_local_models set status = 'missing', pull_progress = 0, pull_message = $3, \
           updated_at = now() \
         where provider_id = $1 and model_key = $2 and status = 'pulling'",
    )
    .bind(provider_id)
    .bind(model_key.trim())
    .bind("cancelled")
    .execute(pool)
    .await?;
    Ok(())
}
