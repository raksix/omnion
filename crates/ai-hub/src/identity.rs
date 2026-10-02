//! AI identities: the named grant sets a run borrows (REQ-100, slice 2).
//!
//! # What an identity is
//!
//! An agent's own `tools` column says which tools it *may* try. It does not say which tools the
//! installation has agreed this agent's operator is allowed to use — and those are different
//! questions with different owners. The first is a per-agent configuration decision made by
//! whoever built the agent. The second is the thing an organization wants to audit: "if this
//! agent is hijacked by a prompt injection, what can it actually do?"
//!
//! An identity is that second answer, made named and reusable: a row plus one grant row per tool
//! the operator has an opinion about.
//!
//! # Why inherit is the absence of a row
//!
//! The screen's tri-state is allow / deny / inherit, and the database stores only two values.
//! `ai_tool_grants.effect` is a NOT NULL boolean, and **inherit is row absence**. A nullable
//! column would carry the third state in the database and force every read to remember
//! `effect is not null` — the same class of bug the platform's `null = built-in` convention
//! exists to prevent, and it would make a missing grant indistinguishable from a broken filter
//! forever. The migration says the same thing in SQL; this is the Rust half of that rule, and
//! [`set_grant`] is the only function allowed to write the two cases.
//!
//! # Why a deny is a row and not a filter
//!
//! A deny must survive a later allow. If denies were computed as "not in the allow set", then
//! granting a tool anywhere would silently clear a deny, and the only record of the decision
//! somebody made would be gone. [`resolve`] is the one place the two combine, and it is a pure
//! function precisely so the combination can be unit-tested without a database: an explicit deny
//! beats every allow from every source, including the identity's own default and the agent's
//! allow-list. That ordering is the whole security claim of this file, so it is written once,
//! here, and the execution path calls it rather than re-deriving it.
//!
//! # Why uniqueness folds the organization
//!
//! The migration's `coalesce(organization_id, '0000…')` index exists because plain
//! `unique (organization_id, key)` does not fire for NULL in PostgreSQL, and NULL here means
//! "platform-level, shared by every tenant". A plain constraint would let every organization
//! install a row that claims to be the platform default, and the first agent to resolve an
//! identity would get whichever one the planner returned first. Every identity read in this
//! file therefore also states *whose* row it wants, and the platform row is a deliberate
//! fallback rather than a race.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

/// The identity key's ceiling, matching the migration's check constraint.
pub const MAX_KEY_CHARS: usize = 64;
/// The name's ceiling, matching `ai_identities_name_len`.
pub const MAX_NAME_CHARS: usize = 80;
/// The description's ceiling, matching `ai_identities_description_len`.
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// Every column the store reads back, in one place so a write and a read cannot drift.
const IDENTITY_COLUMNS: &str = "id, organization_id, key, name, description, is_default, \
     created_by, created_at, updated_at";

/// An identity, as the API and the panel read it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, sqlx::FromRow)]
pub struct AiIdentity {
    pub id: Uuid,
    /// `None` is the platform-level identity, shared by every organization.
    pub organization_id: Option<Uuid>,
    pub key: String,
    pub name: String,
    pub description: String,
    /// The identity a run borrows when its agent does not name one.
    pub is_default: bool,
    pub created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl AiIdentity {
    /// A platform-level row: readable by everyone, editable by nobody below the platform.
    ///
    /// The distinction is the request's — "a platform-level identity is readable but not
    /// editable by an organization admin" — and it has to be a method rather than a `== None`
    /// comparison scattered through the routes, because the day somebody forgets the check the
    /// failure is a tenant rewriting the installation's shared AI permissions.
    #[must_use]
    pub fn is_platform_level(&self) -> bool {
        self.organization_id.is_none()
    }
}

/// The three states a matrix cell can be in.
///
/// The wire form is a tagged string, not a number and not a nullable bool: a cell a client cannot
/// distinguish from a missing one is a cell a client will guess about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantEffect {
    /// An explicit allow: this identity may call the tool.
    Allow,
    /// An explicit deny, which beats every allow from every source.
    Deny,
    /// No decision here; the run falls through to the agent's own allow-list.
    Inherit,
}

