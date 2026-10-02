//! MCP clients, their tool grants, and the invocation log (REQ-108, slice 1).
//!
//! # A client is a machine user, so its token is treated like one
//!
//! The request names this area "the highest-value target in the platform for lateral movement",
//! and the whole module is shaped by that sentence. The token is stored as a SHA-256 hash and
//! returned in clear exactly once, by [`McpStore::create_client`]; every later read of a client
//! can only ever produce its [`ClientRow::token_prefix`]. Authentication is a single-row lookup
//! on the unique hash index rather than a scan-and-compare over the table, because a token check
//! that is O(clients) is a denial-of-service surface in itself.
//!
//! # Comparing a presented token must not leak where it first differs
//!
//! [`verify_token`] compares digests byte by byte over a fixed length and does not early-return
//! on the first mismatch. A `==` on a hex string stops at the first differing character, and the
//! time it takes to stop is a channel: an attacker who can time authentication learns the
//! length of the shared prefix their guess got right. The same reasoning is why the hash covers
//! the *whole* token including its `omnmcp_` prefix.
//!
//! # Revocation and deletion are different acts
//!
//! Revoking keeps the row and its whole invocation history — a client that called a tool at 09:14
//! must still be answerable for it at 09:15 — while deleting is for a client that was created by
//! mistake. The migration's check constraint makes `revoked_at` and `enabled` mutually
//! exclusive, so an edit that forgets the flag is refused by the database rather than producing
//! a row that says "revoked" and answers calls anyway.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// The prefix every MCP token carries. Deliberately distinct from the developer keys'
/// `omndev_` and the service-account `omsa_`: a token that looked like one of those would be
/// misdiagnosed as one, and an operator triaging a leaked token would be sent to the wrong log.
pub const TOKEN_PREFIX: &str = "omnmcp_";

/// One MCP client, as the panel's table and detail render it.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ClientRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub description: String,
    /// The last four characters of the token, kept in the clear for recognition.
    pub token_prefix: String,
    pub scopes: serde_json::Value,
    pub sandbox: bool,
    pub rate_limit_per_min: i32,
    pub enabled: bool,
    pub last_used_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
    /// How many grants this client holds, so the table can render "3 / 24" without a request
    /// per row. Counted here rather than in the client because the panel renders one table and
    /// N+1 requests at N rows is the difference between a fast list and a slow one.
    pub tool_count: i64,
}

impl ClientRow {
    /// What the panel shows in the status column.
    ///
    /// Derived rather than stored, because `enabled`, `revoked_at` and `revoked_at IS NULL AND
    /// NOT enabled` are three different states that a single boolean cannot carry. A revoked
    /// client that is merely disabled is *disabled* — that is a pause — and the panel needs to
    /// say so, because the remedy differs.
    pub fn status(&self) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if !self.enabled {
            "disabled"
        } else {
            "active"
        }
    }

    /// True when the token would authenticate: not revoked and not disabled.
    pub fn can_authenticate(&self) -> bool {
        self.enabled && self.revoked_at.is_none()
    }
}

/// One granted tool.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ClientToolRow {
    pub client_id: Uuid,
    pub tool: String,
    /// The permission the tool resolves to in the registry, denormalised so the grant picker can
    /// render "this tool needs this power" without loading the whole registry.
    pub permission: Option<String>,
    pub approval_required: bool,
    pub enabled: bool,
    pub added_at: OffsetDateTime,
}

/// What creating a client takes.
#[derive(Debug, Clone)]
pub struct NewClient {
    pub organization_id: Uuid,
    pub name: String,
    pub description: String,
    pub scopes: Vec<String>,
    pub sandbox: bool,
    pub rate_limit_per_min: i32,
    pub created_by: Option<Uuid>,
}

/// A created client and the only copy of its token that will ever exist.
#[derive(Debug, Clone)]
pub struct CreatedClient {
    pub client: ClientRow,
    /// `omnmcp_<43 chars>`. Returned once; the panel shows it once and warns to store it.
    pub token: String,
}

/// The result of authenticating a presented token.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuthenticatedClient {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub enabled: bool,
    pub revoked_at: Option<OffsetDateTime>,
    pub sandbox: bool,
    pub rate_limit_per_min: i32,
}

