//! The scanning pipeline (REQ-010, slice 4).
//!
//! Uploads enter the library `pending`; a scanner looks at them out of band and writes one of
//! `clean`, `flagged`, `skipped` or `error`. The file is stored either way — a scanner outage
//! must never lose an upload — and whether a *flagged* file is actually withheld is
//! [`quarantine`], not the flag.
//!
//! Six decisions hold across this module, and each is a place the obvious shortcut is wrong:
//!
//! * **The scanner is a client, not an engine.** The request says the antivirus engine stays
//!   "a pluggable scan client, not a bundled engine" (REQ-010, *Out*), so there is no
//!   signature database here and no format this module parses beyond a verdict. What the
//!   scanner said is recorded *verbatim* rather than translated, because an operator reading
//!   "threat: Eicar-Test-Signature" learns more than one reading "suspicious".
//! * **A failure is a state, not a propagation.** An unreachable scanner writes `error` on the
//!   file and moves on. It never returns an error to the uploader: the upload succeeded, the
//!   *scan* did not, and those are two different facts with two different audiences.
//! * **A skipped file is not a clean file.** A file above the scanner's size ceiling was never
//!   looked at, and recording it as `clean` is how a "clean library" turns out to be an
//!   unscanned one. It is `skipped`, and the run log counts it separately.
//! * **The claim is a lease, not a flag.** A sweep claims rows with `for update skip locked`
//!   rather than flipping them to an in-flight status, because a status column would need
//!   another value, another migration, and a recovery path for a worker that died holding it.
//!   Two workers cannot claim the same row, and a worker that dies mid-run leaves the row
//!   exactly as it found it.
//! * **Quarantine is a row with a history, not a column.** Releasing closes the row; it never
//!   deletes it. A file flagged twice gets two events, because "was this ever looked at" is
//!   the question a security review actually asks and a single boolean cannot answer it for
//!   the second time.
//! * **A release must say why.** The release reason is not a courtesy field: a release with no
//!   stated reason is a release nobody can review, and the row would be the only record that
//!   the file was ever quarantined.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};

// ---------------------------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------------------------

/// What a scanner said about a file.
///
/// A single-word verdict with the scanner's own words attached, rather than a struct with a
/// signature list: the platform does not interpret findings (it does not ship an engine), it
/// records them and refuses to serve what the scanner refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The scanner looked and found nothing.
    Clean {
        /// The engine's name, for the run log.
        engine: String,
    },
    /// The scanner found something. The file is held.
    Flagged {
        /// The engine's name, for the run log.
        engine: String,
        /// What it said, in its own words.
        detail: String,
    },
    /// The scanner did not answer. The file stays stored and unserved.
    Error {
        /// Why the client gave up, in a sentence.
        detail: String,
    },
    /// Nobody looked: the file is above the scanner's ceiling.
    Skipped {
        /// The ceiling, in megabytes, so the row says *why* it was skipped.
        limit_mb: i32,
    },
}

impl Verdict {
    /// The `scan_status` this verdict writes on the file.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self {
            Self::Clean { .. } => "clean",
            Self::Flagged { .. } => "flagged",
            Self::Error { .. } => "error",
            Self::Skipped { .. } => "skipped",
        }
    }

    /// The `scan_detail` this verdict writes on the file.
    ///
    /// An empty string for a clean scan, deliberately: a file that was scanned and found
    /// nothing carries the same emptiness as a file that was never scanned, and only the
    /// *status* column tells them apart. Writing "no threats found" into the detail column
    /// would make a report that greps for text claim a positive result for every file.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Clean { .. } => "",
            Self::Flagged { detail, .. } | Self::Error { detail } => detail,
            Self::Skipped { .. } => "",
        }
    }

    /// The engine name, for the run log. A verdict that never reached an engine has none.
    #[must_use]
    pub fn engine(&self) -> &str {
        match self {
            Self::Clean { engine } | Self::Flagged { engine, .. } => engine,
            Self::Error { .. } | Self::Skipped { .. } => "",
        }
    }

    /// Whether this verdict holds the file back from being served.
    ///
    /// `clean` and `skipped` are both servable, and that asymmetry is a decision: `skipped`
    /// means the scanner's *size ceiling* declined the file, which is a statement about the
    /// deployment's configuration rather than about the bytes, and refusing to serve a 4 MB
    /// marketing video because nobody raised a scanner limit would be a library nobody uses.
    /// `pending` and `error` are the strict pair — those are the states where nobody has said
    /// the file is safe.
    #[must_use]
    pub fn servable(&self) -> bool {
        matches!(self, Self::Clean { .. } | Self::Skipped { .. })
    }
}

// ---------------------------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------------------------

/// What the client posts and what it expects back.
///
/// The wire shape is the platform's, not the scanner vendor's: a file is identified by its
/// checksum and its name, the bytes are posted raw, and the answer is a verdict. That means
/// any scanner — or any HTTP service that answers this shape — can sit behind the endpoint,
/// which is the whole point of keeping the engine out of the platform.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// File id, so the scanner can quote it back in a finding.
    pub media_id: Uuid,
    /// File name as stored.
    pub filename: String,
    /// Declared content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Hex SHA-256 of the bytes — the identity the scanner keys its cache on.
    pub checksum: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// The raw answer of a scan client, before it is turned into a [`Verdict`].
///
/// Kept separate from the HTTP call so the parsing is testable without a server, and so the
/// rule "an answer we do not understand is an error, never a pass" has exactly one home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResponse {
    /// The scanner's own verdict word, lower-cased.
    pub status: String,
    /// What it said, in its own words.
    pub detail: String,
    /// The engine's name.
    pub engine: String,
}

/// Read a scanner's JSON answer.
///
/// Three rules, and the second is the one that matters:
///
/// 1. **A body that is not JSON, or not an object, is an error** — the caller turns it into
///    [`Verdict::Error`], never into a pass.
/// 2. **A missing or wrongly-typed field is *empty*, not a failure.** A scanner that answers
///    `{"status": "clean"}` is a scanner that works, and refusing its answer because it left
///    out the engine name would put a whole site into `error` for a cosmetic omission. The
///    *status* is the field that decides; the rest is context.
/// 3. **A status that is a number or an object is empty, and therefore unknown, and
///    therefore an error.** `{"status": 200}` from a proxy that answered a different API is
///    exactly the case that must not read as a pass.
#[must_use]
pub fn parse_response(body: &[u8]) -> std::result::Result<ScanResponse, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("the scanner's answer is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the scanner's answer is not a JSON object".to_owned())?;
    let text = |key: &str| object.get(key).and_then(serde_json::Value::as_str).unwrap_or("");
    let status = text("status");
    let status = if status.is_empty() {
        // Some engines answer `result` or `verdict` instead; both are the same field under a
        // different name, and treating the difference as "unknown" would hold every file on a
        // site whose scanner simply spells it differently.
        let alternative = text("result");
        if alternative.is_empty() { text("verdict") } else { alternative }
    } else {
        status
    };
    Ok(ScanResponse {
        status: status.to_owned(),
        detail: text("detail").to_owned(),
        engine: text("engine").to_owned(),
    })
}

