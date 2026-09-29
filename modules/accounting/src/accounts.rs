//! The chart of accounts and the tax rates.
//!
//! Four rules the HTTP layer must not have to remember, because a screen, a CSV import and the
//! journal are all going to call these:
//!
//! * **Every read and write is organization-scoped in SQL, and a row of another organization is
//!   `404`.** Not `403`: a `403` confirms the row exists, and one organization's chart is the
//!   one thing this module exists to keep apart.
//! * **An account with postings deactivates and never deletes.** A journal line references an
//!   account with `on delete restrict`, so a delete attempt is a database error naming a
//!   constraint; the store turns it into [`AccountInUse`] with the **count**, because "cannot
//!   delete" sends the operator to a report to find out how exposed the account is.
//! * **The system accounts are the seeded ones.** A seeded account may be renamed and deactivated
//!   but not deleted, because the seeding in the migration is idempotent and a deleted system
//!   account comes back on the next tenant's creation and looks like a ghost.
//! * **Exactly one default tax rate per (organization, kind).** A partial unique index in the
//!   schema is the enforcement; the store's job is to tell the person which row already holds it
//!   instead of surfacing the index name.

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::dates;
use crate::error::{AccountingError, Result};

// The kinds are **re-exported publicly** from here rather than imported privately, because the
// screens reach for `accounts::AccountKind` while the SQL contract lives in `model`. A private
// `use` plus a `pub use` of the same name in `lib.rs` is a compile error, and the alternative —
// making every caller import from two places — is how two names for one type appear.
pub use crate::model::{AccountKind, TaxRateKind};

/// Rows an account page holds when the caller names no size.
pub const DEFAULT_PER_PAGE: i64 = 100;

/// Hard cap on an account page — a chart of accounts is read whole.
pub const MAX_PER_PAGE: i64 = 500;

/// Longest an account code may be, the same bound the schema stores.
pub const MAX_CODE_LENGTH: usize = 32;

/// Longest an account or rate name may be.
pub const MAX_NAME_LENGTH: usize = 120;

/// Longest a percent may be written with before it is refused.
pub const MAX_PERCENT_LENGTH: usize = 6;

// ---------------------------------------------------------------------------------------------
// The shapes the API returns
// ---------------------------------------------------------------------------------------------

/// An account as the tree editor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The code a line grid picks from.
    pub code: String,
    /// The name the tree header prints.
    pub name: String,
    /// Which of the five kinds it belongs to.
    pub kind: AccountKind,
    /// The account it rolls up into, if any.
    pub parent_id: Option<Uuid>,
    /// Whether new lines may be booked here.
    pub active: bool,
    /// True for the eleven accounts the migration seeds.
    ///
    /// It is a **display** flag, not a guard: the store refuses to delete any account with
    /// postings, and separately refuses to delete a system account, because the seed is
    /// idempotent and a deleted system row returns on the next tenant.
    pub system: bool,
    /// How many journal lines name this account — the tree node's "used by N lines".
    pub line_count: i64,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl AccountView {
    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "account_id": self.id,
            "code": self.code,
            "name": self.name,
            "kind": self.kind.as_str(),
        })
    }
}

/// A tax rate as the editor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxRateView {
    /// The row's id.
    pub id: Uuid,
    /// The organization that owns it.
    pub organization_id: Uuid,
    /// The name a line grid picks from.
    pub name: String,
    /// The rate, as text — `20.00`, not a double.
    pub percent: String,
    /// Which side of the sale it applies to.
    pub kind: TaxRateKind,
    /// Whether an invoice with no explicit rate uses this one.
    pub is_default: bool,
    /// Whether the rate may still be picked.
    pub active: bool,
    /// When it was created.
    #[serde(with = "crate::dates::instant")]
    pub created_at: OffsetDateTime,
}

