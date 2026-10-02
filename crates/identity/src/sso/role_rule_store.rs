//! Storing and reading the role rules (REQ-065, slice 3).
//!
//! The language lives in [`super::role_rules`]; this is the half that talks to
//! `provider_role_rules`. It is deliberately small for the same reason [`super::mappings`] is: the
//! only interesting operation is [`replace_rules`], and the only interesting question is whether
//! it is atomic.
//!
//! It is a transaction, and the reason is sharper here than it was for the attribute map. The map's
//! worst intermediate state is "a provider has no email mapping", which refuses a sign-in. The
//! rules' worst intermediate state is worse: a provider whose role set has been emptied between
//! the delete and the insert grants **nobody** the role they should have, and the first person to
//! notice is a colleague whose access stopped working — silently, until somebody reads the audit
//! log. Delete-then-insert without a transaction would need a lock on the sign-in path, and a
//! lock is a worse answer than a transaction that is atomic by construction.
//!
//! A second ordering problem lives in the table itself: the unique index on
//! `(provider_id, position)` is what makes the order mean something, so the writes have to be
//! **in descending position order** as well as inside the transaction. Renumbering a set from
//! `0,1,2` to `1,2,3` through ascending writes would collide on `1` with the row that is about to
//! be deleted. That is not a theoretical hazard — it is what a drag does.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};
use crate::sso::role_rules::{RoleRule, RoleRules, ScopeType, WhenKind, WhenOperator};

/// One stored row: the editor row plus its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRule {
    /// Row id.
    pub id: Uuid,
    /// The provider it belongs to.
    pub provider_id: Uuid,
    /// The rule itself.
    pub rule: RoleRule,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

/// The columns, kept in one place so a schema change is a one-line edit.
const COLUMNS: &str = "id, provider_id, position, when_kind, when_key, when_operator, \
                       when_value, role_id, scope_type, site_id, stop, enabled, created_at";

/// A row as it comes back from the database, where the enums are text.
///
/// A private shape for the same reason as in [`super::mappings`]: a failed parse should cost one
/// row, not the whole editor.
#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    provider_id: Uuid,
    position: i32,
    when_kind: String,
    when_key: String,
    when_operator: String,
    when_value: String,
    role_id: Uuid,
    scope_type: String,
    site_id: Option<Uuid>,
    stop: bool,
    enabled: bool,
    created_at: OffsetDateTime,
}

impl From<Row> for StoredRule {
    fn from(row: Row) -> Self {
        // The `check` constraints make these parses infallible, and a parse that failed here would
        // be a migration that did not land. `unwrap_or` keeps a corrupt row from taking a whole
        // list down: the panel shows the rest of the set and this row falls back to a shape that is
        // visible and fixable, rather than to one that silently matches everything.
        let rule = RoleRule {
            position: row.position,
            when_kind: WhenKind::parse(&row.when_kind).unwrap_or(WhenKind::Claim),
            when_key: row.when_key,
            when_operator: WhenOperator::parse(&row.when_operator).unwrap_or(WhenOperator::Equals),
            when_value: row.when_value,
            role_id: row.role_id,
            scope_type: ScopeType::parse(&row.scope_type).unwrap_or(ScopeType::Organization),
            site_id: row.site_id,
            stop: row.stop,
            enabled: row.enabled,
        };
        Self {
            id: row.id,
            provider_id: row.provider_id,
            rule,
            created_at: row.created_at,
        }
    }
}

/// Read a provider's whole rule set, in search order.
pub async fn list_rules(pool: &PgPool, provider_id: Uuid) -> Result<Vec<StoredRule>> {
    let rows = sqlx::query_as::<_, Row>(&format!(
        "select {COLUMNS} from provider_role_rules \
         where provider_id = $1 order by position"
    ))
    .bind(provider_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(StoredRule::from).collect())
}

/// Read a provider's rules as the language type, ready to resolve.
///
/// This is the *only* way the sign-in path and the dry run obtain rules, which is what stops them
/// from disagreeing: they cannot, because there is nothing to disagree with until the rows are
/// read through here.
pub async fn load_rules(pool: &PgPool, provider_id: Uuid) -> Result<RoleRules> {
    let rows = list_rules(pool, provider_id).await?;
    Ok(RoleRules::new(
        rows.into_iter().map(|row| row.rule).collect(),
    ))
}