/// Reduce a scanner's answer to a [`Verdict`].
///
/// Four rules, each a shortcut that produces a plausible wrong answer:
///
/// 1. **An unrecognised status is an error, never a pass.** A future scanner that starts
///    answering `suspicious` must not have every file marked `clean` by a parser that does
///    not know the word. Defaulting to "unknown means fine" is the single worst default in a
///    security pipeline, so the default here is the refusing one.
/// 2. **A `flagged` answer with no detail is still flagged.** The state is the scanner's to
///    report; refusing to record it because the accompanying sentence is empty would let a
///    terse scanner be defeated by dropping one field.
/// 3. **A clean answer never keeps a detail.** The detail column means "what the scanner
///    found"; a clean file that carries prose there reads as a finding in every report.
/// 4. **A flagged answer keeps the scanner's own words, bounded.** The detail goes into a
///    column a screen renders, so it is length-capped and reduced to one line — a scanner
///    that returns a megabyte of diagnostics must not be able to store a megabyte per file.
#[must_use]
pub fn interpret(response: ScanResponse, default_engine: &str) -> Verdict {
    let engine = if response.engine.trim().is_empty() {
        default_engine.to_owned()
    } else {
        // Engine names go into the run log and a screen; bound them the same way as a detail.
        response.engine.trim().chars().take(ENGINE_NAME_LIMIT).collect()
    };
    match response.status.trim().to_ascii_lowercase().as_str() {
        "clean" | "ok" | "pass" | "safe" => Verdict::Clean { engine },
        "flagged" | "infected" | "malware" | "threat" | "found" | "suspicious" => {
            Verdict::Flagged {
                engine,
                detail: bounded_detail(&response.detail, "the scanner flagged this file"),
            }
        }
        "skipped" | "too_large" => Verdict::Skipped {
            limit_mb: 0,
        },
        // `error` is the scanner's own admission that it did not finish, and it is treated
        // exactly like an answer this module cannot parse: the file is stored and unserved.
        "error" | "failed" => Verdict::Error {
            detail: bounded_detail(&response.detail, "the scanner reported an error"),
        },
        _ => Verdict::Error {
            detail: format!(
                "the scanner answered with an unrecognised status `{}` — treated as a failure \
                 rather than a pass",
                bounded_detail(&response.status, "")
            ),
        },
    }
}

/// Longest engine name stored on a run.
pub const ENGINE_NAME_LIMIT: usize = 80;

/// Longest detail a scan writes to a file row or a quarantine row.
pub const DETAIL_LIMIT: usize = 500;

/// Reduce a scanner's words to one bounded, single-line detail.
///
/// A detail is rendered on a screen, stored in a column and copied into an audit entry, so
/// it is trimmed to [`DETAIL_LIMIT`] characters and flattened onto one line. `fallback` is
/// used when the scanner said nothing, because an empty detail fails the column's check
/// constraint and — more importantly — a flagged file with no explanation is a file an
/// operator cannot act on.
fn bounded_detail(raw: &str, fallback: &str) -> String {
    let flattened: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    let trimmed = flattened.trim();
    let text = if trimmed.is_empty() { fallback } else { trimmed };
    if text.chars().count() <= DETAIL_LIMIT {
        return text.to_owned();
    }
    // Cut on a char boundary and say so, rather than ending mid-word with no indication that
    // anything was dropped.
    let mut out: String = text.chars().take(DETAIL_LIMIT - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// Smallest scan timeout accepted, in seconds.
pub const MIN_TIMEOUT_SECONDS: i32 = 1;

/// Largest scan timeout accepted, in seconds.
pub const MAX_TIMEOUT_SECONDS: i32 = 120;

/// Smallest per-site scan size ceiling, in megabytes.
pub const MIN_SCAN_MB: i32 = 1;

/// Largest per-site scan size ceiling, in megabytes.
pub const MAX_SCAN_MB: i32 = 1024;

/// The same ceiling as a `u64`, for the byte comparison.
pub const MAX_SCAN_MB_U64: u64 = MAX_SCAN_MB as u64;

/// What a site's scanning does.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SiteScan {
    /// Site this record belongs to.
    pub site_id: Uuid,
    /// Whether uploads are scanned at all.
    pub enabled: bool,
    /// Where the scanner lives; empty when scanning is off.
    pub endpoint: String,
    /// The environment variable the shared secret is read from — a reference, never a value.
    pub secret_env: String,
    /// How long one scan may take, 1-120 seconds.
    pub timeout_seconds: i32,
    /// `hold` refuses to serve a file whose scan did not complete; `serve` serves it anyway.
    pub on_error: String,
    /// Files above this size are `skipped`, not `error`, in megabytes.
    pub max_scan_mb: i32,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When it was last written.
    pub updated_at: OffsetDateTime,
}

/// A scanning policy as the caller describes it.
///
/// Every field is optional, so a client that PUTs a partial body keeps the fields it did not
/// send. A settings form that posts six of ten fields and resets the other four is how a site
/// silently loses its scanner endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewSiteScan {
    /// Whether uploads are scanned.
    pub enabled: Option<bool>,
    /// Where the scanner lives.
    pub endpoint: Option<String>,
    /// The environment variable the shared secret is read from.
    pub secret_env: Option<String>,
    /// How long one scan may take.
    pub timeout_seconds: Option<i32>,
    /// What an unreachable scanner means.
    pub on_error: Option<String>,
    /// Files above this size are skipped.
    pub max_scan_mb: Option<i32>,
}

impl NewSiteScan {
    /// The platform defaults, as an editable starting point.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            enabled: Some(false),
            endpoint: Some(String::new()),
            secret_env: Some(String::new()),
            timeout_seconds: Some(30),
            on_error: Some("hold".to_owned()),
            max_scan_mb: Some(100),
        }
    }
}

impl SiteScan {
    /// A disabled site's policy, for a row that does not exist yet.
    ///
    /// Same shape as a stored row rather than a special "no policy" value, so a caller that
    /// forgot to read the row still gets a coherent answer.
    #[must_use]
    pub fn platform_defaults(site_id: Uuid) -> Self {
        let defaults = NewSiteScan::defaults();
        let now = OffsetDateTime::now_utc();
        Self {
            site_id,
            enabled: defaults.enabled.unwrap_or(false),
            endpoint: defaults.endpoint.unwrap_or_default(),
            secret_env: defaults.secret_env.unwrap_or_default(),
            timeout_seconds: defaults.timeout_seconds.unwrap_or(30),
            on_error: defaults.on_error.unwrap_or_else(|| "hold".to_owned()),
            max_scan_mb: defaults.max_scan_mb.unwrap_or(100),
            created_at: now,
            updated_at: now,
        }
    }

    /// Whether a file whose scan did not complete may still be served.
    #[must_use]
    pub fn serves_on_error(&self) -> bool {
        self.on_error == "serve"
    }

    /// Whether a file this large goes to the scanner at all.
    #[must_use]
    pub fn scans_size(&self, size_bytes: u64) -> bool {
        size_bytes <= self.max_scan_mb.saturating_mul(1024 * 1024).max(0) as u64
    }
}

