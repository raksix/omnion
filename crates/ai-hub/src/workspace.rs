//! The per-agent workspace (REQ-099, slice 2).
//!
//! A workspace is the scratch area a run reads from and writes to: the inputs somebody uploaded
//! for it, and the outputs it decided to keep. This module owns the three things that make that
//! safe, and all three are *rules* rather than *conventions* — they are functions a test can
//! call with a value it wrote itself, and the API calls the same function the test called.
//!
//! # The three rules
//!
//! 1. **A path is an identifier, not prose.** It must be relative, at most 512 characters, free
//!    of control characters, and carry no `..` segment. A traversal is not "a path outside the
//!    agent's folder" once the key is derived from a checksum rather than from the path — but
//!    refusing it is still the correct first line, because the *message* a caller gets for
//!    `../../etc/passwd` should say why rather than 404ing a file that was never there.
//!
//! 2. **The caps are arithmetic over rows, never a running sum in memory.** The per-agent 100 MB
//!    ceiling is `sum(size_bytes) where agent_id = $1`, so a process that died mid-upload
//!    leaves a row that already counts against the quota and one that does not. A counter in
//!    process memory is a counter that a restart silently resets, which turns "the agent's
//!    workspace is full" into "the agent's workspace is full until somebody restarts the API".
//!
//! 3. **The storage key is derived, never supplied.** It is a function of the agent id and the
//!    file's bytes, so two files with the same path in two agents never collide, a rename is a
//!    new key rather than a move, and the key stored on the row cannot be edited into another
//!    file's address. The path is what a human types and what a goal quotes; the key is what
//!    the bucket is asked for, and the two are never the same string.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// The longest path a workspace file may have.
///
/// Bounded so a key derived from it stays inside every storage backend's own key limit; the
/// database restates the same number as a check constraint so a row written by anything other
/// than this module cannot exceed it either.
pub const MAX_PATH_CHARS: usize = 512;

/// The per-file ceiling, in bytes (10 MB).
///
/// A workspace holds the inputs of a *goal* — a spreadsheet, a PDF, a log export — and not the
/// output of a build. Anything larger than this belongs in the media library, which has versions
/// and previews and a lifecycle; the workspace is deliberately the smaller, blunter tool.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// The per-agent ceiling, in bytes (100 MB).
///
/// Ten files at the per-file cap, or a thousand small ones. The number is a *policy* expressed
/// as a sum, which is why [`usage`] returns the components rather than only the total: a usage
/// bar that can say "97 of 100 MB, 6 files" tells somebody which half of the pair to act on.
pub const MAX_AGENT_BYTES: u64 = 100 * 1024 * 1024;

/// What a workspace file is, as the store and the panel see it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentFile {
    /// Row identity.
    pub id: Uuid,
    /// The agent that owns it.
    pub agent_id: Uuid,
    /// The run that wrote it, when it was written by one.
    pub run_id: Option<Uuid>,
    /// The human-readable address inside the workspace.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Declared content type.
    pub content_type: String,
    /// The opaque object-storage key. Never shown to a caller.
    pub storage_key: String,
    /// Hex-encoded SHA-256 of the bytes.
    pub checksum: String,
    /// Who added it.
    pub created_by: Option<Uuid>,
    /// When it was added.
    pub created_at: OffsetDateTime,
    /// When a run last named it.
    pub last_used_at: Option<OffsetDateTime>,
}

impl AgentFile {
    /// The size as the cap arithmetic wants it.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.size_bytes.max(0) as u64
    }
}

/// The row about to be written.
#[derive(Debug, Clone)]
pub struct NewAgentFile {
    /// The owning agent.
    pub agent_id: Uuid,
    /// The run that produced it, if any.
    pub run_id: Option<Uuid>,
    /// The validated path.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Declared content type.
    pub content_type: String,
    /// The derived storage key.
    pub storage_key: String,
    /// Hex-encoded SHA-256.
    pub checksum: String,
    /// Who added it.
    pub created_by: Option<Uuid>,
}