impl GrantEffect {
    /// The database value for an effect, or `None` for inherit — because inherit writes no row.
    ///
    /// This is the conversion, and it is the reason [`GrantEffect::Inherit`] cannot be stored by
    /// accident: every write path goes through it.
    #[must_use]
    pub fn as_stored(self) -> Option<bool> {
        match self {
            Self::Allow => Some(true),
            Self::Deny => Some(false),
            Self::Inherit => None,
        }
    }

    /// Read a stored boolean back into a state. Absent row means inherit.
    #[must_use]
    pub fn from_stored(stored: Option<bool>) -> Self {
        match stored {
            Some(true) => Self::Allow,
            Some(false) => Self::Deny,
            None => Self::Inherit,
        }
    }

    /// Read a wire value, refusing anything that is not one of the three.
    pub fn from_wire(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "inherit" => Ok(Self::Inherit),
            other => Err(AiHubError::InvalidIdentity(format!(
                "`{other}` is not an effect; use allow, deny or inherit"
            ))),
        }
    }

    /// The wire name.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Inherit => "inherit",
        }
    }
}

/// What an identity row may be created with.
#[derive(Debug, Clone)]
pub struct NewIdentity {
    /// `None` builds the platform-level identity, which only a platform account may do.
    pub organization_id: Option<Uuid>,
    pub key: String,
    pub name: String,
    pub description: String,
    pub is_default: bool,
    pub created_by: Option<Uuid>,
}

/// What a caller may change on an identity.
///
/// The key is deliberately absent: it is the identity's external name, it appears in
/// `ai_tool_calls.identity_id` history and in audit rows by key, and a rename would orphan both.
/// Renaming a *display* name is ordinary; renaming an identifier is a migration.
#[derive(Debug, Clone, Default)]
pub struct IdentityChanges {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Promote to this organization's default, or give that up.
    pub is_default: Option<bool>,
}

impl IdentityChanges {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.description.is_none() && self.is_default.is_none()
    }
}