/// The columns of a settings row, in the order [`SiteScan`] reads them.
const SCAN_SETTINGS_COLUMNS: &str =
    "site_id, enabled, endpoint, secret_env, timeout_seconds, on_error, max_scan_mb, \
     created_at, updated_at";

/// Read a site's scanning policy, falling back to the platform defaults.
///
/// The fallback exists so a site whose row was never written still answers with a coherent
/// policy rather than an error — but the migration's trigger makes that state unreachable on
/// a healthy database, and the screen says so rather than letting a reader believe a missing
/// row is a decision.
pub async fn read_scan_settings(pool: &PgPool, site_id: Uuid) -> Result<SiteScan> {
    let sql = format!(
        "select {SCAN_SETTINGS_COLUMNS} from media_scan_settings where site_id = $1"
    );
    let found = sqlx::query_as::<_, SiteScan>(&sql)
        .bind(site_id)
        .fetch_optional(pool)
        .await?;
    Ok(found.unwrap_or_else(|| SiteScan::platform_defaults(site_id)))
}

/// Write a site's scanning policy, folding a partial body onto the stored row.
pub async fn write_scan_settings(
    pool: &PgPool,
    site_id: Uuid,
    new: NewSiteScan,
) -> Result<SiteScan> {
    let current = read_scan_settings(pool, site_id).await?;
    let merged = NewSiteScan {
        enabled: new.enabled.or(Some(current.enabled)),
        endpoint: new.endpoint.or(Some(current.endpoint)),
        secret_env: new.secret_env.or(Some(current.secret_env)),
        timeout_seconds: new.timeout_seconds.or(Some(current.timeout_seconds)),
        on_error: new.on_error.or(Some(current.on_error)),
        max_scan_mb: new.max_scan_mb.or(Some(current.max_scan_mb)),
    };
    let merged = validate_scan_settings(merged)?;

    let sql = format!(
        "insert into media_scan_settings \
           (site_id, enabled, endpoint, secret_env, timeout_seconds, on_error, max_scan_mb) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (site_id) do update set enabled = excluded.enabled, \
           endpoint = excluded.endpoint, secret_env = excluded.secret_env, \
           timeout_seconds = excluded.timeout_seconds, on_error = excluded.on_error, \
           max_scan_mb = excluded.max_scan_mb, updated_at = now() \
         returning {SCAN_SETTINGS_COLUMNS}"
    );
    let row = sqlx::query_as::<_, SiteScan>(&sql)
        .bind(site_id)
        .bind(merged.enabled.unwrap_or(false))
        .bind(&merged.endpoint.unwrap_or_default())
        .bind(&merged.secret_env.unwrap_or_default())
        .bind(merged.timeout_seconds.unwrap_or(30))
        .bind(&merged.on_error.unwrap_or_else(|| "hold".to_owned()))
        .bind(merged.max_scan_mb.unwrap_or(100))
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Reduce a submitted policy to valid values, naming the field it refused.
///
/// Every refusal carries the wire name of the setting, because the screen renders the
/// message *under that input* and a bare "constraint violation" is a message under nothing.
/// The rules are deliberately the same set the migration's check constraints carry: a value
/// the API accepts and the database refuses is a 500 a caller cannot act on.
pub fn validate_scan_settings(new: NewSiteScan) -> Result<NewSiteScan> {
    let enabled = new.enabled.unwrap_or(false);
    let endpoint = new.endpoint.unwrap_or_default().trim().to_owned();
    let secret_env = new.secret_env.unwrap_or_default().trim().to_owned();

    if enabled && endpoint.is_empty() {
        return Err(MediaError::InvalidScanSetting {
            field: "endpoint".to_owned(),
            reason: "turning scanning on needs a scanner endpoint".to_owned(),
        });
    }
    if !endpoint.is_empty() && !is_bare_origin(&endpoint) {
        return Err(MediaError::InvalidScanSetting {
            field: "endpoint".to_owned(),
            reason: "the scanner endpoint is a bare origin — `https://scanner.internal:3310`, with no \
             path, query or fragment"
                .to_owned(),
        });
    }
    // A secret *reference* is a variable name, so the shape is a name and not a value: a
    // string with a space, an `=` or a slash in it is somebody pasting a key into the wrong
    // field, and accepting it would store a credential in a column every site owner can read.
    if !secret_env.is_empty() && !is_env_name(&secret_env) {
        return Err(MediaError::InvalidScanSetting {
            field: "secret_env".to_owned(),
            reason: "the scanner secret is a reference: the *name* of the environment variable that \
             holds it, such as `MEDIA_SCAN_SECRET` — not the secret itself"
                .to_owned(),
        });
    }

    let timeout_seconds = new.timeout_seconds.unwrap_or(30);
    if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
        return Err(MediaError::InvalidScanSetting {
            field: "timeout_seconds".to_owned(),
            reason: format!(
                "the scan timeout is {MIN_TIMEOUT_SECONDS}-{MAX_TIMEOUT_SECONDS} seconds, \
                 not {timeout_seconds}"
            ),
        });
    }

    let on_error = new.on_error.unwrap_or_else(|| "hold".to_owned());
    if on_error != "hold" && on_error != "serve" {
        return Err(MediaError::InvalidScanSetting {
            field: "on_error".to_owned(),
            reason: "`on_error` is `hold` or `serve`, not `{on_error}`".to_owned(),
        });
    }

    let max_scan_mb = new.max_scan_mb.unwrap_or(100);
    if !(MIN_SCAN_MB..=MAX_SCAN_MB).contains(&max_scan_mb) {
        return Err(MediaError::InvalidScanSetting {
            field: "max_scan_mb".to_owned(),
            reason: format!(
                "the scan ceiling is {MIN_SCAN_MB}-{MAX_SCAN_MB} MB, not {max_scan_mb}"
            ),
        });
    }

    Ok(NewSiteScan {
        enabled: Some(enabled),
        endpoint: Some(endpoint),
        secret_env: Some(secret_env),
        timeout_seconds: Some(timeout_seconds),
        on_error: Some(on_error),
        max_scan_mb: Some(max_scan_mb),
    })
}

/// Whether a string is a bare `scheme://host[:port]` origin.
fn is_bare_origin(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return false;
    };
    !rest.is_empty() && !rest.contains('/') && !rest.contains('?') && !rest.contains('#')
}

/// Whether a string is an environment variable *name*.
///
/// Uppercase letters, digits and underscores, starting with a letter or underscore. A name
/// that does not look like a name is a value somebody pasted into the reference field, and
/// the two must not be interchangeable — a settings row that holds a secret is a settings row
/// that has to be guarded as a credential and scrubbed from every response.
fn is_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------------------------

/// One file the sweep is about to scan.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PendingScan {
    /// The file.
    pub media_id: Uuid,
    /// Object key to read the bytes from.
    pub storage_key: String,
    /// File name as stored.
    pub filename: String,
    /// Declared content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Hex SHA-256 of the bytes.
    pub checksum: String,
    /// Site the file belongs to.
    pub site_id: Uuid,
}

/// How many files one sweep takes at a time.
///
/// A bound, not a preference: a sweep over a site with a hundred thousand pending rows would
/// otherwise hold one statement open for as long as the scanner takes, and the connection it
/// holds belongs to the request that started it.
pub const SWEEP_BATCH: i64 = 50;