/// How full a workspace is — the two numbers the usage bar needs and the two it can be checked
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Bytes currently stored across the agent's files.
    pub used_bytes: u64,
    /// The per-agent ceiling.
    pub limit_bytes: u64,
    /// How many files count against it.
    pub file_count: u64,
}

impl Usage {
    /// Bytes still writable.
    #[must_use]
    pub fn remaining_bytes(&self) -> u64 {
        self.limit_bytes.saturating_sub(self.used_bytes)
    }

    /// The bar as a whole percentage, for the panel's width.
    #[must_use]
    pub fn percent(&self) -> u8 {
        if self.limit_bytes == 0 {
            return 100;
        }
        let ratio = self.used_bytes.saturating_mul(100) / self.limit_bytes;
        ratio.min(100) as u8
    }
}

const FILE_COLUMNS: &str = "id, agent_id, run_id, path, size_bytes, content_type, storage_key, \
     checksum, created_by, created_at, last_used_at";

/// Refuse a path the workspace cannot hold.
///
/// Every refusal carries the rule it broke *and* the limit, because the caller is a form field
/// and a message that says only "invalid path" leaves the person guessing which of four rules
/// they hit. The rules, in the order they are checked:
///
/// - not empty, and at most [`MAX_PATH_CHARS`] characters;
/// - no control character anywhere (a newline in a path survives into a log line and a CSV);
/// - not absolute (no leading `/`, and no Windows-style `C:` drive prefix);
/// - no `..` segment, at the start or after a `/` — including a trailing one, which is the
///   spelling `notes/..` that a naive `contains("..")` check would catch but a segment parse
///   must not, because `..hidden.md` is a perfectly good file name.
///
/// The check is a *pure function* on purpose: the store's own `list_files` re-runs it over
/// every row, so a row written by an older build under a rule that has since tightened becomes
/// unreachable rather than reachable-by-being-older.
pub fn validate_path(path: &str) -> Result<String> {
    if path.trim().is_empty() {
        return Err(AiHubError::InvalidFile("the path is empty".to_owned()));
    }
    let chars = path.chars().count();
    if chars > MAX_PATH_CHARS {
        return Err(AiHubError::InvalidFile(format!(
            "the path is {chars} characters; the limit is {MAX_PATH_CHARS}"
        )));
    }
    if path.chars().any(|c| c.is_control()) {
        return Err(AiHubError::InvalidFile(
            "the path contains a control character".to_owned(),
        ));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(AiHubError::InvalidFile(
            "the path is absolute; a workspace path is relative to the agent".to_owned(),
        ));
    }
    // `C:\…` and `C:/…`: a drive letter is an absolute path wearing a relative costume, and it
    // is the one a `starts_with('/')` check misses on every platform that is not Windows.
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Err(AiHubError::InvalidFile(
            "the path names a drive; a workspace path is relative to the agent".to_owned(),
        ));
    }
    for segment in path.split(['/', '\\']) {
        if segment == ".." {
            return Err(AiHubError::InvalidFile(
                "the path walks out of the workspace with `..`".to_owned(),
            ));
        }
    }
    // Trailing separators make two spellings of one file (`notes.md` and `notes.md/`), and a
    // unique index on the raw string would then hold two rows for one address.
    if path.ends_with('/') || path.ends_with('\\') {
        return Err(AiHubError::InvalidFile(
            "the path ends with a separator".to_owned(),
        ));
    }
    Ok(path.to_owned())
}

/// The opaque object-storage key for one file.
///
/// Derived from the agent id, a *fixed* prefix and the bytes' own checksum, so:
///
/// - two agents writing `notes.md` with identical bytes get **different** keys (the agent id
///   is in the key), which is what keeps one agent from overwriting another's object;
/// - the key is not a function of the path, so a path can never be steered into a key that
///   addresses a different object;
/// - the checksum makes the key stable for the same content, so a re-upload of identical bytes
///   reuses the same object instead of leaving an orphan behind.
///
/// The prefix is `agents/`, not a bare hex string, because an operator listing a bucket needs
/// to recognise what a key belongs to; a workspace blob with no marker in its key is one more
/// unexplained object in an S3 console.
#[must_use]
pub fn storage_key(agent_id: Uuid, checksum: &str) -> String {
    let hex = checksum.trim();
    let hex = if hex.is_empty() { "unhashed" } else { hex };
    format!("agents/{agent_id}/{hex}")
}