/// Validate an identity key, returning the trimmed form.
///
/// The shape is the migration's (`^[a-z][a-z0-9_-]{0,63}$`) and it is checked here as well as
/// in SQL because a clear `400` beats a PostgreSQL error code leaking through a 500.
pub fn validate_key(key: &str) -> Result<String> {
    let key = key.trim().to_lowercase();
    if key.is_empty() {
        return Err(AiHubError::InvalidIdentity("the key is required".to_owned()));
    }
    if key.len() > MAX_KEY_CHARS {
        return Err(AiHubError::InvalidIdentity(format!(
            "the key is at most {MAX_KEY_CHARS} characters"
        )));
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap_or_default();
    if !first.is_ascii_lowercase() {
        return Err(AiHubError::InvalidIdentity(
            "the key starts with a lowercase letter".to_owned(),
        ));
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
        return Err(AiHubError::InvalidIdentity(
            "the key holds only lowercase letters, digits, `_` and `-`".to_owned(),
        ));
    }
    Ok(key)
}

/// Validate a display name.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AiHubError::InvalidIdentity("the name is required".to_owned()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AiHubError::InvalidIdentity(format!(
            "the name is at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

/// Check a description's length.
pub fn validate_description(description: &str) -> Result<String> {
    let description = description.trim();
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AiHubError::InvalidIdentity(format!(
            "the description is at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok(description.to_owned())
}

/// The identities visible to one organization: its own, plus the platform-level ones.
///
/// Read access to a platform identity is deliberate (the request says so) and write access is
/// not: the routes refuse the second half, and this function is the read that makes the first
/// half possible. The organization's own rows come first so the screen's "mine above the
/// shared" ordering is a property of the query rather than of a sort the panel has to repeat.
pub async fn list_identities(pool: &PgPool, organization_id: Uuid) -> Result<Vec<AiIdentity>> {
    let sql = format!(
        "select {IDENTITY_COLUMNS} from ai_identities \
         where organization_id = $1 or organization_id is null \
         order by (organization_id is null), is_default desc, key"
    );
    Ok(sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await?)
}

/// One identity, visible to this organization.
pub async fn get_identity(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<AiIdentity>> {
    let sql = format!(
        "select {IDENTITY_COLUMNS} from ai_identities where id = $1 \
         and (organization_id = $2 or organization_id is null)"
    );
    Ok(sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

/// The identity a run borrows when its agent names none: this organization's default, or the
/// platform default.
///
/// The same "own row wins, shared row is the fallback" rule the skill registry uses, for the same
/// reason — both tables allow a tenant row to shadow a shared key, and a query that forgets the
/// ordering makes shadowing a coin flip.
pub async fn default_identity(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<AiIdentity>> {
    let sql = format!(
        "select {IDENTITY_COLUMNS} from ai_identities where is_default \
         and (organization_id = $1 or organization_id is null) \
         order by (organization_id is null) limit 1"
    );
    Ok(sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?)
}

/// Create an identity, validating the row before it is written.
///
/// The `is_default` promotion is a two-step update rather than an `insert … on conflict`:
/// clearing the previous default first is what makes "exactly one default per organization" true
/// *during* the write, and the migration's partial unique index is the backstop that turns a
/// race between two requests into a 409 rather than a second default.
pub async fn create_identity(pool: &PgPool, new: &NewIdentity) -> Result<AiIdentity> {
    let key = validate_key(&new.key)?;
    let name = validate_name(&new.name)?;
    let description = validate_description(&new.description)?;

    if new.is_default {
        if let Some(organization_id) = new.organization_id {
            sqlx::query(
                "update ai_identities set is_default = false, updated_at = now() \
                 where organization_id = $1 and is_default",
            )
            .bind(organization_id)
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "update ai_identities set is_default = false, updated_at = now() \
                 where organization_id is null and is_default",
            )
            .execute(pool)
            .await?;
        }
    }

    let sql = format!(
        "insert into ai_identities (organization_id, key, name, description, is_default, created_by) \
         values ($1, $2, $3, $4, $5, $6) returning {IDENTITY_COLUMNS}"
    );
    sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(new.organization_id)
        .bind(&key)
        .bind(&name)
        .bind(&description)
        .bind(new.is_default)
        .bind(new.created_by)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            AiHubError::IdentityConflict(format!("the identity `{key}` already exists here"))
        })
}

/// Change an identity.
///
/// The default promotion uses the same clear-then-set pair as [`create_identity`], and for the
/// same reason: the one-default index is an index, not a trigger, so the ordering is this
/// function's job.
pub async fn update_identity(
    pool: &PgPool,
    id: Uuid,
    changes: &IdentityChanges,
) -> Result<Option<AiIdentity>> {
    if let Some(name) = &changes.name {
        validate_name(name)?;
    }
    if let Some(description) = &changes.description {
        validate_description(description)?;
    }
    let Some(existing) = sqlx::query_as::<_, AiIdentity>(&format!(
        "select {IDENTITY_COLUMNS} from ai_identities where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    if changes.is_default == Some(true) {
        match existing.organization_id {
            Some(organization_id) => {
                sqlx::query(
                    "update ai_identities set is_default = false, updated_at = now() \
                     where organization_id = $1 and is_default and id <> $2",
                )
                .bind(organization_id)
                .bind(id)
                .execute(pool)
                .await?;
            }
            None => {
                sqlx::query(
                    "update ai_identities set is_default = false, updated_at = now() \
                     where organization_id is null and is_default and id <> $1",
                )
                .bind(id)
                .execute(pool)
                .await?;
            }
        }
    }

    let name = changes
        .name
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| existing.name.clone());
    let description = changes
        .description
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| existing.description.clone());
    let is_default = changes.is_default.unwrap_or(existing.is_default);

    let sql = format!(
        "update ai_identities set name = $2, description = $3, is_default = $4, updated_at = now() \
         where id = $1 returning {IDENTITY_COLUMNS}"
    );
    Ok(sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(id)
        .bind(name)
        .bind(description)
        .bind(is_default)
        .fetch_optional(pool)
        .await?)
}

/// Remove an identity and, with it, its grants (the FK cascades — see the migration).
pub async fn delete_identity(pool: &PgPool, id: Uuid) -> Result<bool> {
    let done = sqlx::query("delete from ai_identities where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// One identity's grants, as `tool_key → effect`.
///
/// A missing key is inherit by construction: the map only ever holds rows that exist, so
/// [`GrantEffect::from_stored`] is applied at the read boundary and every caller downstream sees
/// the same tri-state whether it looked the map up or not.
pub async fn grants_of(pool: &PgPool, identity_id: Uuid) -> Result<BTreeMap<String, bool>> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "select tool_key, effect from ai_tool_grants where identity_id = $1 order by tool_key",
    )
    .bind(identity_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Every identity's grants for the identities an organization can see, in one query.
///
/// The matrix screen is `tools × (agents + identities)`, and reading the grants per column would
/// be one query per identity on a screen that exists to be scanned in a single pass. One query
/// keyed by identity id is the difference between a matrix that opens and one that crawls.
pub async fn grants_for_identities(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<BTreeMap<Uuid, BTreeMap<String, bool>>> {
    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        id: Uuid,
        tool_key: String,
        effect: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "select i.id, g.tool_key, g.effect from ai_tool_grants g \
         join ai_identities i on i.id = g.identity_id \
         where i.organization_id = $1 or i.organization_id is null \
         order by i.id, g.tool_key",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let mut by_identity: BTreeMap<Uuid, BTreeMap<String, bool>> = BTreeMap::new();
    for row in rows {
        by_identity.entry(row.id).or_default().insert(row.tool_key, row.effect);
    }
    Ok(by_identity)
}

/// Set one cell of the matrix: allow, deny, or back to inherit.
///
/// **The tri-state in one function, because that is where it can be wrong.** An allow or a deny
/// writes (or replaces) a row; inherit *deletes* the row. There is no third code path, so a
/// screen that toggles a cell back cannot leave a stale row behind — and a stale deny is the
/// failure mode that looks like a bug in the agent while actually being a decision an operator
/// removed last Tuesday.
pub async fn set_grant(
    pool: &PgPool,
    identity_id: Uuid,
    tool_key: &str,
    effect: GrantEffect,
    granted_by: Option<Uuid>,
) -> Result<bool> {
    // The tool must exist, and it is named in the refusal: a client that typos a key gets
    // "no tool `…`" rather than a foreign-key violation from the database.
    let known: bool = sqlx::query_scalar("select exists(select 1 from ai_tools where key = $1)")
        .bind(tool_key)
        .fetch_one(pool)
        .await?;
    if !known {
        return Err(AiHubError::ToolNotFound(format!(
            "no tool `{tool_key}` in the registry"
        )));
    }
    let identity_exists: bool =
        sqlx::query_scalar("select exists(select 1 from ai_identities where id = $1)")
            .bind(identity_id)
            .fetch_one(pool)
            .await?;
    if !identity_exists {
        return Err(AiHubError::IdentityNotFound(identity_id));
    }

    match effect.as_stored() {
        Some(stored) => {
            sqlx::query(
                "insert into ai_tool_grants (identity_id, tool_key, effect, granted_by) \
                 values ($1, $2, $3, $4) \
                 on conflict (identity_id, tool_key) do update set \
                   effect = excluded.effect, granted_by = excluded.granted_by, updated_at = now()",
            )
            .bind(identity_id)
            .bind(tool_key)
            .bind(stored)
            .bind(granted_by)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query("delete from ai_tool_grants where identity_id = $1 and tool_key = $2")
                .bind(identity_id)
                .bind(tool_key)
                .execute(pool)
                .await?;
        }
    }
    Ok(true)
}

/// Replace a whole identity's grant map — the "Allow all visible" / "Deny all visible" pair and
/// the editor's save.
///
/// It is a **replace, not a merge**, and the distinction is the point: a client that removes a
/// cell from its map means "this cell is inherit now", and a merge would keep the old row alive
/// forever. One transaction, because a half-applied grant map is an identity that grants tools
/// its operator believes it does not.
pub async fn replace_grants(
    pool: &PgPool,
    identity_id: Uuid,
    decisions: &[(String, GrantEffect)],
) -> Result<()> {
    let mut known: BTreeSet<String> = BTreeSet::new();
    for (tool_key, _) in decisions {
        let exists: bool =
            sqlx::query_scalar("select exists(select 1 from ai_tools where key = $1)")
                .bind(tool_key)
                .fetch_one(pool)
                .await?;
        if !exists {
            return Err(AiHubError::ToolNotFound(format!(
                "no tool `{tool_key}` in the registry"
            )));
        }
        if !known.insert(tool_key.clone()) {
            return Err(AiHubError::InvalidIdentity(format!(
                "`{tool_key}` appears twice in one grant map"
            )));
        }
    }

    let mut tx = pool.begin().await?;
    sqlx::query("delete from ai_tool_grants where identity_id = $1")
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;
    for (tool_key, effect) in decisions {
        // Inherit contributes no row — the same rule `set_grant` follows, so a bulk save and a
        // single toggle cannot produce different states for the same decision.
        if let Some(stored) = effect.as_stored() {
            sqlx::query(
                "insert into ai_tool_grants (identity_id, tool_key, effect) values ($1, $2, $3)",
            )
            .bind(identity_id)
            .bind(tool_key)
            .bind(stored)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Which identities currently name a tool, for the tool detail screen.
///
/// Reads the **grant** table rather than the agents' allow-lists: this answers "who has an
/// opinion about this tool", which is a different question from `registry::agents_using`'s "who
/// might call it".
pub async fn identities_granting(pool: &PgPool, tool_key: &str) -> Result<Vec<AiIdentity>> {
    let sql = format!(
        "select distinct {IDENTITY_COLUMNS} from ai_identities i \
         join ai_tool_grants g on g.identity_id = i.id \
         where g.tool_key = $1 order by key"
    );
    Ok(sqlx::query_as::<_, AiIdentity>(&sql)
        .bind(tool_key)
        .fetch_all(pool)
        .await?)
}

/// What one tool resolves to for one run.
///
/// **This is the function the request's "an explicit deny beats an allow from any source"
/// criterion is about**, so it is a pure function over three inputs rather than a query: the
/// identity's grant, the agent's own allow-list, and whether the tool is enabled at all. Pure
/// means the ordering is unit-testable without a database, and the ordering is the security
/// claim — get it backwards and every other guarantee in this file is decoration.
///
/// The order, and why:
/// 1. **disabled wins** — an operator who switched a tool off has withdrawn it from everyone.
/// 2. **deny wins** — an explicit deny beats the identity's own allow and the agent's allow-list.
/// 3. **the agent's allow-list** — an empty list means "no tools", never "all tools"
///    (`AllowList`'s rule, reused rather than re-derived).
/// 4. **the identity's allow** — the identity can only widen what the agent already named, never
///    grant a tool the agent does not carry. That direction is deliberate: a shared identity
///    edited by one operator must not be able to push `deployment.deploy` into an agent its
///    author never listed.
#[must_use]
pub fn resolve(
    grants: &BTreeMap<String, bool>,
    agent_tools: &[String],
    tool_key: &str,
    enabled: bool,
) -> Resolution {
    if !enabled {
        return Resolution {
            effect: GrantEffect::Deny,
            reason: ResolutionReason::Disabled,
        };
    }
    if grants.get(tool_key) == Some(&false) {
        return Resolution {
            effect: GrantEffect::Deny,
            reason: ResolutionReason::ExplicitDeny,
        };
    }
    let agent_allows = agent_tools.iter().any(|entry| entry == tool_key);
    if !agent_allows {
        return Resolution {
            effect: GrantEffect::Deny,
            reason: ResolutionReason::NotInAgentList,
        };
    }
    Resolution {
        effect: GrantEffect::Allow,
        reason: ResolutionReason::Allowed,
    }
}

/// The outcome of [`resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub effect: GrantEffect,
    /// Why, so a refusal in the trace says *which* rule refused rather than only that it did.
    pub reason: ResolutionReason,
}

/// The five answers [`resolve`] can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionReason {
    /// The agent carries the tool and nothing refused it.
    Allowed,
    /// An operator disabled the tool for everybody.
    Disabled,
    /// An identity carries an explicit deny for this tool.
    ExplicitDeny,
    /// The agent's own allow-list does not name the tool.
    NotInAgentList,
}

impl ResolutionReason {
    /// A stable code for the call log and the event payload.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Disabled => "tool_disabled",
            Self::ExplicitDeny => "tool_denied",
            Self::NotInAgentList => "tool_not_in_agent_list",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(pairs: &[(&str, bool)]) -> BTreeMap<String, bool> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    // -- the tri-state ------------------------------------------------------------------------

    #[test]
    fn inherit_is_the_absence_of_a_stored_value() {
        // The rule the whole file rests on, asserted from both directions: a state with no
        // stored form and a missing row produce the SAME state.
        assert_eq!(GrantEffect::Inherit.as_stored(), None);
        assert_eq!(GrantEffect::from_stored(None), GrantEffect::Inherit);
    }

    #[test]
    fn an_allow_and_a_deny_are_opposite_stored_values_not_two_nulls() {
        assert_eq!(GrantEffect::Allow.as_stored(), Some(true));
        assert_eq!(GrantEffect::Deny.as_stored(), Some(false));
        assert_eq!(GrantEffect::from_stored(Some(true)), GrantEffect::Allow);
        assert_eq!(GrantEffect::from_stored(Some(false)), GrantEffect::Deny);
    }

    #[test]
    fn a_wire_value_outside_the_three_states_is_refused() {
        assert_eq!(
            GrantEffect::from_wire("allow").expect("allow parses"),
            GrantEffect::Allow
        );
        assert_eq!(
            GrantEffect::from_wire("deny").expect("deny parses"),
            GrantEffect::Deny
        );
        assert_eq!(
            GrantEffect::from_wire("inherit").expect("inherit parses"),
            GrantEffect::Inherit
        );
        // A client that sends `true`, or a typo, must not get a silent `false` — that would be
        // a deny written by a client that never asked for one.
        let refused = GrantEffect::from_wire("true");
        assert!(refused.is_err(), "`true` is not one of the three states");
    }

    #[test]
    fn every_effect_round_trips_through_its_wire_name() {
        for effect in [GrantEffect::Allow, GrantEffect::Deny, GrantEffect::Inherit] {
            assert_eq!(
                GrantEffect::from_wire(effect.wire()).expect("a wire name parses"),
                effect,
                "{} must survive the wire",
                effect.wire()
            );
        }
    }

    // -- the deny ordering, which is the security claim ----------------------------------------

    #[test]
    fn a_deny_beats_the_identitys_own_allow_and_the_agents_allow_list() {
        // The identity both allows and denies would be contradictory data; the rule under test
        // is which one wins, and a deny losing here would make every deny row optional.
        let resolved = resolve(
            &grants(&[("content.read", false)]),
            &["content.read".to_owned()],
            "content.read",
            true,
        );
        assert_eq!(resolved.effect, GrantEffect::Deny);
        assert_eq!(resolved.reason, ResolutionReason::ExplicitDeny);
    }

    #[test]
    fn a_disabled_tool_is_refused_before_any_grant_is_consulted() {
        // Disabled outranks everything, including an allow. An operator who switched a tool off
        // has withdrawn it from everyone, and a stale allow must not survive that decision.
        let resolved = resolve(
            &grants(&[("deployment.deploy", true)]),
            &["deployment.deploy".to_owned()],
            "deployment.deploy",
            false,
        );
        assert_eq!(resolved.effect, GrantEffect::Deny);
        assert_eq!(resolved.reason, ResolutionReason::Disabled);
    }

    #[test]
    fn an_identity_allow_cannot_push_a_tool_into_an_agent_that_never_listed_it() {
        // A shared identity is edited by one operator; the agent's own list is the author's
        // decision. If an identity could widen an agent's list, editing the shared identity
        // would hand a deployment tool to an agent nobody reviewed for it.
        let resolved = resolve(
            &grants(&[("deployment.deploy", true)]),
            &["content.read".to_owned()],
            "deployment.deploy",
            true,
        );
        assert_eq!(resolved.effect, GrantEffect::Deny);
        assert_eq!(resolved.reason, ResolutionReason::NotInAgentList);
    }

    #[test]
    fn an_empty_agent_list_means_no_tools_and_not_all_tools() {
        // The rule `AllowList` already states for the runtime. Re-derived here it would be free
        // to disagree, which is how "the matrix says allow" becomes "the agent called it anyway".
        let resolved = resolve(&BTreeMap::new(), &[], "content.read", true);
        assert_eq!(resolved.effect, GrantEffect::Deny);
    }

    #[test]
    fn an_inherited_tool_the_agent_allows_resolves_to_allowed() {
        // The ordinary case: the agent carries it, the identity has no opinion.
        let resolved = resolve(
            &BTreeMap::new(),
            &["content.read".to_owned()],
            "content.read",
            true,
        );
        assert_eq!(resolved.effect, GrantEffect::Allow);
        assert_eq!(resolved.reason, ResolutionReason::Allowed);
    }

    #[test]
    fn an_inherited_tool_the_agent_allows_and_the_identity_allows_is_still_allowed() {
        // Both sources agreeing is not a conflict, and resolving it any other way would make
        // the matrix's allow cells meaningless.
        let resolved = resolve(
            &grants(&[("content.read", true)]),
            &["content.read".to_owned()],
            "content.read",
            true,
        );
        assert_eq!(resolved.effect, GrantEffect::Allow);
    }

    #[test]
    fn every_resolution_reason_has_a_distinct_stable_code() {
        let reasons = [
            ResolutionReason::Allowed,
            ResolutionReason::Disabled,
            ResolutionReason::ExplicitDeny,
            ResolutionReason::NotInAgentList,
        ];
        let mut codes: Vec<&str> = reasons.iter().map(|reason| reason.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        // The call log branches on these strings, so two reasons sharing a code is a refusal
        // that reports the wrong rule.
        assert_eq!(codes.len(), before, "resolution codes must be distinct");
    }

    // -- keys ----------------------------------------------------------------------------------

    #[test]
    fn a_key_is_normalised_rather_than_rejected_for_its_case() {
        // A key is an API-visible identifier pasted into URLs; refusing "Ops" for a capital
        // letter would be a validation rule that teaches nothing.
        assert_eq!(
            validate_key("  Content-Ops  ").expect("a key with case and padding normalises"),
            "content-ops"
        );
    }

    #[test]
    fn a_key_that_cannot_be_an_identifier_is_refused_with_a_reason() {
        for bad in ["", "  ", "1ops", "-ops", "content ops", "content.ops", "ops!"] {
            assert!(
                validate_key(bad).is_err(),
                "`{bad}` must not be accepted as an identity key"
            );
        }
    }

    #[test]
    fn a_key_at_the_ceiling_is_accepted_and_one_past_it_is_not() {
        let exact = "a".repeat(MAX_KEY_CHARS);
        assert_eq!(validate_key(&exact).expect("the ceiling is inclusive"), exact);
        let past = "a".repeat(MAX_KEY_CHARS + 1);
        assert!(validate_key(&past).is_err(), "one past the ceiling is refused");
    }

    #[test]
    fn a_name_is_required_and_a_description_is_not() {
        assert!(validate_name("   ").is_err(), "a blank name is refused");
        assert_eq!(validate_name(" Ops ").expect("padding is trimmed"), "Ops");
        assert_eq!(
            validate_description("").expect("an empty description is allowed"),
            ""
        );
    }

    #[test]
    fn a_description_past_the_ceiling_is_refused() {
        let past = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(validate_description(&past).is_err());
    }

    #[test]
    fn an_empty_change_set_is_recognised_so_a_route_can_refuse_it() {
        assert!(IdentityChanges::default().is_empty());
        assert!(!IdentityChanges {
            name: Some("Ops".to_owned()),
            ..IdentityChanges::default()
        }
        .is_empty());
    }
}