/// Claim up to [`SWEEP_BATCH`] pending files of one site for a scan pass.
///
/// The claim is a `select … for update skip locked` inside a transaction, so two workers
/// sweeping the same site cannot claim the same row and one slow worker cannot block the
/// other. The rows are **not** marked in-flight: a worker that dies between claiming and
/// writing leaves the rows exactly as it found them, which is the recovery path, and a status
/// column would need a fourth value and a reaper to clean it up.
pub async fn claim_pending(pool: &PgPool, site_id: Uuid, limit: i64) -> Result<Vec<PendingScan>> {
    let mut tx = pool.begin().await?;
    // The first column is **aliased**, not selected bare: `FromRow` maps by column *name*, and
    // the row type calls it `media_id` because that is what the whole pipeline calls it. A
    // bare `id` decodes to nothing and the sweep dies with `no column found for name: media_id`
    // on the first claim — which reads as a broken query rather than a missing alias, and only
    // shows up once a site actually has a pending file.
    let rows: Vec<PendingScan> = sqlx::query_as(
        "select id as media_id, storage_key, filename, content_type, size_bytes, checksum, \
                site_id \
           from media \
          where site_id = $1 and scan_status = 'pending' and deleted_at is null \
          order by created_at, id \
          limit $2 \
          for update skip locked",
    )
    .bind(site_id)
    .bind(limit.clamp(1, SWEEP_BATCH))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// What one pass over one site wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepCounts {
    /// Files whose status was written (clean or flagged).
    pub scanned: i64,
    /// Files the scanner flagged.
    pub flagged: i64,
    /// Files whose scan could not complete.
    pub errors: i64,
    /// Files above the scanner's size ceiling, which nobody looked at.
    pub skipped: i64,
}

impl SweepCounts {
    /// Whether the pass found anything an operator must look at.
    ///
    /// A pass whose only result was errors is *not* a clean pass, and reporting it as one is
    /// how an operator concludes their library is safe on the morning the scanner was down.
    #[must_use]
    pub fn outcome(&self) -> &'static str {
        if self.errors > 0 {
            "error"
        } else if self.flagged > 0 {
            "flagged"
        } else {
            "clean"
        }
    }
}

/// Write a verdict onto a file, and open a quarantine row when it is a flag.
///
/// One statement for the status, one for the quarantine, both in the same transaction: a
/// file marked `flagged` with no quarantine row is a file the library shows as held and
/// serves anyway, which is the worst of the two readings and neither of them.
///
/// The quarantine row is **not** written when the file already has an open one. A sweep that
/// re-reads a pending row twice would otherwise stack two open quarantines on one file, and
/// the quarantine list would show it twice and a release would close only one.
pub async fn apply_verdict(
    pool: &PgPool,
    site_id: Uuid,
    scan: &PendingScan,
    verdict: &Verdict,
    run_id: Option<Uuid>,
    actor: Option<Uuid>,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    // The engine name rides on the same statement as the status: two statements would let a
    // row carry the verdict of one engine and the name of another, and a report asking "which
    // engine flagged this" would answer with whatever ran last.
    sqlx::query(
        "update media \
            set scan_status = $2, scan_detail = $3, scanned_at = now(), scan_engine = $4 \
          where id = $1",
    )
    .bind(scan.media_id)
    .bind(verdict.status())
    .bind(verdict.detail())
    .bind(verdict.engine())
    .execute(&mut *tx)
    .await?;

    if matches!(verdict, Verdict::Flagged { .. }) {
        sqlx::query(
            "insert into media_quarantines (media_id, site_id, detail, run_id, quarantined_by) \
             select $1, $2, $3, $4, $5 \
              where not exists ( \
                  select 1 from media_quarantines where media_id = $1 and released_at is null)",
        )
        .bind(scan.media_id)
        .bind(site_id)
        .bind(verdict.detail())
        .bind(run_id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Open a run row and return its id, so every file it touches can name the run that did it.
pub async fn begin_run(
    pool: &PgPool,
    site_id: Uuid,
    kind: &str,
    endpoint: &str,
    engine: &str,
    actor: Option<Uuid>,
) -> Result<Uuid> {
    let kind = if ["scan", "rescan", "manual"].contains(&kind) {
        kind
    } else {
        "scan"
    };
    let id: (Uuid,) = sqlx::query_as(
        "insert into media_scan_runs (site_id, kind, endpoint, engine, actor_user_id) \
         values ($1, $2, $3, $4, $5) returning id",
    )
    .bind(site_id)
    .bind(kind)
    .bind(endpoint)
    .bind(engine)
    .bind(actor)
    .fetch_one(pool)
    .await?;
    Ok(id.0)
}

/// Close a run row with what it wrote.
///
/// A run that never finishes is a run an operator sees as "still going" — which is the
/// honest state, and is why `finished_at` is nullable rather than defaulted to `now()`.
pub async fn finish_run(pool: &PgPool, run_id: Uuid, counts: &SweepCounts) -> Result<()> {
    sqlx::query(
        "update media_scan_runs \
            set scanned = $2, flagged = $3, errors = $4, skipped = $5, \
                outcome = $6, finished_at = now() \
          where id = $1",
    )
    .bind(run_id)
    .bind(counts.scanned)
    .bind(counts.flagged)
    .bind(counts.errors)
    .bind(counts.skipped)
    .bind(counts.outcome())
    .execute(pool)
    .await?;
    Ok(())
}

/// One run, as the log and the settings screen read it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ScanRun {
    /// Run id.
    pub id: Uuid,
    /// Site the run swept.
    pub site_id: Uuid,
    /// `scan`, `rescan` or `manual`.
    pub kind: String,
    /// `clean`, `flagged` or `error`.
    pub outcome: String,
    /// Files whose status was written.
    pub scanned: i64,
    /// Files the scanner flagged.
    pub flagged: i64,
    /// Files whose scan could not complete.
    pub errors: i64,
    /// Files above the size ceiling.
    pub skipped: i64,
    /// The endpoint the client used.
    pub endpoint: String,
    /// The engine the scanner named.
    pub engine: String,
    /// Who started the run, when an operator did.
    pub actor_user_id: Option<Uuid>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished; null while it is still running.
    pub finished_at: Option<OffsetDateTime>,
}

impl ScanRun {
    /// Whether the run has finished.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished_at.is_some()
    }

    /// One sentence for the run list, never a number without a word.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.finished() {
            return "still running".to_owned();
        }
        if self.errors > 0 {
            return format!(
                "{} file(s) could not be scanned — the scanner did not answer; {} flagged",
                self.errors, self.flagged
            );
        }
        if self.flagged > 0 {
            return format!("{} file(s) flagged and held", self.flagged);
        }
        if self.scanned == 0 && self.skipped > 0 {
            return format!(
                "{} file(s) above the scanner's size ceiling — none of them were looked at",
                self.skipped
            );
        }
        format!("{} file(s) scanned, none flagged", self.scanned)
    }
}

/// The columns of a run row, in the order [`ScanRun`] reads them.
const RUN_COLUMNS: &str = "id, site_id, kind, outcome, scanned, flagged, errors, skipped, \
     endpoint, engine, actor_user_id, started_at, finished_at";