/// How full one agent's workspace is.
///
/// One aggregate query rather than a per-file loop, because the Workspace tab renders this on
/// every load and a hundred rows of `sum` is a hundred round trips for one number.
pub async fn usage(pool: &PgPool, agent_id: Uuid) -> Result<Usage> {
    // `sum(bigint)` returns **numeric** in PostgreSQL, not `bigint`: the aggregate is promoted so
    // a sum that would overflow eight bytes errors instead of wrapping. So the cast has to be in
    // the SQL — `coalesce(sum(size_bytes), 0)::bigint` — and a query without it decodes as
    // `Option<i64>` and fails at run time on a perfectly good row. The `::bigint` cast of a
    // `numeric` that exceeded the range would itself error, which is the answer we want: a
    // workspace cannot hold 9.2 exabytes, and if it somehow did, the cap check must not pass.
    let row: (Option<i64>, Option<i64>) = sqlx::query_as(
        "select coalesce(sum(size_bytes), 0)::bigint, count(*) from ai_agent_files \
         where agent_id = $1",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await?;
    Ok(Usage {
        used_bytes: row.0.unwrap_or_default().max(0) as u64,
        limit_bytes: MAX_AGENT_BYTES,
        file_count: row.1.unwrap_or_default().max(0) as u64,
    })
}

/// Refuse an upload that breaks a cap.
///
/// Both caps live here rather than in the route, so the walk that proves them calls the same
/// function the API does — a rule proved on a copy is a rule about the copy.
///
/// `usage` is the agent's current usage; pass the *would-be* total so the caller does not have
/// to add the two up itself and get it subtly wrong for a replacement upload.
pub fn check_caps(usage: &Usage, new_bytes: u64, replacing_bytes: u64) -> Result<()> {
    if new_bytes > MAX_FILE_BYTES {
        return Err(AiHubError::InvalidFile(format!(
            "the file is {new_bytes} bytes; the per-file limit is {MAX_FILE_BYTES} bytes \
             (10 MB) — a larger file belongs in the media library"
        )));
    }
    if new_bytes == 0 {
        return Err(AiHubError::InvalidFile("the file is empty".to_owned()));
    }
    let total = usage
        .used_bytes
        .saturating_sub(replacing_bytes)
        .saturating_add(new_bytes);
    if total > usage.limit_bytes {
        return Err(AiHubError::InvalidFile(format!(
            "the workspace would hold {total} bytes; the per-agent limit is {} bytes (100 MB) \
             — {} bytes are already stored in {} file(s)",
            usage.limit_bytes,
            usage.used_bytes,
            usage.file_count
        )));
    }
    Ok(())
}

/// Every file in one agent's workspace, newest first.
pub async fn list_files(pool: &PgPool, agent_id: Uuid) -> Result<Vec<AgentFile>> {
    let sql = format!(
        "select {FILE_COLUMNS} from ai_agent_files where agent_id = $1 order by created_at desc"
    );
    let rows = sqlx::query_as::<_, AgentFile>(&sql)
        .bind(agent_id)
        .fetch_all(pool)
        .await?;
    // Re-validated on the way out, not on the way in. A row written under an older rule must
    // not become reachable just by being old, and the alternative — trusting the write path
    // alone — means the rule has two implementations and only one of them is tested.
    Ok(rows
        .into_iter()
        .filter(|row| validate_path(&row.path).is_ok())
        .collect())
}

/// One file, addressed by path inside one agent.
pub async fn get_file(
    pool: &PgPool,
    agent_id: Uuid,
    path: &str,
) -> Result<Option<AgentFile>> {
    let path = validate_path(path)?;
    let sql = format!(
        "select {FILE_COLUMNS} from ai_agent_files where agent_id = $1 and path = $2"
    );
    Ok(sqlx::query_as::<_, AgentFile>(&sql)
        .bind(agent_id)
        .bind(&path)
        .fetch_optional(pool)
        .await?)
}

/// One file by row id, for a caller that already has the index row.
///
/// The agent is part of the `where` clause, not a filter afterwards: a file id from another
/// organization must be indistinguishable from one that does not exist.
pub async fn get_file_by_id(
    pool: &PgPool,
    agent_id: Uuid,
    id: Uuid,
) -> Result<Option<AgentFile>> {
    let sql = format!(
        "select {FILE_COLUMNS} from ai_agent_files where id = $1 and agent_id = $2"
    );
    Ok(sqlx::query_as::<_, AgentFile>(&sql)
        .bind(id)
        .bind(agent_id)
        .fetch_optional(pool)
        .await?)
}

/// Write the index row for a stored object.
///
/// A path that already exists in this agent is **replaced**, not duplicated: the unique index
/// on `(agent_id, path)` is the real guarantee, and this statement is the one that honours it.
/// The old object is *not* deleted here — the caller owns the bytes it just wrote, and removing
/// the previous one is a storage operation that can fail independently of the row that points
/// at it. Leaving an orphan for a lifecycle sweep to reap is better than a delete that fails
/// halfway and leaves the index pointing at a key nobody holds.
pub async fn put_file(pool: &PgPool, new: &NewAgentFile) -> Result<AgentFile> {
    let sql = format!(
        "insert into ai_agent_files \
         (agent_id, run_id, path, size_bytes, content_type, storage_key, checksum, created_by) \
         values ($1,$2::uuid,$3,$4,$5,$6,$7,$8::uuid) \
         on conflict (agent_id, path) do update set \
           run_id = excluded.run_id, \
           size_bytes = excluded.size_bytes, \
           content_type = excluded.content_type, \
           storage_key = excluded.storage_key, \
           checksum = excluded.checksum, \
           created_by = excluded.created_by, \
           created_at = now() \
         returning {FILE_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, AgentFile>(&sql)
        .bind(new.agent_id)
        .bind(new.run_id)
        .bind(&new.path)
        .bind(new.size_bytes)
        .bind(&new.content_type)
        .bind(&new.storage_key)
        .bind(&new.checksum)
        .bind(new.created_by)
        .fetch_one(pool)
        .await?)
}

/// The size of the file currently at `path`, or 0 — what a replacement upload refunds from the
/// quota.
pub async fn path_size(pool: &PgPool, agent_id: Uuid, path: &str) -> Result<u64> {
    let path = validate_path(path)?;
    let row: Option<i64> =
        sqlx::query_scalar("select size_bytes from ai_agent_files where agent_id = $1 and path = $2")
            .bind(agent_id)
            .bind(&path)
            .fetch_optional(pool)
            .await?;
    Ok(row.unwrap_or_default().max(0) as u64)
}

/// Remove the index row for one file; `true` when one was there.
///
/// The bytes are *not* removed: the caller holds the storage handle and deletes the object
/// itself, in the same order media's own upload does it. A store that deleted the object and
/// then failed to delete the row would leave a row pointing at nothing, which is a worse state
/// than an orphan object nobody references.
pub async fn delete_file(pool: &PgPool, agent_id: Uuid, id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from ai_agent_files where id = $1 and agent_id = $2")
        .bind(id)
        .bind(agent_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Note that a run named this file, so the Workspace tab's "last used by run" column is real.
///
/// Best-effort by design: the file exists whether or not the note lands, and a run that
/// completed is not a run that failed because a `last_used_at` did not move.
pub async fn mark_used(pool: &PgPool, agent_id: Uuid, id: Uuid, run_id: Uuid) -> Result<()> {
    sqlx::query(
        "update ai_agent_files set last_used_at = now(), run_id = coalesce(run_id, $3) \
         where id = $1 and agent_id = $2",
    )
    .bind(id)
    .bind(agent_id)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_path_is_kept_as_written() {
        assert_eq!(validate_path("report.md").unwrap(), "report.md");
        assert_eq!(
            validate_path("data/2026/q3/sales.csv").unwrap(),
            "data/2026/q3/sales.csv"
        );
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert!(validate_path("").is_err());
        assert!(validate_path("   ").is_err());
    }

    #[test]
    fn an_absolute_path_is_refused_in_both_spellings() {
        assert!(validate_path("/etc/passwd").is_err());
        assert!(validate_path("\\windows\\system32").is_err());
        // A drive letter is an absolute path wearing a relative costume.
        assert!(validate_path("C:/notes.md").is_err());
        assert!(validate_path("c:notes.md").is_err());
    }

    #[test]
    fn a_traversal_is_refused_in_every_position() {
        for path in [
            "../secrets.txt",
            "notes/../../secrets.txt",
            "a/b/../../../etc/passwd",
            "..",
            "a/..",
        ] {
            let refused = validate_path(path).is_err();
            assert!(refused, "the traversal {path:?} must be refused");
        }
    }

    #[test]
    fn a_name_that_merely_starts_with_dots_is_a_file() {
        // `..hidden.md` is not a traversal and refusing it would refuse a real name. This is
        // the test that keeps the `..` check a *segment* comparison rather than a substring one.
        assert!(validate_path("..hidden.md").is_ok());
        assert!(validate_path("notes/..archive").is_ok());
    }

    #[test]
    fn a_control_character_is_refused() {
        assert!(validate_path("notes\n.md").is_err());
        assert!(validate_path("notes\t.md").is_err());
        assert!(validate_path("notes\u{0}.md").is_err());
    }

    #[test]
    fn a_trailing_separator_is_refused() {
        // `notes.md/` and `notes.md` are one file with two spellings, and the unique index is on
        // the raw string.
        assert!(validate_path("notes.md/").is_err());
        assert!(validate_path("data/").is_err());
    }

    #[test]
    fn the_length_limit_is_enforced_with_the_limit_in_the_message() {
        let long = "a".repeat(MAX_PATH_CHARS + 1);
        let message = validate_path(&long).unwrap_err().to_string();
        assert!(message.contains(&MAX_PATH_CHARS.to_string()), "{message}");
    }

    #[test]
    fn a_storage_key_is_opaque_and_names_its_agent() {
        let agent = Uuid::new_v4();
        let key = storage_key(agent, "abc123");
        assert!(key.starts_with("agents/"), "{key}");
        assert!(key.contains(&agent.to_string()), "{key}");
        assert!(key.ends_with("abc123"), "{key}");
        // The path is not in it — a key that encoded the path would be steerable.
        assert!(!key.contains("notes"), "{key}");
    }

    #[test]
    fn two_agents_writing_the_same_path_get_different_keys() {
        let checksum = "deadbeef";
        let one = storage_key(Uuid::new_v4(), checksum);
        let two = storage_key(Uuid::new_v4(), checksum);
        assert_ne!(one, two);
    }

    #[test]
    fn the_same_content_stored_twice_reuses_one_object() {
        let agent = Uuid::new_v4();
        assert_eq!(storage_key(agent, "cafe"), storage_key(agent, "cafe"));
        // A re-upload of a renamed file is therefore a rename, not a second copy.
        assert_eq!(storage_key(agent, "cafe"), storage_key(agent, "cafe"));
    }

    #[test]
    fn an_unhashed_file_still_gets_a_key() {
        let key = storage_key(Uuid::new_v4(), "");
        assert!(key.starts_with("agents/"), "{key}");
        assert!(key.ends_with("unhashed"), "{key}");
    }

    #[test]
    fn the_per_file_cap_names_both_the_size_and_the_limit() {
        let usage = Usage { used_bytes: 0, limit_bytes: MAX_AGENT_BYTES, file_count: 0 };
        let error = check_caps(&usage, MAX_FILE_BYTES + 1, 0).unwrap_err().to_string();
        assert!(error.contains("10 MB"), "{error}");

        // Exactly at the cap is allowed; one byte over is not. A ceiling that refuses its own
        // boundary is a ceiling off by one that nobody notices until it refuses a legal upload.
        assert!(check_caps(&usage, MAX_FILE_BYTES, 0).is_ok());
        assert!(check_caps(&usage, MAX_FILE_BYTES + 1, 0).is_err());
    }

    #[test]
    fn an_empty_file_is_refused() {
        let usage = Usage { used_bytes: 0, limit_bytes: MAX_AGENT_BYTES, file_count: 0 };
        assert!(check_caps(&usage, 0, 0).is_err());
    }

    #[test]
    fn overwriting_a_file_never_grows_the_workspace() {
        // The point of passing `replacing_bytes` is that overwriting must not be impossible
        // once the agent is close to the ceiling — which is exactly when somebody is most
        // likely to be replacing a file. A cap that only *adds* makes `notes.md`
        // un-overwritable at 95 MB, so the fix would be to delete a file first, which is a
        // worse answer than the one the screen is asking for.
        //
        // Every figure is written as an *offset from the cap* rather than as a round number in
        // decimal megabytes. The first draft of this test asserted 10 500 000 bytes "just over
        // the 10 MB per-file limit" and failed, because 10 MB here is 10 × 1024² and the
        // round number is well under it. A test that has to know which MB the code means is a
        // test that breaks when someone switches to decimal.
        let full = MAX_FILE_BYTES as u64;
        let near = Usage {
            used_bytes: MAX_AGENT_BYTES - 800_000,
            limit_bytes: MAX_AGENT_BYTES,
            file_count: 10,
        };
        // Same size: the total is unchanged and the upload goes through.
        assert!(check_caps(&near, full, full).is_ok());
        // The largest legal file is still legal when it replaces a file of that size: 99.2 MB
        // - 10 MB + 10 MB = 99.2 MB, and the ceiling is 100 MB.
        assert!(check_caps(&near, full, full).is_ok());
        // A *new* file at the same size has nothing to subtract, so it is the one refused —
        // which is the correct asymmetry: replacing is always possible, growing is not.
        assert!(check_caps(&near, full, 0).is_err());
    }

    #[test]
    fn a_replacement_that_shrinks_is_always_allowed() {
        // Even at exactly the ceiling, replacing a 10 MB file with a 1 KB one goes through.
        let full = Usage {
            used_bytes: MAX_AGENT_BYTES,
            limit_bytes: MAX_AGENT_BYTES,
            file_count: 10,
        };
        assert!(check_caps(&full, 1_000, MAX_FILE_BYTES as u64).is_ok());
    }

    #[test]
    fn a_replacement_that_grows_past_the_ceiling_is_refused() {
        // 99.2 MB used, replacing a 1 MB file with the 10 MB maximum: 108 MB, over the line.
        let near = Usage {
            used_bytes: MAX_AGENT_BYTES - 800_000,
            limit_bytes: MAX_AGENT_BYTES,
            file_count: 10,
        };
        let one_mb: u64 = 1024 * 1024;
        assert!(check_caps(&near, MAX_FILE_BYTES as u64, one_mb).is_err());
    }

    #[test]
    fn the_cap_message_names_what_is_already_stored() {
        let usage = Usage { used_bytes: 99_000_000, limit_bytes: MAX_AGENT_BYTES, file_count: 7 };
        let message = check_caps(&usage, 10_000_000, 0).unwrap_err().to_string();
        assert!(message.contains("100 MB"), "{message}");
        assert!(message.contains('7'), "{message}");
    }

    #[test]
    fn usage_percent_is_bounded() {
        let empty = Usage { used_bytes: 0, limit_bytes: 100, file_count: 0 };
        assert_eq!(empty.percent(), 0);
        let half = Usage { used_bytes: 50, limit_bytes: 100, file_count: 1 };
        assert_eq!(half.percent(), 50);
        let full = Usage { used_bytes: 100, limit_bytes: 100, file_count: 1 };
        assert_eq!(full.percent(), 100);
        // Over the line (a row written before a cap was lowered) must still render a bar.
        let over = Usage { used_bytes: 200, limit_bytes: 100, file_count: 1 };
        assert_eq!(over.percent(), 100);
        assert_eq!(over.remaining_bytes(), 0);
        // A zero limit is a misconfiguration, and must not divide by zero into a panic in the
        // panel's inline style.
        let zero = Usage { used_bytes: 5, limit_bytes: 0, file_count: 1 };
        assert_eq!(zero.percent(), 100);
    }
}