/// Replace a provider's rule set with exactly what was submitted.
///
/// Atomic, and the validation happens **before** the first delete. An empty set is legitimate —
/// that is how an operator clears it — so an empty list is written as a delete of every row
/// rather than refused.
pub async fn replace_rules(
    pool: &PgPool,
    provider_id: Uuid,
    rules: RoleRules,
) -> Result<Vec<StoredRule>> {
    let problems = rules.validate();
    if !problems.is_empty() {
        return Err(IdentityError::InvalidProvider(
            problems
                .iter()
                .map(|problem| problem.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let rules = rules.normalized();
    let mut transaction = pool.begin().await?;

    sqlx::query("delete from provider_role_rules where provider_id = $1")
        .bind(provider_id)
        .execute(&mut *transaction)
        .await?;

    for rule in &rules.rules {
        sqlx::query(
            "insert into provider_role_rules \
                 (provider_id, position, when_kind, when_key, when_operator, when_value, \
                  role_id, scope_type, site_id, stop, enabled) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(provider_id)
        .bind(rule.position)
        .bind(rule.when_kind.as_str())
        .bind(&rule.when_key)
        .bind(rule.when_operator.as_str())
        .bind(&rule.when_value)
        .bind(rule.role_id)
        .bind(rule.scope_type.as_str())
        .bind(rule.site_id)
        .bind(rule.stop)
        .bind(rule.enabled)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    list_rules(pool, provider_id).await
}

/// How many rules a provider carries, without reading them.
///
/// The enablement gate and the delete guard need a number, not a set, and a `select count(*)` on a
/// secondary index is cheaper than materialising every rule for a check that only asks whether
/// there are any.
pub async fn count_rules(pool: &PgPool, provider_id: Uuid) -> Result<i64> {
    let count: (i64,) =
        sqlx::query_as("select count(*) from provider_role_rules where provider_id = $1")
            .bind(provider_id)
            .fetch_one(pool)
            .await?;
    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_round_trips_through_the_text_enums() {
        let row = Row {
            id: Uuid::from_u128(1),
            provider_id: Uuid::from_u128(2),
            position: 3,
            when_kind: "group".to_owned(),
            when_key: "groups".to_owned(),
            when_operator: "starts_with".to_owned(),
            when_value: "eng".to_owned(),
            role_id: Uuid::from_u128(3),
            scope_type: "site".to_owned(),
            site_id: Some(Uuid::from_u128(4)),
            stop: false,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let stored = StoredRule::from(row);
        assert_eq!(stored.rule.when_kind, WhenKind::Group);
        assert_eq!(stored.rule.when_operator, WhenOperator::StartsWith);
        assert_eq!(stored.rule.scope_type, ScopeType::Site);
        assert_eq!(stored.rule.position, 3);
        assert!(!stored.rule.stop);
    }

    #[test]
    fn a_row_whose_enum_text_is_unknown_degrades_to_a_visible_shape_not_a_wildcard() {
        // A corrupt row must not become a rule that matches everything: the safe-looking fallback
        // for an unknown `when_kind` is `claim`, which needs a key and therefore needs evidence.
        let row = Row {
            id: Uuid::from_u128(1),
            provider_id: Uuid::from_u128(2),
            position: 0,
            when_kind: "moon_phase".to_owned(),
            when_key: String::new(),
            when_operator: "equals".to_owned(),
            when_value: String::new(),
            role_id: Uuid::from_u128(3),
            scope_type: "organization".to_owned(),
            site_id: None,
            stop: true,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let stored = StoredRule::from(row);
        assert_eq!(stored.rule.when_kind, WhenKind::Claim);
        assert!(
            !stored.rule.matches(&crate::sso::claims::Identity {
                subject: "u".to_owned(),
                email: "a@b.test".to_owned(),
                display_name: None,
                groups: Vec::new(),
                attributes: serde_json::Map::new(),
            }),
            "an unreadable rule must match nothing, not everything"
        );
    }
}