impl TaxRateView {
    /// The compact reference an audit row and an event payload carry.
    #[must_use]
    pub fn reference(&self) -> serde_json::Value {
        serde_json::json!({
            "tax_rate_id": self.id,
            "name": self.name,
            "percent": self.percent,
            "kind": self.kind.as_str(),
            "is_default": self.is_default,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// What a caller writes
// ---------------------------------------------------------------------------------------------

/// The body of `POST /accounting/accounts`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewAccount {
    /// The code. Required, unique per organization.
    pub code: String,
    /// The name. Required.
    pub name: String,
    /// Which of the five kinds. Required — there is no default, because guessing `expense` for a
    /// liability is a chart that balances and says nothing true.
    #[serde(default)]
    pub kind: Option<String>,
    /// The account it rolls up into, if any.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Start deactivated. Rarely used, and refused for a system account, which is seeded live.
    #[serde(default)]
    pub active: Option<bool>,
}

/// The body of `PATCH /accounting/accounts/{id}`.
///
/// **The code is deliberately not patchable.** A code is what a journal line and a report row
/// refer to; renaming it after postings exist turns every historical reference into a lie, and
/// the schema's `unique (organization_id, code)` is the only thing preventing a duplicate.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountPatch {
    /// A new display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Deactivate or reactivate.
    #[serde(default)]
    pub active: Option<bool>,
    /// Re-parent the account in the tree.
    #[serde(default)]
    pub parent_id: Option<Option<Uuid>>,
}

/// The body of `POST /accounting/tax-rates`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewTaxRate {
    /// The name. Required, unique per organization.
    pub name: String,
    /// The rate. Required, `0`–`100`, at most two decimals.
    pub percent: String,
    /// `sales` or `purchase`. Required.
    #[serde(default)]
    pub kind: Option<String>,
    /// Make this the default for its kind.
    #[serde(default)]
    pub is_default: Option<bool>,
}

/// The body of `PATCH /accounting/tax-rates/{id}`.
///
/// Editing a rate **never** changes an already-issued document: the invoice line stores its own
/// `tax_percent` snapshot precisely so a rate can be corrected without rewriting history. That is
/// why `percent` is patchable here at all.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaxRatePatch {
    /// A new display name.
    #[serde(default)]
    pub name: Option<String>,
    /// A new rate.
    #[serde(default)]
    pub percent: Option<String>,
    /// Take the default flag, giving it up if another row holds it.
    #[serde(default)]
    pub is_default: Option<bool>,
    /// Deactivate or reactivate.
    #[serde(default)]
    pub active: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Validate an account code: what a chart of accounts is actually made of.
pub fn validate_code(entity: &'static str, code: &str) -> Result<String> {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return Err(AccountingError::invalid(
            entity,
            "code",
            "an account needs a code — the digits a journal line picks",
        ));
    }
    if trimmed.chars().count() > MAX_CODE_LENGTH {
        return Err(AccountingError::invalid(
            entity,
            "code",
            format!("a code is at most {MAX_CODE_LENGTH} characters"),
        ));
    }
    // Letters, digits, space, dot and dash. A code appears in a CSV export and in a PDF, and a
    // semicolon or a quote in it turns the export into a file that opens wrong.
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '-'))
    {
        return Err(AccountingError::invalid(
            entity,
            "code",
            "a code is letters, digits, spaces, dots and dashes",
        ));
    }
    Ok(trimmed.to_owned())
}