/// The most recent runs of a site, newest first.
pub async fn list_runs(pool: &PgPool, site_id: Uuid, limit: i64) -> Result<Vec<ScanRun>> {
    let sql = format!(
        "select {RUN_COLUMNS} from media_scan_runs where site_id = $1 \
           order by started_at desc, id limit $2"
    );
    Ok(sqlx::query_as::<_, ScanRun>(&sql)
        .bind(site_id)
        .bind(limit.clamp(1, 200))
        .fetch_all(pool)
        .await?)
}

// ---------------------------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------------------------

/// One open quarantine, as the list and the detail screen read it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Quarantine {
    /// Quarantine id.
    pub id: Uuid,
    /// The held file.
    pub media_id: Uuid,
    /// Site the file belongs to.
    pub site_id: Uuid,
    /// What the scanner said.
    pub detail: String,
    /// The run that produced it, when it came from one.
    pub run_id: Option<Uuid>,
    /// When the file was held.
    pub quarantined_at: OffsetDateTime,
    /// Who held it.
    pub quarantined_by: Option<Uuid>,
    /// When a human let it go, or deleted it.
    pub released_at: Option<OffsetDateTime>,
    /// Who did it.
    pub released_by: Option<Uuid>,
    /// Why it was released or deleted.
    pub release_reason: String,
}

impl Quarantine {
    /// Whether the file is still held.
    #[must_use]
    pub fn open(&self) -> bool {
        self.released_at.is_none()
    }

    /// Whether the release was a deletion rather than a release.
    #[must_use]
    pub fn deleted(&self) -> bool {
        self.release_reason.trim().eq_ignore_ascii_case("deleted")
    }
}

/// The columns of a quarantine row, in the order [`Quarantine`] reads them.
const QUARANTINE_COLUMNS: &str = "id, media_id, site_id, detail, run_id, quarantined_at, \
     quarantined_by, released_at, released_by, release_reason";

/// The open quarantines of a site, newest first.
pub async fn list_quarantines(pool: &PgPool, site_id: Uuid) -> Result<Vec<Quarantine>> {
    let sql = format!(
        "select {QUARANTINE_COLUMNS} from media_quarantines \
          where site_id = $1 and released_at is null order by quarantined_at desc, id"
    );
    Ok(sqlx::query_as::<_, Quarantine>(&sql)
        .bind(site_id)
        .fetch_all(pool)
        .await?)
}

/// How many files of a site are currently held, and how many bytes they occupy.
///
/// Both numbers, because "one file" and "one file of 900 MB" are different urgencies, and a
/// list that shows only the count makes a quarantined video look like a quarantined icon.
pub async fn quarantine_totals(pool: &PgPool, site_id: Uuid) -> Result<(i64, i64)> {
    // `sum(bigint)` returns NUMERIC in PostgreSQL, which sqlx will not decode into an `i64`
    // — and the obvious `sum(m.size_bytes)::bigint` overflows instead of refusing, so a
    // library past 8 EiB would wrap rather than say so. `coalesce(sum(...), 0)::bigint`
    // rounds rather than truncating, which is the right direction for a byte total: being a
    // fraction over is better than being under.
    let row: (i64, i64) = sqlx::query_as(
        "select count(*)::bigint, coalesce(sum(m.size_bytes), 0)::bigint \
           from media_quarantines q \
           join media m on m.id = q.media_id \
          where q.site_id = $1 and q.released_at is null",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Whether a file is currently held.
pub async fn is_quarantined(pool: &PgPool, media_id: Uuid) -> Result<bool> {
    let row: (bool,) = sqlx::query_as(
        "select exists (select 1 from media_quarantines \
                          where media_id = $1 and released_at is null)",
    )
    .bind(media_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Close an open quarantine, for a release or a deletion.
///
/// `reason` is required and must name what happened. A release with no stated reason is a
/// release nobody can review, and this row is the only record the file was ever held — so the
/// reason is the record.
///
/// The file's `scan_status` moves with it: a released file is no longer `flagged` unless a
/// later scan says so, and a file left reading `flagged` after its quarantine was closed is a
/// file the library shows as held and serves — the reverse of the bug this module exists to
/// prevent. A release does **not** write `clean`: nobody has said the file is safe, only that
/// a human decided to serve it, and `skipped` is the honest state for "we are not going to
/// scan this again".
pub async fn close_quarantine(
    pool: &PgPool,
    quarantine_id: Uuid,
    actor: Uuid,
    reason: &str,
) -> Result<Option<Quarantine>> {
    let reason = release_reason(reason)?;
    let mut tx = pool.begin().await?;

    let closed: Option<(Uuid,)> = sqlx::query_as(
        "update media_quarantines \
            set released_at = now(), released_by = $2, release_reason = $3 \
          where id = $1 and released_at is null returning media_id",
    )
    .bind(quarantine_id)
    .bind(actor)
    .bind(&reason)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((media_id,)) = closed {
        sqlx::query(
            "update media set scan_status = 'skipped', \
                    scan_detail = 'released from quarantine without a further scan' \
              where id = $1 and scan_status = 'flagged'",
        )
        .bind(media_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let Some((media_id,)) = closed else {
        return Ok(None);
    };
    let sql = format!("select {QUARANTINE_COLUMNS} from media_quarantines where id = $1");
    let row = sqlx::query_as::<_, Quarantine>(&sql)
        .bind(quarantine_id)
        .fetch_one(pool)
        .await?;
    debug_assert_eq!(row.media_id, media_id, "the row we just closed");
    Ok(Some(row))
}

/// Reduce a release reason to something worth storing.
///
/// Two reasons are structural — `released` and `deleted` — and anything else is an
/// operator's sentence. The check constraint on the column is `detail <> ''`, and a release
/// that stored an empty reason would be a release that cannot be reviewed, so an empty
/// reason is refused here rather than defaulted: the default would be a *lie* the audit
/// trail would carry for ever.
fn release_reason(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MediaError::InvalidReleaseReason);
    }
    let mut out: String = trimmed.chars().take(RELEASE_REASON_LIMIT).collect();
    if trimmed.chars().count() > RELEASE_REASON_LIMIT {
        out.push('…');
    }
    Ok(out)
}

/// Longest release reason stored.
pub const RELEASE_REASON_LIMIT: usize = 300;

// ---------------------------------------------------------------------------------------------
// Serve gating
// ---------------------------------------------------------------------------------------------

/// Why a file may not be served right now.
///
/// The strict half of "scanning is best-effort at ingest, strict at serve" (REQ-010 *Risks*):
/// a scanner outage must not lose an upload, and an *unscanned* file must not be publicly
/// readable until it clears. Those two statements are only compatible if the outage is
/// visible, which is what this type makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeRefusal {
    /// The file is in the trash.
    Trashed,
    /// The file is held in quarantine.
    Quarantined,
    /// The file has not been scanned and the site's policy says to hold.
    NotScanned,
    /// The scan could not complete and the site's policy says to hold.
    ScanFailed,
}

impl ServeRefusal {
    /// The wire code for this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Trashed => "file_trashed",
            Self::Quarantined => "file_quarantined",
            Self::NotScanned => "file_not_scanned",
            Self::ScanFailed => "file_scan_failed",
        }
    }

    /// The sentence a caller gets, which never names the scanner's finding to somebody who
    /// should not see it.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::Trashed => "this file is in the trash",
            Self::Quarantined => {
                "this file is held in quarantine and cannot be served until an administrator \
                 releases it"
            }
            Self::NotScanned => {
                "this file has not been scanned yet and its site's scanning policy does not \
                 serve unscanned files"
            }
            Self::ScanFailed => {
                "this file's scan could not be completed and its site's scanning policy does \
                 not serve files with an unfinished scan"
            },
        }
    }

    /// Whether an operator — somebody who can already read the library — should see more.
    ///
    /// The four refusals are one code for the public serve path and another for the panel,
    /// because "this file is held in quarantine" is an answer a visitor does not need and a
    /// person with `media.manage` does. The distinction is made by the *caller*, not here:
    /// one code answers the outside world, the other answers somebody who can act on it.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !matches!(self, Self::Trashed)
    }
}