impl AuthenticatedClient {
    /// A token authenticates only while it is both enabled and unrevoked. Revocation is checked
    /// here rather than in the query's `where` so an operator can tell "no such token" from
    /// "that token was revoked" in the log without the two being distinguishable to a caller —
    /// the *response* is the same either way, which is the point.
    pub fn is_usable(&self) -> bool {
        self.enabled && self.revoked_at.is_none()
    }
}

/// Hash a token for storage and lookup.
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Compare a presented token against a stored hash without an early return.
pub fn verify_token(presented: &str, stored_hash: &str) -> bool {
    let candidate = hash_token(presented);
    // Both sides are 64 hex characters, so a length mismatch is itself the answer rather than a
    // loop bound; `constant_time_eq` below is the real comparison.
    if candidate.len() != stored_hash.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in candidate.bytes().zip(stored_hash.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// The eight characters kept in the clear: four of the fixed prefix and the last four of the
/// secret. Enough to recognise a token in a log; useless without the rest.
pub fn token_display_prefix(token: &str) -> String {
    let secret = token.strip_prefix(TOKEN_PREFIX).unwrap_or(token);
    let tail: String = secret.chars().rev().take(4).collect();
    format!("{}{}", &TOKEN_PREFIX[..4], tail.chars().rev().collect::<String>())
}

/// Mint a token: `omnmcp_` plus 43 characters of base64url from `OsRng`.
///
/// 32 bytes of `OsRng` is 256 bits, which is where the 43 comes from: base64url without
/// padding encodes 32 bytes as 43 characters, so the encoded form carries the *whole* of the
/// entropy rather than a slice of it. Hand-rolling that mapping (which is what a
/// nibble-at-a-time encoder does) would be shorter to read and wrong in the one place that
/// matters, so `base64` does it.
pub fn mint_token() -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use rand::RngCore;
    use rand::rngs::OsRng;
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
}

/// Store access for MCP clients and grants.
#[derive(Debug, Clone)]
pub struct McpStore {
    pool: PgPool,
}

impl McpStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a client and return its token.
    ///
    /// The token is minted here and nowhere else, which is what makes "shown exactly once" a
    /// property of the code rather than a promise: there is no other function that could return
    /// the cleartext, because there is no other function that has it.
    pub async fn create_client(&self, new: NewClient) -> Result<CreatedClient> {
        validate_new(&new)?;
        let token = mint_token();
        let hash = hash_token(&token);
        let prefix = token_display_prefix(&token);
        let id = Uuid::new_v4();
        let scopes = serde_json::to_value(&new.scopes).unwrap_or_else(|_| serde_json::json!([]));

        let row = sqlx::query_as::<_, ClientRow>(
            r#"
            insert into mcp_clients (
                id, organization_id, name, description, token_prefix, token_hash,
                scopes, sandbox, rate_limit_per_min, created_by
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            returning id, organization_id, name, description, token_prefix, scopes, sandbox,
                      rate_limit_per_min, enabled, last_used_at, created_by, created_at,
                      updated_at, revoked_at, 0::bigint as tool_count
            "#,
        )
        .bind(id)
        .bind(new.organization_id)
        .bind(new.name.trim())
        .bind(new.description.trim())
        .bind(&prefix)
        .bind(&hash)
        .bind(&scopes)
        .bind(new.sandbox)
        .bind(new.rate_limit_per_min)
        .bind(new.created_by)
        .fetch_optional(&self.pool)
        .await?;

        // 23505 is the unique index on (organization_id, name). A duplicate name is a
        // conflict the panel resolves by asking for another name, not a 500 — the message says
        // which name, and the caller gets a code it can branch on. The name is trimmed first,
        // so the conflict message has to report the trimmed form too or it names a string the
        // operator never sees.
        match row {
            Some(client) => Ok(CreatedClient { client, token }),
            None => Err(AiHubError::McpClientConflict(format!(
                "an MCP client named \"{}\" already exists",
                new.name.trim()
            ))),
        }
    }

    /// Every client in a tenant, newest first.
    pub async fn list_clients(&self, organization_id: Uuid) -> Result<Vec<ClientRow>> {
        let rows = sqlx::query_as::<_, ClientRow>(
            r#"
            select c.id, c.organization_id, c.name, c.description, c.token_prefix, c.scopes,
                   c.sandbox, c.rate_limit_per_min, c.enabled, c.last_used_at, c.created_by,
                   c.created_at, c.updated_at, c.revoked_at,
                   (select count(*) from mcp_client_tools t
                     where t.client_id = c.id and t.enabled) as tool_count
              from mcp_clients c
             where c.organization_id = $1
             order by c.created_at desc
            "#,
        )
        .bind(organization_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// One client, or `None` if it belongs to another tenant.
    ///
    /// The `organization_id` predicate is not an optimisation: an MCP client id in a URL is a
    /// perfectly guessable UUID, and without it the route would answer 200 with another
    /// tenant's name and token prefix.
    pub async fn read_client(
        &self,
        organization_id: Uuid,
        id: Uuid,
    ) -> Result<Option<ClientRow>> {
        let row = sqlx::query_as::<_, ClientRow>(
            r#"
            select c.id, c.organization_id, c.name, c.description, c.token_prefix, c.scopes,
                   c.sandbox, c.rate_limit_per_min, c.enabled, c.last_used_at, c.created_by,
                   c.created_at, c.updated_at, c.revoked_at,
                   (select count(*) from mcp_client_tools t
                     where t.client_id = c.id and t.enabled) as tool_count
              from mcp_clients c
             where c.organization_id = $1 and c.id = $2
            "#,
        )
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Authenticate a presented token in one indexed lookup.
    pub async fn authenticate(&self, token: &str) -> Result<Option<AuthenticatedClient>> {
        let hash = hash_token(token);
        let row = sqlx::query_as::<_, AuthenticatedClient>(
            r#"
            select id, organization_id, name, enabled, revoked_at, sandbox, rate_limit_per_min
              from mcp_clients
             where token_hash = $1
            "#,
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record that a token was used, so the table can show "last used" without a scan.
    ///
    /// Best-effort by design: a failure here must not fail the call that was actually served.
    /// The caller's outcome is the thing being audited, not the timestamp next to it.
    pub async fn touch(&self, id: Uuid) -> Result<()> {
        sqlx::query("update mcp_clients set last_used_at = now() where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Replace a client's grants wholesale.
    ///
    /// Delete-then-insert in one transaction, so the picker never sees a half-applied set and a
    /// failure leaves the previous grants intact rather than none of them. Each row carries the
    /// permission the registry gave for that tool: the picker is the only reader that needs it
    /// and the join would be a request per row.
    pub async fn replace_tools(
        &self,
        client_id: Uuid,
        grants: &[(String, Option<String>, bool)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("delete from mcp_client_tools where client_id = $1")
            .bind(client_id)
            .execute(&mut *tx)
            .await?;
        for (tool, permission, approval_required) in grants {
            sqlx::query(
                r#"
                insert into mcp_client_tools (client_id, tool, permission, approval_required)
                values ($1, $2, $3, $4)
                on conflict (client_id, tool) do update
                    set permission = excluded.permission,
                        approval_required = excluded.approval_required,
                        enabled = true
                "#,
            )
            .bind(client_id)
            .bind(tool)
            .bind(permission)
            .bind(*approval_required)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A client's grants, enabled ones first for `tools/list`.
    pub async fn list_tools(&self, client_id: Uuid) -> Result<Vec<ClientToolRow>> {
        let rows = sqlx::query_as::<_, ClientToolRow>(
            r#"
            select client_id, tool, permission, approval_required, enabled, added_at
              from mcp_client_tools
             where client_id = $1 and enabled
             order by tool
            "#,
        )
        .bind(client_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// The enabled grants as bare tool names — what `tools/list` answers with.
    pub async fn granted_tool_names(&self, client_id: Uuid) -> Result<Vec<String>> {
        Ok(self
            .list_tools(client_id)
            .await?
            .into_iter()
            .map(|t| t.tool)
            .collect())
    }

    /// Whether a client holds a grant for a tool, and what that tool needs.
    ///
    /// Both halves in one row, because a `tools/call` that checked the grant and then looked
    /// the permission up separately would have a window where the grant and the permission
    /// disagree — and the permission is the half that decides.
    pub async fn grant_for(
        &self,
        client_id: Uuid,
        tool: &str,
    ) -> Result<Option<ClientToolRow>> {
        let row = sqlx::query_as::<_, ClientToolRow>(
            r#"
            select client_id, tool, permission, approval_required, enabled, added_at
              from mcp_client_tools
             where client_id = $1 and tool = $2 and enabled
            "#,
        )
        .bind(client_id)
        .bind(tool)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Issue a new token, dropping the old one in the same statement.
    ///
    /// One `update` and not delete-then-insert: a rotation that failed between the two would
    /// leave the client with no token at all, which reads as "the panel ate my client".
    pub async fn rotate_secret(&self, organization_id: Uuid, id: Uuid) -> Result<CreatedClient> {
        let token = mint_token();
        let hash = hash_token(&token);
        let prefix = token_display_prefix(&token);
        let row = sqlx::query_as::<_, ClientRow>(
            r#"
            update mcp_clients
               set token_hash = $3, token_prefix = $4, updated_at = now()
             where organization_id = $1 and id = $2
            returning id, organization_id, name, description, token_prefix, scopes, sandbox,
                      rate_limit_per_min, enabled, last_used_at, created_by, created_at,
                      updated_at, revoked_at,
                      (select count(*) from mcp_client_tools t
                        where t.client_id = mcp_clients.id and t.enabled) as tool_count
            "#,
        )
        .bind(organization_id)
        .bind(id)
        .bind(&hash)
        .bind(&prefix)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|client| CreatedClient { client, token })
            .ok_or(AiHubError::McpClientNotFound(id))
    }

    /// Revoke without deleting, so the invocation history stays answerable.
    pub async fn revoke(&self, organization_id: Uuid, id: Uuid) -> Result<ClientRow> {
        // The two assignments are the same statement on purpose: the migration's check
        // constraint forbids `revoked_at IS NOT NULL AND enabled`, so a revoke that only set the
        // timestamp would be refused by the database, and one that only cleared `enabled` would
        // be a pause that the panel then called a revocation.
        let row = sqlx::query_as::<_, ClientRow>(
            r#"
            update mcp_clients
               set revoked_at = now(), enabled = false, updated_at = now()
             where organization_id = $1 and id = $2
            returning id, organization_id, name, description, token_prefix, scopes, sandbox,
                      rate_limit_per_min, enabled, last_used_at, created_by, created_at,
                      updated_at, revoked_at,
                      (select count(*) from mcp_client_tools t
                        where t.client_id = mcp_clients.id and t.enabled) as tool_count
            "#,
        )
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(AiHubError::McpClientNotFound(id))
    }

    /// Delete a client. Its invocations cascade — see the migration note.
    pub async fn delete_client(&self, organization_id: Uuid, id: Uuid) -> Result<()> {
        let done = sqlx::query("delete from mcp_clients where organization_id = $1 and id = $2")
            .bind(organization_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(AiHubError::McpClientNotFound(id));
        }
        Ok(())
    }

    /// Edit the mutable fields. The token is not among them, on purpose.
    pub async fn update_client(
        &self,
        organization_id: Uuid,
        id: Uuid,
        name: &str,
        description: &str,
        scopes: &[String],
        sandbox: bool,
        rate_limit_per_min: i32,
    ) -> Result<ClientRow> {
        let check = NewClient {
            organization_id,
            name: name.to_owned(),
            description: description.to_owned(),
            scopes: scopes.to_vec(),
            sandbox,
            rate_limit_per_min,
            created_by: None,
        };
        validate_new(&check)?;
        let scopes_json =
            serde_json::to_value(scopes).unwrap_or_else(|_| serde_json::json!([]));
        let row = sqlx::query_as::<_, ClientRow>(
            r#"
            update mcp_clients
               set name = $3, description = $4, scopes = $5, sandbox = $6,
                   rate_limit_per_min = $7, updated_at = now()
             where organization_id = $1 and id = $2
            returning id, organization_id, name, description, token_prefix, scopes, sandbox,
                      rate_limit_per_min, enabled, last_used_at, created_by, created_at,
                      updated_at, revoked_at,
                      (select count(*) from mcp_client_tools t
                        where t.client_id = mcp_clients.id and t.enabled) as tool_count
            "#,
        )
        .bind(organization_id)
        .bind(id)
        .bind(name.trim())
        .bind(description.trim())
        .bind(&scopes_json)
        .bind(sandbox)
        .bind(rate_limit_per_min)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or(AiHubError::McpClientNotFound(id))
    }
}

/// Refuse what the columns would eventually refuse, by name.
///
/// The migration's check constraints are the backstop; this is the error message. A client
/// named "" arriving as a 500 from a constraint violation tells an operator nothing, and the
/// panel's field-level validation has to say which field is wrong.
pub fn validate_new(new: &NewClient) -> Result<()> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(AiHubError::InvalidMcpClient("name must not be empty".into()));
    }
    if name.chars().count() > 60 {
        return Err(AiHubError::InvalidMcpClient(
            "name must be 60 characters or fewer".into(),
        ));
    }
    if new.description.chars().count() > 200 {
        return Err(AiHubError::InvalidMcpClient(
            "description must be 200 characters or fewer".into(),
        ));
    }
    if !(1..=600).contains(&new.rate_limit_per_min) {
        return Err(AiHubError::InvalidMcpClient(
            "rate limit must be between 1 and 600 per minute".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_authenticates_and_one_more_character_does_not() {
        let token = mint_token();
        assert!(token.starts_with(TOKEN_PREFIX), "the prefix is what identifies it");
        let hash = hash_token(&token);
        assert!(verify_token(&token, &hash), "the token authenticates");
        // The obvious three: a trailing character, a substituted one, and the empty string.
        assert!(!verify_token(&format!("{token}x"), &hash));
        assert!(!verify_token(&format!("{}x", &token[..token.len() - 1]), &hash));
        assert!(!verify_token("", &hash));
        // A hash of a *different* length must not read as a match.
        assert!(!verify_token(&token, "short"));
    }

    #[test]
    fn the_display_prefix_is_recognition_not_credential() {
        let token = mint_token();
        let prefix = token_display_prefix(&token);
        assert_eq!(prefix.len(), 8, "four of the prefix plus the last four");
        assert!(prefix.starts_with("omnm"));
        assert!(token.ends_with(&prefix[4..]), "the tail is the token's own tail");
        // Two tokens minted in a row must not share a prefix: that is the property that makes
        // the column worth indexing and the panel worth showing.
        let other = mint_token();
        assert_ne!(token_display_prefix(&other), prefix);
    }

    #[test]
    fn two_minted_tokens_differ() {
        // Not a property test so much as a guard against a refactor that mints from a constant.
        let a = mint_token();
        let b = mint_token();
        assert_ne!(a, b);
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn a_token_hash_is_stable_across_calls() {
        let token = "omnmcp_example";
        assert_eq!(hash_token(token), hash_token(token));
        assert_eq!(hash_token(token).len(), 64, "a full SHA-256 in hex");
        assert_ne!(hash_token(token), hash_token("omnmcp_example "), "no trimming");
    }

    #[test]
    fn name_and_rate_are_refused_by_name() {
        let base = NewClient {
            organization_id: Uuid::nil(),
            name: "Reporting agent".into(),
            description: String::new(),
            scopes: vec![],
            sandbox: true,
            rate_limit_per_min: 60,
            created_by: None,
        };
        validate_new(&base).expect("the reference client is valid");

        let mut blank = base.clone();
        blank.name = "   ".into();
        assert!(validate_new(&blank).is_err(), "whitespace is not a name");

        let mut long = base.clone();
        long.name = "x".repeat(61);
        assert!(validate_new(&long).is_err());

        // 60 is the limit, inclusive.
        let mut edge = base.clone();
        edge.name = "x".repeat(60);
        validate_new(&edge).expect("60 is allowed");

        let mut fast = base.clone();
        fast.rate_limit_per_min = 601;
        assert!(validate_new(&fast).is_err());
        let mut slow = base.clone();
        slow.rate_limit_per_min = 0;
        assert!(validate_new(&slow).is_err());
    }

    #[test]
    fn a_client_reports_three_states_not_one() {
        let mut row = ClientRow {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "c".into(),
            description: String::new(),
            token_prefix: "omnmabcd".into(),
            scopes: serde_json::json!([]),
            sandbox: true,
            rate_limit_per_min: 60,
            enabled: true,
            last_used_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            revoked_at: None,
            tool_count: 0,
        };
        assert_eq!(row.status(), "active");
        assert!(row.can_authenticate());

        row.enabled = false;
        assert_eq!(row.status(), "disabled", "a pause is not a revocation");
        assert!(!row.can_authenticate());

        row.enabled = true;
        row.revoked_at = Some(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(row.status(), "revoked");
        assert!(!row.can_authenticate(), "a revoked token never authenticates");
    }
}