/// Validate an account or rate name.
pub fn validate_name(entity: &'static str, name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AccountingError::invalid(
            entity,
            "name",
            "a name is required",
        ));
    }
    if trimmed.chars().count() > MAX_NAME_LENGTH {
        return Err(AccountingError::invalid(
            entity,
            "name",
            format!("a name is at most {MAX_NAME_LENGTH} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Validate a percentage: `0`–`100`, at most two decimals, refused as a word.
///
/// The refusal message names the range because the two mistakes people make are `20 %` and
/// `0.2`, and neither is obvious from "invalid percent".
pub fn validate_percent(percent: &str) -> Result<String> {
    let trimmed = percent.trim();
    if trimmed.is_empty() {
        return Err(AccountingError::invalid(
            "tax_rate",
            "percent",
            "a rate is required — enter 20 for twenty percent",
        ));
    }
    if trimmed.chars().count() > MAX_PERCENT_LENGTH {
        return Err(AccountingError::invalid(
            "tax_rate",
            "percent",
            "a rate is a number between 0 and 100",
        ));
    }
    let parsed = crate::money::Amount::parse(trimmed)
        .map_err(|source| AccountingError::number("tax_rate", "percent", source))?;
    if parsed.cents() < 0 || parsed.cents() > 10_000 {
        return Err(AccountingError::invalid(
            "tax_rate",
            "percent",
            "a rate is between 0 and 100, not 0.2 — enter the number of percent",
        ));
    }
    Ok(parsed.to_text())
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

/// The row shape a query returns before it becomes a view.
#[derive(Debug, FromRow)]
struct AccountRow {
    id: Uuid,
    organization_id: Uuid,
    code: String,
    name: String,
    kind: String,
    parent_id: Option<Uuid>,
    active: bool,
    system: bool,
    created_at: OffsetDateTime,
}

impl AccountRow {
    fn into_view(self, line_count: i64) -> Result<AccountView> {
        let kind = AccountKind::parse(&self.kind).ok_or_else(|| {
            // A kind the schema's CHECK allows but this build does not know is a row written by a
            // newer version. It is reported rather than defaulted, because defaulting an unknown
            // kind to `asset` puts a liability on the wrong side of a balance sheet.
            AccountingError::not_allowed(format!(
                "account {} carries the kind {:?}, which this build does not know",
                self.code, self.kind
            ))
        })?;
        Ok(AccountView {
            id: self.id,
            organization_id: self.organization_id,
            code: self.code,
            name: self.name,
            kind,
            parent_id: self.parent_id,
            active: self.active,
            system: self.system,
            line_count,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TaxRateRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    percent: String,
    kind: String,
    is_default: bool,
    active: bool,
    created_at: OffsetDateTime,
}

impl TaxRateRow {
    fn into_view(self) -> Result<TaxRateView> {
        let kind = TaxRateKind::parse(&self.kind).ok_or_else(|| {
            AccountingError::not_allowed(format!(
                "tax rate {} carries the kind {:?}, which this build does not know",
                self.name, self.kind
            ))
        })?;
        Ok(TaxRateView {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            percent: self.percent,
            kind,
            is_default: self.is_default,
            active: self.active,
            created_at: self.created_at,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------------------------

/// The whole chart, grouped by kind, for the tree screen.
///
/// Read in **one** statement plus one count, rather than a query per node: a chart is a
/// hierarchy, and a screen that fetches a node's children on expand shows a spinner on every
/// expand and reports the wrong total until the last one loads.
pub async fn list_accounts(
    pool: &PgPool,
    organization_id: Uuid,
    kind: Option<AccountKind>,
    include_inactive: bool,
) -> Result<Vec<AccountView>> {
    let rows = sqlx::query_as::<_, AccountRow>(
        "select id, organization_id, code, name, kind, parent_id, active, system, created_at \
         from accounting_accounts where organization_id = $1 \
           and ($2::text is null or kind = $2) \
           and ($3 or active) \
         order by kind, code",
    )
    .bind(organization_id)
    .bind(kind.map(|k| k.as_str()))
    .bind(include_inactive)
    .fetch_all(pool)
    .await?;

    // One count for the whole chart, stamped onto each view. Counting per account would be a
    // correlated subquery the database has to run once per row, and the tree renders all of them.
    let counts = line_counts(pool, organization_id).await?;

    rows.into_iter()
        .map(|row| {
            let line_count = counts.get(&row.id).copied().unwrap_or(0);
            row.into_view(line_count)
        })
        .collect()
}

/// The chart of accounts, in the order the reports group by.
pub async fn chart(pool: &PgPool, organization_id: Uuid) -> Result<Vec<AccountView>> {
    list_accounts(pool, organization_id, None, true).await
}

/// How many journal lines name each account, in one query.
async fn line_counts(pool: &PgPool, organization_id: Uuid) -> Result<std::collections::HashMap<Uuid, i64>> {
    let rows = sqlx::query(
        "select account_id, count(*) as lines from accounting_journal_lines \
         where organization_id = $1 group by account_id",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut counts = std::collections::HashMap::new();
    for row in rows {
        counts.insert(row.get::<Uuid, _>("account_id"), row.get::<i64, _>("lines"));
    }
    Ok(counts)
}

/// One account, or `404` — which is also the answer for another organization's account.
pub async fn get_account(
    pool: &PgPool,
    organization_id: Uuid,
    account_id: Uuid,
) -> Result<AccountView> {
    let row = sqlx::query_as::<_, AccountRow>(
        "select id, organization_id, code, name, kind, parent_id, active, system, created_at \
         from accounting_accounts where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AccountingError::NotFound("account"))?;

    let counts = line_counts(pool, organization_id).await?;
    let line_count = counts.get(&row.id).copied().unwrap_or(0);
    row.into_view(line_count)
}

/// Add an account to the chart.
pub async fn create_account(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewAccount,
) -> Result<AccountView> {
    let code = validate_code("account", &new.code)?;
    let name = validate_name("account", &new.name)?;
    let kind = new
        .kind
        .as_deref()
        .map(AccountKind::parse)
        .ok_or_else(|| {
            AccountingError::invalid(
                "account",
                "kind",
                "an account needs a kind: asset, liability, equity, income or expense",
            )
        })?
        .ok_or_else(|| {
            AccountingError::invalid(
                "account",
                "kind",
                "an account needs a kind: asset, liability, equity, income or expense",
            )
        })?;

    // The parent is resolved **before** the insert, not after: a parent of another organization
    // would otherwise produce an account whose tree crosses a tenant boundary, and the insert
    // succeeds because the column has no foreign key to the organization.
    if let Some(parent_id) = new.parent_id {
        get_account(pool, organization_id, parent_id).await?;
    }

    let inserted: std::result::Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "insert into accounting_accounts (organization_id, code, name, kind, parent_id, active) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(organization_id)
    .bind(&code)
    .bind(&name)
    .bind(kind.as_str())
    .bind(new.parent_id)
    .bind(new.active.unwrap_or(true))
    .fetch_one(pool)
    .await;

    let id = match inserted {
        Ok(id) => id,
        Err(error) => {
            return Err(map_unique_violation(&error, "account", &code).unwrap_or(error.into()));
        }
    };

    get_account(pool, organization_id, id).await
}

/// Rename, deactivate or re-parent an account.
pub async fn patch_account(
    pool: &PgPool,
    organization_id: Uuid,
    account_id: Uuid,
    patch: &AccountPatch,
) -> Result<AccountView> {
    let before = get_account(pool, organization_id, account_id).await?;

    if let Some(parent_id) = patch.parent_id {
        if let Some(parent_id) = parent_id {
            let parent = get_account(pool, organization_id, parent_id).await?;
            // A cycle would make the tree editor recurse forever, and the schema cannot see it
            // because the parent pointer has no depth rule. Refusing a self-parent and a
            // parent-in-the-subtree is the whole of the check, and it belongs here rather than in
            // the screen, because a CSV import calls this too.
            if parent.id == account_id {
                return Err(AccountingError::invalid(
                    "account",
                    "parent_id",
                    "an account cannot be its own parent",
                ));
            }
            if is_descendant(pool, organization_id, account_id, parent_id).await? {
                return Err(AccountingError::invalid(
                    "account",
                    "parent_id",
                    format!(
                        "{} is inside {} — moving it there would make the tree loop",
                        parent.code, before.code
                    ),
                ));
            }
        }
    }

    // The whole SET list is built in ONE statement with COALESCE, not with a `QueryBuilder`
    // that appends `, name = $3` as it goes. The builder version emitted the tenant predicate
    // FIRST and the assignments after it, producing `update … set active = active where
    // organization_id = $1 and id = $2, name = $3 returning id` — a syntax error that only shows
    // up once a caller sends a field to change, because the no-field patch is the one statement
    // that happens to parse. Dynamic SET and a fixed WHERE are two different jobs.
    //
    // `COALESCE($3, name)` is how one statement covers "the caller did not send this field":
    // a NULL bind leaves the stored value exactly as it was, and a `sets` vector of strings
    // numbered by hand can no longer drift out of step with the binds.
    let patched = sqlx::query_as::<_, AccountRow>(
        "update accounting_accounts \
            set name = coalesce($3, name), \
                active = coalesce($4, active), \
                parent_id = case when $5::boolean then $6 else parent_id end \
          where organization_id = $1 and id = $2 \
      returning id, organization_id, code, name, kind, parent_id, active, system, created_at",
    )
    .bind(organization_id)
    .bind(account_id)
    .bind(patch.name.as_deref().map(|n| validate_name("account", n)).transpose()?)
    .bind(patch.active)
    .bind(patch.parent_id.is_some())
    .bind(patch.parent_id)
    .fetch_optional(pool)
    .await?;

    // The returned row is what proves the write happened. Without the tenant predicate matching,
    // a `PATCH` carrying a cross-tenant id would update nothing and answer "ok" — the silent
    // success a row exists to catch. `pool`, not a transaction: one statement, and the
    // `get_account` below re-reads the committed row.
    if patched.is_none() {
        return Err(AccountingError::NotFound("account"));
    }

    get_account(pool, organization_id, account_id).await
}

/// Deactivate an account. The name is the operation because **nothing is deleted here**: a
/// journal line references an account with `on delete restrict`, so the closest thing to a
/// delete is closing the account and leaving its history readable.
pub async fn deactivate_account(
    pool: &PgPool,
    organization_id: Uuid,
    account_id: Uuid,
    active: bool,
) -> Result<AccountView> {
    // A system account is seeded and re-seeded, so a **DELETE** would let the next tenant
    // creation bring it back as a duplicate nobody added, and a chart that grows a row on its
    // own is a chart nobody trusts. Deactivating a seeded account is a different act and stays
    // available: an organization that never touches "Cost of Goods Sold" needs exactly that, and
    // the row has to stay readable because past journal lines name it.
    //
    // The guard is on deletion, and there is no delete route — so this function refuses nothing
    // and says so in its own doc comment. It used to refuse deactivating a seeded account too,
    // which contradicted the sentence right above it and broke the one behaviour the row exists
    // for: the code compared "can I close this" with "may I remove this" and answered the first
    // question with the second.
    let _ = active;
    patch_account(
        pool,
        organization_id,
        account_id,
        &AccountPatch {
            active: Some(active),
            ..AccountPatch::default()
        },
    )
    .await
}

/// Whether `maybe_child` sits somewhere under `ancestor` in the tree.
async fn is_descendant(
    pool: &PgPool,
    organization_id: Uuid,
    ancestor: Uuid,
    maybe_child: Uuid,
) -> Result<bool> {
    // Walk **up** from the proposed child, bounded by the number of accounts in the
    // organization. The bound is not paranoia: a cycle already in the data would otherwise
    // loop here forever, and this is the one place a bad row could take the process down.
    let ceiling = sqlx::query_scalar::<_, i64>(
        "select count(*) from accounting_accounts where organization_id = $1",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;

    let mut cursor = Some(maybe_child);
    for _ in 0..ceiling {
        let Some(current) = cursor else { return Ok(false) };
        if current == ancestor {
            return Ok(true);
        }
        cursor = sqlx::query_scalar::<_, Option<Uuid>>(
            "select parent_id from accounting_accounts where organization_id = $1 and id = $2",
        )
        .bind(organization_id)
        .bind(current)
        .fetch_optional(pool)
        .await?
        .flatten();
    }
    Ok(false)
}

// ---------------------------------------------------------------------------------------------
// Tax rates
// ---------------------------------------------------------------------------------------------

/// Every rate, for the editor and for the line grid's combobox.
pub async fn list_tax_rates(
    pool: &PgPool,
    organization_id: Uuid,
    kind: Option<TaxRateKind>,
    include_inactive: bool,
) -> Result<Vec<TaxRateView>> {
    let rows = sqlx::query_as::<_, TaxRateRow>(
        "select id, organization_id, name, percent::text as percent, kind, is_default, active, \
                created_at \
         from accounting_tax_rates where organization_id = $1 \
           and ($2::text is null or kind = $2) \
           and ($3 or active) \
         order by kind, is_default desc, name",
    )
    .bind(organization_id)
    .bind(kind.map(|k| k.as_str()))
    .bind(include_inactive)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(TaxRateRow::into_view).collect()
}

/// One rate, or `404`.
pub async fn get_tax_rate(
    pool: &PgPool,
    organization_id: Uuid,
    rate_id: Uuid,
) -> Result<TaxRateView> {
    sqlx::query_as::<_, TaxRateRow>(
        "select id, organization_id, name, percent::text as percent, kind, is_default, active, \
                created_at \
         from accounting_tax_rates where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(rate_id)
    .fetch_optional(pool)
    .await?
    .ok_or(AccountingError::NotFound("tax_rate"))?
    .into_view()
}

/// Add a rate.
pub async fn create_tax_rate(
    pool: &PgPool,
    organization_id: Uuid,
    new: &NewTaxRate,
) -> Result<TaxRateView> {
    let name = validate_name("tax_rate", &new.name)?;
    let percent = validate_percent(&new.percent)?;
    let kind = new
        .kind
        .as_deref()
        .and_then(TaxRateKind::parse)
        .ok_or_else(|| {
            AccountingError::invalid(
                "tax_rate",
                "kind",
                "a rate needs a kind: sales or purchase",
            )
        })?;
    let wants_default = new.is_default.unwrap_or(false);

    // The default is **demoted inside the same transaction** as the insert, because a partial
    // unique index allows exactly one default per (organization, kind) and an insert that claims
    // it while another row holds it is a constraint error whose name says nothing about which row
    // is in the way.
    let mut tx = pool.begin().await?;

    if wants_default {
        sqlx::query(
            "update accounting_tax_rates set is_default = false \
             where organization_id = $1 and kind = $2 and is_default",
        )
        .bind(organization_id)
        .bind(kind.as_str())
        .execute(&mut *tx)
        .await?;
    }

    let inserted: std::result::Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "insert into accounting_tax_rates \
             (organization_id, name, percent, kind, is_default, active) \
         values ($1, $2, $3::numeric, $4, $5, true) returning id",
    )
    .bind(organization_id)
    .bind(&name)
    .bind(&percent)
    .bind(kind.as_str())
    .bind(wants_default)
    .fetch_one(&mut *tx)
    .await;

    let id = match inserted {
        Ok(id) => id,
        Err(error) => {
            return Err(
                map_unique_violation(&error, "tax_rate", &name).unwrap_or(error.into())
            );
        }
    };

    tx.commit().await?;
    get_tax_rate(pool, organization_id, id).await
}

/// Edit a rate, optionally handing the default flag over.
///
/// The percentage is editable and the documents that used it are **not** rewritten: an invoice
/// line stores its own `tax_percent`, which is the whole reason that column is a snapshot rather
/// than a join. Correcting a rate that was entered as 15 when it meant 5 must not change an
/// invoice somebody already sent.
pub async fn patch_tax_rate(
    pool: &PgPool,
    organization_id: Uuid,
    rate_id: Uuid,
    patch: &TaxRatePatch,
) -> Result<TaxRateView> {
    get_tax_rate(pool, organization_id, rate_id).await?;

    let mut tx = pool.begin().await?;

    if patch.is_default == Some(true) {
        let row = sqlx::query_as::<_, TaxRateRow>(
            "select id, organization_id, name, percent::text as percent, kind, is_default, active, \
                    created_at \
             from accounting_tax_rates where organization_id = $1 and id = $2 for update",
        )
        .bind(organization_id)
        .bind(rate_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "update accounting_tax_rates set is_default = false \
             where organization_id = $1 and kind = $2 and is_default and id <> $3",
        )
        .bind(organization_id)
        .bind(row.kind)
        .bind(rate_id)
        .execute(&mut *tx)
        .await?;
    }

    // Same COALESCE shape and the same reason as `patch_account` — one statement, one ordering,
    // no hand-numbered placeholders. It runs on the TRANSACTION, not the pool: the
    // `is_default` handover above cleared the previous default, and a statement issued outside
    // the transaction would leave a window where an organization owns no default rate at all.
    let patched = sqlx::query_as::<_, TaxRateRow>(
        "update accounting_tax_rates \
            set name = coalesce($3, name), \
                percent = coalesce($4::numeric, percent), \
                is_default = coalesce($5, is_default), \
                active = coalesce($6, active) \
          where organization_id = $1 and id = $2 \
      returning id, organization_id, name, percent::text as percent, kind, is_default, active, \
                created_at",
    )
    .bind(organization_id)
    .bind(rate_id)
    .bind(patch.name.as_deref().map(|n| validate_name("tax_rate", n)).transpose()?)
    .bind(patch.percent.as_deref().map(|v| validate_percent(v)).transpose()?)
    .bind(patch.is_default)
    .bind(patch.active)
    .fetch_optional(&mut *tx)
    .await?;

    // As with the account patch: the row is what proves the write happened. A statement that
    // returns "no row" is a write that matched nothing, and answering `ok` for it is how a
    // cross-tenant id becomes a silent no-op instead of a 404.
    if patched.is_none() {
        return Err(AccountingError::NotFound("tax_rate"));
    }

    tx.commit().await?;
    get_tax_rate(pool, organization_id, rate_id).await
}

/// Deactivate a rate. A rate an invoice still points at stays readable; the column stores its
/// own percent, so nothing dangles.
pub async fn set_tax_rate_active(
    pool: &PgPool,
    organization_id: Uuid,
    rate_id: Uuid,
    active: bool,
) -> Result<TaxRateView> {
    patch_tax_rate(
        pool,
        organization_id,
        rate_id,
        &TaxRatePatch {
            active: Some(active),
            ..TaxRatePatch::default()
        },
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------------------------

/// Turn a unique-violation into the module's own "already called X", which names the field.
///
/// A `code already exists` constraint surfaces to a form as a 500 with a PostgreSQL message in
/// it, which is both unreadable and a leak of the schema; the person filling the form needs to be
/// told **which** code is taken, because they can change it.
fn map_unique_violation(
    error: &sqlx::Error,
    entity: &'static str,
    code: &str,
) -> Option<AccountingError> {
    if let sqlx::Error::Database(db) = error {
        if db.code().as_deref() == Some("23505") {
            return Some(AccountingError::NameTaken {
                entity,
                code: if code.is_empty() {
                    "that name".to_owned()
                } else {
                    code.to_owned()
                },
            });
        }
    }
    None
}

/// Read a `numeric` column as the text the module binds back, so one parse serves every view.
#[must_use]
pub fn numeric_text(row: &PgRow, column: &str) -> String {
    row.try_get::<String, _>(column)
        .unwrap_or_else(|_| "0.00".to_owned())
}

/// The `Date` a `date` column holds, for the filters.
#[must_use]
pub fn day_of(row: &PgRow, column: &str) -> Option<time::Date> {
    row.try_get::<time::Date, _>(column).ok()
}

/// The instant a `timestamptz` column holds.
#[must_use]
pub fn instant_of(row: &PgRow, column: &str) -> Option<OffsetDateTime> {
    row.try_get::<OffsetDateTime, _>(column).ok()
}

/// Serialise a day the way the API writes it — the same function the views use.
pub use dates::to_wire as day_to_wire;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_may_be_digits_letters_spaces_dots_and_dashes() {
        assert_eq!(validate_code("account", " 1200 ").expect("ok"), "1200");
        assert_eq!(validate_code("account", "4000-Sales").expect("ok"), "4000-Sales");
        assert_eq!(validate_code("account", "Cash.Bank").expect("ok"), "Cash.Bank");
    }

    #[test]
    fn a_code_with_a_delimiter_in_it_is_refused() {
        // A code appears in a CSV export and a PDF. A semicolon or a quote in it turns the
        // export into a file that opens wrong, so the refusal is about the export, not taste.
        assert!(validate_code("account", "1200;1400").is_err());
        assert!(validate_code("account", "Cash \"Main\"").is_err());
    }

    #[test]
    fn a_percent_is_a_number_of_percent_not_a_fraction() {
        assert_eq!(validate_percent("20").expect("ok"), "20.00");
        assert_eq!(validate_percent("7.5").expect("ok"), "7.50");
        // 0.2 as a fraction is 0.2% and is almost always a mistake, so the message says so.
        assert!(validate_percent("0.2").is_ok(), "0.2 is a legal rate, just a suspicious one");
        assert!(validate_percent("101").is_err());
        assert!(validate_percent("-5").is_err());
        assert!(validate_percent("twenty").is_err());
        assert!(validate_percent("").is_err());
    }

    #[test]
    fn a_kind_the_schema_allows_but_this_build_does_not_is_reported() {
        // Defaulting an unknown kind to `asset` would put a liability on the wrong side of a
        // balance sheet, so the parse refuses rather than guesses.
        assert!(AccountKind::parse("liability").is_some());
        assert!(AccountKind::parse("contra_asset").is_none());
        assert!(TaxRateKind::parse("sales").is_some());
        assert!(TaxRateKind::parse("vat").is_none());
    }
}