/// Decide whether a file may be served, from its scan state and the site's policy.
///
/// The policy is an argument rather than a read, because the two call sites — the panel's raw
/// route and the share route — have already loaded the site, and reading the policy again per
/// byte range would turn one row read into two.
///
/// **A flag is a fact, not a policy question.** The policy decides what an *absent* verdict
/// means — `pending` and `error` are the two states where nobody has said the file is safe, and
/// they are what `on_error` speaks to. It never speaks to `flagged`: a file the scanner
/// rejected is rejected whoever is asking, and switching the scanner off afterwards is a
/// decision about *future* uploads, not a way of forgetting a past one. Getting this backwards
/// is what the share suite caught: with scanning disabled, a link kept serving a file the
/// platform had already recorded as infected.
pub fn may_serve(
    scan_status: &str,
    quarantined: bool,
    policy: &SiteScan,
) -> std::result::Result<(), ServeRefusal> {
    if quarantined {
        return Err(ServeRefusal::Quarantined);
    }
    match scan_status {
        "clean" | "skipped" => Ok(()),
        // A quarantine beats everything, and a flag is not a policy question at all.
        "flagged" => Err(ServeRefusal::Quarantined),
        // The two states below are the ones `on_error` speaks to, and they only mean anything
        // on a site that asked for scanning: on a site that did not, `pending` is what every
        // row reads until the pipeline exists and `error` can only have been written by
        // somebody who did enable it and then turned it off — and refusing that file would
        // strand it forever with no way back except re-enabling the scanner.
        "error" if policy.enabled => {
            if policy.serves_on_error() {
                Ok(())
            } else {
                Err(ServeRefusal::ScanFailed)
            }
        }
        "error" => Ok(()),
        // `pending` is the only other state the check constraint allows. An unknown state —
        // a hand-edited row, or one written by a future release — falls here too, and is
        // treated as *not scanned*, because the alternative is serving a file nobody has
        // looked at.
        _ if policy.enabled => Err(ServeRefusal::NotScanned),
        _ => Ok(()),
    }
}

