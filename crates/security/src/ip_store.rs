//! The IP access rules SQL: the two lists, the add and the remove (REQ-012, slice 4).
//!
//! Slice 4's rule is the same one slices 1–3 followed, and it is worth stating because this
//! table is the one place in the security centre where a bug is *visible from outside*:
//!
//! * **The read returns what is stored, never a default.** [`list`] hands back every live rule
//!   *and every expired one*, because the evaluator needs the expired rules too — that is what
//!   lets the tester answer "the deny that would have matched expired an hour ago" instead of
//!   "no rule matched", which an operator reads as *allowed* and acts on. A read that filtered
//!   by expiry in SQL would make the explanation unreachable.
//! * **The add is an insert, not an upsert.** `unique (kind, cidr)` means re-adding a network
//!   that is already listed is refused, and the refusal names the network — re-adding is almost
//!   always a paste mistake, and silently refreshing it would move a rule an incident response
//!   depends on without recording that anyone did.
//! * **The delete is by id, and the id comes from the screen.** Nothing here deletes by address:
//!   a rule removed by address rather than by row would take whichever duplicate it found first.
//!
//! The evaluator itself is [`crate::ip_rules::evaluate`] and is a pure function — no SQL in this
//! module decides whether an address is allowed.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecurityError};
use crate::ip_rules::{IpRule, RuleKind};
use crate::vocabulary::MAX_PAGE;

/// One row as the database hands it over, and the conversion to the domain type.
///
/// `kind` is `String` here rather than [`RuleKind`], for the reason on [`crate::ip_rules::IpRule`]:
/// the domain module stays a pure policy with no sqlx traits in it, and this conversion — which
/// is also where a stored kind the platform does not recognise becomes an `Invalid` error — sits
/// next to the SQL that produced it. A row whose `kind` is neither `allow` nor `deny` cannot
/// exist (the check constraint holds), so this is a backstop for a database the migration did not
/// create, not a path an operator can reach.
#[derive(Debug, sqlx::FromRow)]
pub struct IpRuleRow {
    /// The rule's id.
    pub id: Uuid,
    /// Which list, as stored.
    pub kind: String,
    /// The network in canonical text.
    pub cidr: String,
    /// Why the rule exists.
    pub note: String,
    /// Who added it.
    pub created_by: Option<Uuid>,
    /// When it was added.
    pub created_at: OffsetDateTime,
    /// When it stops applying.
    pub expires_at: Option<OffsetDateTime>,
}

impl IpRuleRow {
    /// Convert into the domain type.
    ///
    /// # Errors
    /// Returns [`SecurityError::Invalid`] when the stored `kind` is not one of the two.
    pub fn into_rule(self) -> Result<IpRule> {
        Ok(IpRule {
            id: self.id,
            kind: RuleKind::parse(&self.kind)?,
            cidr: self.cidr,
            note: self.note,
            created_by: self.created_by,
            created_at: self.created_at,
            expires_at: self.expires_at,
        })
    }
}

/// Every rule, newest first, expired ones included.
///
/// Expired rows are **not** filtered here. The evaluator reports an expired match as the
/// explanation for a deny that stopped applying, and a read that dropped them would make that
/// explanation impossible to produce.
pub async fn list(pool: &PgPool) -> Result<Vec<IpRule>> {
    let sql = "select id, kind, cidr::text, note, created_by, created_at, expires_at \
               from security_ip_rules \
              order by created_at desc, id";
    let fetched = sqlx::query_as::<_, IpRuleRow>(sql).fetch_all(pool).await?;
    fetched.into_iter().map(IpRuleRow::into_rule).collect()
}

/// The rules of one list, for the panel's two tabs.
pub async fn list_kind(pool: &PgPool, kind: RuleKind, limit: i64) -> Result<Vec<IpRule>> {
    let sql = "select id, kind, cidr::text, note, created_by, created_at, expires_at \
               from security_ip_rules \
              where kind = $1 \
              order by created_at desc, id \
              limit $2";
    let fetched = sqlx::query_as::<_, IpRuleRow>(sql)
        .bind(kind.as_str())
        .bind(limit.clamp(1, MAX_PAGE as i64))
        .fetch_all(pool)
        .await?;
    fetched.into_iter().map(IpRuleRow::into_rule).collect()
}

/// How many rules each list holds — `(deny, allow)`.
///
/// One row's worth of a `count(*) filter`, so the two numbers on the screen's summary line come
/// from one round trip and cannot describe different moments.
pub async fn counts(pool: &PgPool) -> Result<(i64, i64)> {
    sqlx::query_as::<_, (i64, i64)>(
        "select count(*) filter (where kind = 'deny'), count(*) filter (where kind = 'allow') \
           from security_ip_rules",
    )
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Add one rule.
///
/// `cidr` arrives already parsed and normalised from [`crate::ip_rules::parse_cidr`], so this
/// trusts the text and lets the column be the backstop: a caller that skipped the parser gets
/// Postgres's own rejection, which names the input just as well.
pub async fn add(
    pool: &PgPool,
    kind: RuleKind,
    cidr: &str,
    note: &str,
    expires_at: Option<OffsetDateTime>,
    created_by: Uuid,
) -> Result<IpRule> {
    // A duplicate is a caller error rather than a database failure, so it is caught here and
    // restated with the network — the unique violation's own message does not name it, and this
    // one arrives before the row is written rather than as a rollback.
    if let Some(existing) = find(pool, kind, cidr).await? {
        return Err(SecurityError::invalid(format!(
            "{} is already on the {} list — remove it first if you meant to replace it",
            existing.cidr,
            kind.as_str()
        )));
    }

    let sql = "insert into security_ip_rules (cidr, kind, note, created_by, expires_at) \
               values ($1::cidr, $2, $3, $4, $5) \
            returning id, kind, cidr::text, note, created_by, created_at, expires_at";
    let row = sqlx::query_as::<_, IpRuleRow>(sql)
        .bind(cidr)
        .bind(kind.as_str())
        .bind(note)
        .bind(created_by)
        .bind(expires_at)
        .fetch_one(pool)
        .await?;
    row.into_rule()
}

/// One rule by its id, or `None`.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<IpRule>> {
    let sql = "select id, kind, cidr::text, note, created_by, created_at, expires_at \
               from security_ip_rules where id = $1";
    let row = sqlx::query_as::<_, IpRuleRow>(sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(IpRuleRow::into_rule).transpose()
}

/// One rule by its network and list, or `None` — the duplicate check's read.
async fn find(pool: &PgPool, kind: RuleKind, cidr: &str) -> Result<Option<IpRule>> {
    let sql = "select id, kind, cidr::text, note, created_by, created_at, expires_at \
               from security_ip_rules where kind = $1 and cidr = $2::cidr";
    let row = sqlx::query_as::<_, IpRuleRow>(sql)
        .bind(kind.as_str())
        .bind(cidr)
        .fetch_optional(pool)
        .await?;
    row.map(IpRuleRow::into_rule).transpose()
}

/// Remove one rule by its id.
///
/// Returns whether a row went, so the route can answer `404` for a rule that is already gone
/// rather than reporting a success that changed nothing.
pub async fn remove(pool: &PgPool, id: Uuid) -> Result<bool> {
    let removed: Option<Uuid> =
        sqlx::query_scalar("delete from security_ip_rules where id = $1 returning id")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(removed.is_some())
}