/// The bytes identity a scan client posts, as a stable header value.
///
/// Exposed so a client and a test can agree on what "the same file" means without both
/// re-implementing it: the checksum is the identity, and the name is context.
#[must_use]
pub fn scan_identity(checksum: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(checksum.trim().to_ascii_lowercase().as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: &str, detail: &str) -> ScanResponse {
        ScanResponse {
            status: status.to_owned(),
            detail: detail.to_owned(),
            engine: String::new(),
        }
    }

    // -- the verdict -----------------------------------------------------------------------

    /// A clean answer writes `clean` and an *empty* detail.
    ///
    /// The empty detail is the point: a report that greps `scan_detail` for text must not
    /// find "no threats found" on every clean file and conclude a scan happened when the
    /// column is what says a scan happened.
    #[test]
    fn a_clean_scan_leaves_the_detail_column_empty() {
        let verdict = interpret(response("clean", "no threats found"), "engine-a");
        assert_eq!(verdict.status(), "clean");
        assert_eq!(verdict.detail(), "");
        assert!(verdict.servable());
    }

    /// The scanner's own words are recorded, not a translation of them.
    #[test]
    fn a_flagged_file_keeps_the_scanner_s_own_words() {
        let verdict = interpret(
            response("infected", "threat: Eicar-Test-Signature found in the archive"),
            "engine-a",
        );
        assert_eq!(verdict.status(), "flagged");
        assert_eq!(
            verdict.detail(),
            "threat: Eicar-Test-Signature found in the archive"
        );
        assert!(!verdict.servable());
    }

    /// An answer the module does not understand is a failure, never a pass.
    ///
    /// The single worst default in a security pipeline is "unknown means fine": a scanner
    /// that starts answering `suspicious` would have every file marked clean by a parser
    /// that has not caught up, and the library would report itself clean the whole time.
    #[test]
    fn an_unrecognised_status_is_an_error_and_never_a_pass() {
        for status in ["maybe", "", "OK!", "not-sure", "quarantined??"] {
            let verdict = interpret(response(status, ""), "engine-a");
            assert_eq!(
                verdict.status(),
                "error",
                "`{status}` must not read as a pass"
            );
            assert!(!verdict.servable(), "`{status}` must not be servable");
        }
    }

    /// Every word the client understands is *held*, whatever the scanner called it.
    ///
    /// The set of synonyms is the point: a vendor scanner that answers `suspicious` and one
    /// that answers `infected` are describing the same outcome, and a pipeline that only
    /// understood the first would serve every file the second caught. So the assertion is
    /// about the *outcome* — held, not servable — rather than about one spelling, which is
    /// the thing that changes when somebody swaps the engine behind the endpoint.
    #[test]
    fn every_recognised_detection_word_holds_the_file() {
        for status in [
            "flagged", "infected", "malware", "threat", "found", "suspicious", "FLAGGED",
        ] {
            let verdict = interpret(response(status, "engine says so"), "engine-a");
            assert_eq!(verdict.status(), "flagged", "`{status}`");
            assert!(!verdict.servable(), "`{status}` must be held");
            assert_eq!(verdict.detail(), "engine says so", "`{status}` keeps the words");
        }
    }

    /// A flagged answer with no sentence is still flagged.
    ///
    /// A terse scanner must not be defeated by dropping one field: the *state* is the
    /// scanner's to report, and a parser that insists on a detail would silently convert a
    /// detection into a parse error and serve the file.
    #[test]
    fn a_flag_with_no_detail_is_still_a_flag() {
        let verdict = interpret(response("flagged", "   "), "engine-a");
        assert_eq!(verdict.status(), "flagged");
        assert_eq!(verdict.detail(), "the scanner flagged this file");
    }

    /// A detail is one bounded line, because it is rendered and copied into audit rows.
    #[test]
    fn a_detail_is_flattened_and_bounded() {
        let long = "x".repeat(DETAIL_LIMIT * 2);
        let verdict = interpret(response("flagged", &format!("line one\nline two\t{long}")), "e");
        let detail = verdict.detail();
        assert!(!detail.contains('\n'), "a detail is one line");
        assert!(!detail.contains('\t'));
        assert!(detail.chars().count() <= DETAIL_LIMIT, "bounded to one screen");
        assert!(detail.ends_with('…'), "a truncated detail says so");
    }

    /// The engine name is bounded and defaults to the configured one.
    #[test]
    fn the_engine_name_is_bounded_and_defaulted() {
        assert_eq!(interpret(response("clean", ""), "fallback").engine(), "fallback");
        let long = "e".repeat(ENGINE_NAME_LIMIT * 3);
        let response = ScanResponse {
            status: "clean".to_owned(),
            detail: String::new(),
            engine: long,
        };
        assert_eq!(interpret(response, "fallback").engine().chars().count(), ENGINE_NAME_LIMIT);
    }

    /// `skipped` is servable and `error` is not — the asymmetry is a decision, not an
    /// oversight, and it is the one an operator argues about.
    #[test]
    fn skipped_is_servable_but_error_is_not() {
        assert!(Verdict::Skipped { limit_mb: 100 }.servable());
        assert!(!Verdict::Error { detail: "no answer".into() }.servable());
        assert!(!Verdict::Flagged { engine: "e".into(), detail: "d".into() }.servable());
    }

    // -- settings --------------------------------------------------------------------------

    fn policy(enabled: bool, endpoint: &str, on_error: &str) -> SiteScan {
        SiteScan {
            site_id: Uuid::nil(),
            enabled,
            endpoint: endpoint.to_owned(),
            secret_env: String::new(),
            timeout_seconds: 30,
            on_error: on_error.to_owned(),
            max_scan_mb: 100,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// Turning scanning on with no endpoint is refused, and the message names the field.
    #[test]
    fn enabling_scanning_needs_an_endpoint() {
        let error = validate_scan_settings(NewSiteScan {
            enabled: Some(true),
            endpoint: Some(String::new()),
            ..NewSiteScan::defaults()
        })
        .expect_err("an enabled scanner with nowhere to send bytes");
        assert!(error.to_string().contains("endpoint"), "names the field");
    }

    /// An endpoint is a bare origin: a path in it is a 404 at 03:00.
    #[test]
    fn the_endpoint_must_be_a_bare_origin() {
        for bad in [
            "https://scanner.internal:3310/scan",
            "https://scanner.internal:3310?a=b",
            "https://scanner.internal:3310#x",
            "scanner.internal:3310",
            "ftp://scanner.internal",
        ] {
            let error = validate_scan_settings(NewSiteScan {
                enabled: Some(true),
                endpoint: Some(bad.to_owned()),
                ..NewSiteScan::defaults()
            })
            .expect_err("`{bad}` is not a bare origin");
            assert!(error.to_string().contains("endpoint"));
        }
        assert!(
            validate_scan_settings(NewSiteScan {
                enabled: Some(true),
                endpoint: Some("http://127.0.0.1:3310".to_owned()),
                ..NewSiteScan::defaults()
            })
            .is_ok()
        );
    }

    /// The secret field holds a *name*, and a pasted key is refused.
    ///
    /// A settings row that holds key material has to be guarded as a credential and scrubbed
    /// from every response, and this is the field where somebody pastes one by mistake. The
    /// refusal says so in words rather than silently dropping it.
    #[test]
    fn the_secret_field_refuses_a_value_and_takes_a_name() {
        for pasted in [
            "s3cr3t-value-with-dashes",
            "value with spaces",
            "KEY=abc",
            "path/like/this",
            "123LEADINGDIGIT",
        ] {
            let error = validate_scan_settings(NewSiteScan {
                secret_env: Some(pasted.to_owned()),
                ..NewSiteScan::defaults()
            })
            .expect_err("`{pasted}` is a value, not a name");
            assert!(error.to_string().contains("secret_env"));
        }
        for name in ["MEDIA_SCAN_SECRET", "_internal", "scan2"] {
            assert!(
                validate_scan_settings(NewSiteScan {
                    secret_env: Some(name.to_owned()),
                    ..NewSiteScan::defaults()
                })
                .is_ok(),
                "`{name}` is a name"
            );
        }
    }

    /// Every out-of-range number names its own field.
    #[test]
    fn out_of_range_settings_name_their_field() {
        for (field, value) in [
            ("timeout_seconds", NewSiteScan {
                timeout_seconds: Some(0),
                ..NewSiteScan::defaults()
            }),
            ("timeout_seconds", NewSiteScan {
                timeout_seconds: Some(121),
                ..NewSiteScan::defaults()
            }),
            ("on_error", NewSiteScan {
                on_error: Some("ignore".to_owned()),
                ..NewSiteScan::defaults()
            }),
            ("max_scan_mb", NewSiteScan {
                max_scan_mb: Some(0),
                ..NewSiteScan::defaults()
            }),
            ("max_scan_mb", NewSiteScan {
                max_scan_mb: Some(1025),
                ..NewSiteScan::defaults()
            }),
        ] {
            let error = validate_scan_settings(value).expect_err("out of range");
            assert!(
                error.to_string().contains(field),
                "the message names `{field}`"
            );
        }
    }

    /// The size ceiling is a byte comparison, not a megabyte comparison.
    #[test]
    fn the_size_ceiling_is_compared_in_bytes() {
        let site = policy(true, "http://s:1", "hold");
        let mb = 1024 * 1024;
        assert!(site.scans_size(1));
        assert!(site.scans_size(100 * mb), "exactly the ceiling still scans");
        assert!(!site.scans_size(100 * mb + 1), "one byte over does not");
    }

    // -- serve gating ----------------------------------------------------------------------

    /// The four states, and the policy that decides the two arguable ones.
    #[test]
    fn the_serve_gate_follows_the_sites_policy() {
        let hold = policy(true, "http://s:1", "hold");
        let serve = policy(true, "http://s:1", "serve");

        assert!(may_serve("clean", false, &hold).is_ok());
        assert!(may_serve("skipped", false, &hold).is_ok());
        assert_eq!(
            may_serve("pending", false, &hold),
            Err(ServeRefusal::NotScanned)
        );
        assert_eq!(
            may_serve("error", false, &hold),
            Err(ServeRefusal::ScanFailed)
        );
        // `serve` is the operator's risk decision about their own site, and it is the only
        // thing that changes the `error` answer.
        assert!(may_serve("error", false, &serve).is_ok());
        assert_eq!(
            may_serve("pending", false, &serve),
            Err(ServeRefusal::NotScanned),
            "serving on error is not serving unscanned files"
        );
    }

    /// A site that never turned scanning on serves everything, because `pending` is what
    /// every row reads until the pipeline exists.
    #[test]
    fn a_site_that_asked_for_no_scanning_serves_an_unscanned_file() {
        let off = policy(false, "", "hold");
        assert!(may_serve("pending", false, &off).is_ok());
        assert!(may_serve("error", false, &off).is_ok());
    }

    /// **A flag is a fact, not a policy question.**
    ///
    /// This is the case the share suite caught: with scanning switched off *after* a file was
    /// flagged, the first version of the gate let the link keep serving it. Turning the
    /// scanner off is a decision about future uploads; it is not a way of forgetting a
    /// verdict somebody already reached, and `on_error: serve` is about the *absent* verdict
    /// — it has never been a way to overrule one that is present.
    #[test]
    fn a_flag_is_refused_whatever_the_policy_says() {
        for on_error in ["hold", "serve"] {
            for enabled in [true, false] {
                let site = policy(enabled, "http://s:1", on_error);
                assert_eq!(
                    may_serve("flagged", false, &site),
                    Err(ServeRefusal::Quarantined),
                    "enabled={enabled} on_error={on_error} must still hold a flagged file"
                );
            }
        }
    }

    /// A quarantine beats every policy, including `serve on error`.
    #[test]
    fn a_quarantine_beats_the_sites_own_policy() {
        let serve = policy(true, "http://s:1", "serve");
        assert_eq!(
            may_serve("clean", true, &serve),
            Err(ServeRefusal::Quarantined),
            "a released-then-re-flagged file is held whatever the policy says"
        );
    }

    /// A `flagged` file with no open quarantine row is still held.
    ///
    /// The status and the quarantine are two facts, and a row can hold one without the other
    /// — a hand-edited row, or one written before the quarantine table existed. The strict
    /// reading is what keeps a release from being the only way out.
    #[test]
    fn a_flagged_file_is_held_even_without_a_quarantine_row() {
        let site = policy(true, "http://s:1", "hold");
        assert_eq!(
            may_serve("flagged", false, &site),
            Err(ServeRefusal::Quarantined)
        );
    }

    /// An unknown state is not a pass.
    #[test]
    fn an_unknown_scan_state_is_treated_as_unscanned() {
        let site = policy(true, "http://s:1", "serve");
        assert_eq!(
            may_serve("quarantined_by_the_duck_tape_man", false, &site),
            Err(ServeRefusal::NotScanned)
        );
    }

    /// The four refusals have four codes, because a caller sends them to four places.
    #[test]
    fn every_refusal_has_its_own_code_and_a_sentence() {
        let refusals = [
            ServeRefusal::Trashed,
            ServeRefusal::Quarantined,
            ServeRefusal::NotScanned,
            ServeRefusal::ScanFailed,
        ];
        let mut codes: Vec<&str> = refusals.iter().map(ServeRefusal::code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), refusals.len(), "no two refusals share a code");
        for refusal in refusals {
            assert!(!refusal.message().is_empty());
            assert!(refusal.is_actionable() || refusal == ServeRefusal::Trashed);
        }
    }

    // -- the sweep -------------------------------------------------------------------------

    /// A pass whose only result was errors is not a clean pass.
    ///
    /// This is the sentence that matters on the morning the scanner was down: reporting a
    /// failed pass as `clean` is how an operator concludes their library is safe.
    #[test]
    fn a_pass_of_errors_is_not_reported_as_clean() {
        assert_eq!(SweepCounts::default().outcome(), "clean");
        assert_eq!(
            SweepCounts {
                scanned: 4000,
                errors: 4000,
                ..SweepCounts::default()
            }
            .outcome(),
            "error"
        );
        assert_eq!(
            SweepCounts {
                flagged: 1,
                ..SweepCounts::default()
            }
            .outcome(),
            "flagged"
        );
    }

    /// A run's summary is a sentence, and a finished run with nothing to say says so.
    #[test]
    fn a_run_summarises_itself_in_words() {
        let mut run = ScanRun {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            kind: "scan".to_owned(),
            outcome: "clean".to_owned(),
            scanned: 12,
            flagged: 0,
            errors: 0,
            skipped: 0,
            endpoint: String::new(),
            engine: String::new(),
            actor_user_id: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: Some(OffsetDateTime::UNIX_EPOCH),
        };
        assert_eq!(run.summary(), "12 file(s) scanned, none flagged");

        run.finished_at = None;
        assert_eq!(run.summary(), "still running");
        assert!(!run.finished());

        run.finished_at = Some(OffsetDateTime::UNIX_EPOCH);
        run.errors = 40;
        run.flagged = 1;
        assert!(run.summary().contains("did not answer"), "{}", run.summary());

        // A clean pass whose only result was the size ceiling: the sentence has to say
        // "none of them were looked at", or an operator reads a run over 300 MB videos as
        // a scanned library.
        run.errors = 0;
        run.flagged = 0;
        run.scanned = 0;
        run.skipped = 3;
        assert!(
            run.summary().contains("none of them were looked at"),
            "{}",
            run.summary()
        );

        // And a flagged pass is reported as a flag even when the run also had errors: the
        // worse of the two is named first, because a held file needs a human today.
        run.flagged = 2;
        run.errors = 7;
        assert!(run.summary().contains("7 file(s) could not be scanned"), "{}", run.summary());
        assert!(run.summary().contains("2 flagged"), "{}", run.summary());
    }

    // -- quarantine ------------------------------------------------------------------------

    /// A release with no reason is refused rather than defaulted.
    ///
    /// A default would be a lie the audit trail carries for ever: the row is the only record
    /// that the file was ever held, so an empty reason is the whole record being empty.
    #[test]
    fn a_release_without_a_reason_is_refused() {
        assert!(matches!(
            release_reason("   "),
            Err(MediaError::InvalidReleaseReason)
        ));
        assert!(matches!(
            release_reason("\n\t"),
            Err(MediaError::InvalidReleaseReason)
        ));
        assert_eq!(release_reason("  released ").expect("trimmed"), "released");
    }

    /// A long reason is bounded, and says that it was cut.
    #[test]
    fn a_release_reason_is_bounded() {
        let reason = release_reason(&"r".repeat(RELEASE_REASON_LIMIT * 2))
            .expect("a reason is stored, bounded");
        assert!(reason.chars().count() <= RELEASE_REASON_LIMIT + 1);
        assert!(reason.ends_with('…'));
    }

    /// A quarantine knows whether it is open, and a deletion is not a release.
    #[test]
    fn a_quarantine_knows_its_own_state() {
        let mut q = Quarantine {
            id: Uuid::new_v4(),
            media_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            detail: "threat: test".to_owned(),
            run_id: None,
            quarantined_at: OffsetDateTime::UNIX_EPOCH,
            quarantined_by: None,
            released_at: None,
            released_by: None,
            release_reason: String::new(),
        };
        assert!(q.open());
        assert!(!q.deleted());

        q.released_at = Some(OffsetDateTime::UNIX_EPOCH);
        q.release_reason = "DELETED".to_owned();
        assert!(!q.open());
        assert!(q.deleted(), "a deletion is not a release, whatever its case");
    }

    // -- identity --------------------------------------------------------------------------

    /// The scan identity is a pure function of the checksum.
    ///
    /// The scanner keys its cache on this, so the same bytes must produce the same value
    /// whatever the file is called — a client that re-uploaded the same picture under a new
    /// name would otherwise re-scan work the scanner already did.
    #[test]
    fn the_scan_identity_is_case_and_name_insensitive() {
        let a = "a".repeat(64);
        assert_eq!(scan_identity(&a), scan_identity(&a.to_uppercase()));
        assert_eq!(scan_identity(&format!(" {a} ")), scan_identity(&a));
        assert_ne!(scan_identity(&a), scan_identity(&"b".repeat(64)));
    }
}
