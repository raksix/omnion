//! Where an account came from, and what a provider deletion would take with it (REQ-065, slice 4
//! part 9; migration `0127`).
//!
//! `0127` adds three columns to `users` — `identity_source`,
//! `provisioned_by_provider_id`, `external_id` — that the request's own data model names and that
//! did not exist. Everything here is the half of the story that runs *after* the migration: the
//! reads, the writes, and the guard the migration alone cannot be.
//!
//! # Why this is not a `SELECT` on a foreign key
//!
//! The foreign key is declared `on delete set null`, which is the safe direction and also the
//! dangerous one. Deleting a provider whose accounts are still provisioned would quietly turn
//! every one of them into a `local` account, and nothing would say so: the accounts keep their
//! sessions, keep their role grants, and stop having any record of which directory vouched for
//! them. The request's criterion is "deleting a provider that provisioned users is blocked with
//! the affected count listed", so the guard is a **count read first, a delete second** — and the
//! count is not the row count of the provider, it is the number of *people* whose next sign-in
//! would change.
//!
//! # Why the count is two numbers
//!
//! [`Impact`] carries a *total* and a *by source* breakdown, and both are pinned by tests,
//! because either alone is a plausible wrong answer:
//!
//! * The **total** is what the panel shows on the button and what the refusal names. It is the
//!   number of accounts that would lose their provider.
//! * The **by-source** breakdown is what makes the total readable. "7 accounts" is a number to
//!   be waved through; "7 accounts: 5 LDAP, 2 SCIM" tells an operator that a directory sweep
//!   created these people and that removing the provider will not remove them. Collapsing them
//!   into one field loses exactly the information that makes the guard worth having.
//!
//! SCIM-provisioned accounts are the case the breakdown exists for. A connector names no
//! provider, so `0127` deliberately leaves those rows with **both** halves null: attributing
//! them to the provider that happens to be configured would be inventing provenance. They are
//! still counted — as `scim` — because a SCIM token can push accounts without the provider row
//! existing at all, and a guard that said "0 accounts" there would be a lie in the direction
//! that matters.
//!
//! # Reassignment
//!
//! [`reassign`] is the other half of the criterion: "after reassignment the users fall back to
//! local accounts". It is one transaction that clears all three columns together, because the
//! pairing check refuses a half-written provenance and a batch that cleared two of three would
//! fail on the first row and leave the rest untouched — an operator would see a partial batch and
//! no list of which rows moved.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// The systems that may own an account.
///
/// The vocabulary is closed by `0127`'s constraint; this mirrors it so a value that reached the
/// store cannot be one the database would have refused anyway. `Local` is the only kind that
/// carries no provider, which is what makes it the state reassignment falls back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum IdentitySource {
    /// A person who signed up with a password here.
    Local,
    /// LDAP directory.
    Ldap,
    /// Active Directory.
    ActiveDirectory,
    /// OpenID Connect.
    Oidc,
    /// Generic OAuth2.
    Oauth2,
    /// SAML 2.0.
    Saml,
    /// Pushed by a SCIM connector.
    Scim,
}

impl IdentitySource {
    /// The string the database stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ldap => "ldap",
            Self::ActiveDirectory => "active_directory",
            Self::Oidc => "oidc",
            Self::Oauth2 => "oauth2",
            Self::Saml => "saml",
            Self::Scim => "scim",
        }
    }

    /// Read a stored value, or `None` for one this build does not know.
    ///
    /// `None` here is **not** an error. A migration may legitimately introduce a source this
    /// binary has never heard of, and a read that failed would take the provider list down for
    /// every tenant because one account row mentions a word this build predates. The value is
    /// reported as unknown and the count still includes the row.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "local" => Self::Local,
            "ldap" => Self::Ldap,
            "active_directory" => Self::ActiveDirectory,
            "oidc" => Self::Oidc,
            "oauth2" => Self::Oauth2,
            "saml" => Self::Saml,
            "scim" => Self::Scim,
            _ => return None,
        })
    }
}

/// One account's provenance, as read back from the row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Provenance {
    /// The account.
    pub user_id: Uuid,
    /// The organization the account belongs to.
    pub organization_id: Option<Uuid>,
    /// Address of the account (used by the panel to name a person).
    pub email: String,
    /// Which system owns it.
    pub identity_source: String,
    /// The provider that created or first claimed it; `None` for a local or unattributable row.
    pub provisioned_by_provider_id: Option<Uuid>,
    /// What the directory calls it.
    pub external_id: Option<String>,
}

impl Provenance {
    /// The parsed source, or `None` for a value this build does not know.
    #[must_use]
    pub fn source(&self) -> Option<IdentitySource> {
        IdentitySource::parse(&self.identity_source)
    }

    /// Whether deleting a provider would change this account's next sign-in.
    ///
    /// Deliberately **not** "is the provider id set". A SCIM-provisioned row has no provider id
    /// and is still counted, because a connector's push is the reason the account exists and the
    /// operator is being asked about the connector. Anything whose source is not `local` counts.
    #[must_use]
    pub fn is_provisioned(&self) -> bool {
        self.identity_source != IdentitySource::Local.as_str()
    }
}

/// What deleting a provider would do, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Impact {
    /// Accounts that would lose their provider link.
    pub total: i64,
    /// The same number grouped by the system that owns each account.
    ///
    /// Ordered as a vector rather than a map so the panel renders a stable order and a webhook
    /// subscriber gets a byte-identical payload for the same state.
    pub by_source: Vec<SourceCount>,
    /// Whether a source string in the table is not one this build knows.
    pub unknown_sources: bool,
}

/// How many accounts one kind of system owns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SourceCount {
    /// The stored value, verbatim.
    pub source: String,
    /// How many accounts carry it.
    pub count: i64,
}

impl Impact {
    /// A refusal message naming the count and the systems behind it.
    ///
    /// The wording is the point. "Cannot delete" is a dead end; a sentence that says *how many*
    /// and *who they are* is the information an operator needs to decide between reassigning and
    /// keeping the provider.
    #[must_use]
    pub fn refusal(&self) -> String {
        // The verb agrees with the number and appears **once**. "3 accounts are were
        // provisioned" is what a template with both a pluralised noun and a baked-in verb
        // produces, and it is the kind of defect that ships because nobody reads a refusal twice.
        let noun = if self.total == 1 {
            "1 account was".to_owned()
        } else {
            format!("{} accounts were", self.total)
        };
        let mut message = format!("{noun} provisioned by this provider");
        if !self.by_source.is_empty() {
            let breakdown: Vec<String> = self
                .by_source
                .iter()
                .map(|entry| format!("{} {}", entry.count, entry.source))
                .collect();
            message.push_str(&format!(" ({})", breakdown.join(", ")));
        }
        message.push_str(". Reassign these accounts to local first, or disable the provider instead.");
        message
    }
}

/// Count the accounts a provider deletion would affect.
///
/// Read-only and organization-scoped: a provider id is unique on its own, but the query joins
/// through `organization_id` anyway so that a bug in a caller's scoping produces a *smaller*
/// count rather than another tenant's people.
pub async fn deletion_impact(pool: &PgPool, provider_id: Uuid) -> Result<Impact> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select u.identity_source, count(*) \
           from users u \
          where u.organization_id = (select organization_id from auth_providers where id = $1) \
            and u.identity_source <> 'local' \
            and (u.provisioned_by_provider_id = $1 or u.identity_source = 'scim') \
          group by u.identity_source \
          order by u.identity_source",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?;

    let mut impact = Impact::default();
    for (source, count) in rows {
        if IdentitySource::parse(&source).is_none() {
            impact.unknown_sources = true;
        }
        impact.total += count;
        impact.by_source.push(SourceCount { source, count });
    }
    Ok(impact)
}

/// List the accounts a provider deletion would affect, newest first.
///
/// Bounded, because this feeds a panel table and a confirmation dialog: a directory with forty
/// thousand accounts cannot be rendered, and a dialog that lists forty thousand rows is a dialog
/// nobody reads. The count is the truth; the list is a sample of it.
pub async fn affected_accounts(
    pool: &PgPool,
    provider_id: Uuid,
    limit: i64,
) -> Result<Vec<Provenance>> {
    sqlx::query_as::<_, Provenance>(
        "select u.id as user_id, u.organization_id, u.email, u.identity_source, \
                u.provisioned_by_provider_id, u.external_id \
           from users u \
          where u.organization_id = (select organization_id from auth_providers where id = $1) \
            and u.identity_source <> 'local' \
            and (u.provisioned_by_provider_id = $1 or u.identity_source = 'scim') \
          order by u.created_at desc, u.email \
          limit $2",
    )
    .bind(provider_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await
    .map_err(IdentityError::from)
}

/// List the accounts an organization owns, with their provenance, for the panel.
pub async fn list_for_organization(
    pool: &PgPool,
    organization_id: Uuid,
    only_provisioned: bool,
    limit: i64,
) -> Result<Vec<Provenance>> {
    sqlx::query_as::<_, Provenance>(
        "select u.id as user_id, u.organization_id, u.email, u.identity_source, \
                u.provisioned_by_provider_id, u.external_id \
           from users u \
          where u.organization_id = $1 \
            and ($2 is false or u.identity_source <> 'local') \
          order by u.email \
          limit $3",
    )
    .bind(organization_id)
    .bind(only_provisioned)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await
    .map_err(IdentityError::from)
}

/// Fall accounts back to local: the state a provider deletion would otherwise reach by accident.
pub async fn reassign(pool: &PgPool, user_ids: &[Uuid]) -> Result<u64> {
    if user_ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "update users \
            set identity_source = 'local', provisioned_by_provider_id = null, external_id = null \
          where id = any($1)",
    )
    .bind(user_ids)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Point an account at the provider that owns it.
///
/// Refuses a half-written pair by name rather than letting the pairing check raise a constraint
/// error: the caller is a connector, and "the directory id arrived without a provider" is a
/// configuration mistake it can be told about, while a 23514 from a background sweep is a line
/// in a log nobody reads.
pub async fn attribute(
    pool: &PgPool,
    user_id: Uuid,
    source: IdentitySource,
    provider_id: Option<Uuid>,
    external_id: Option<&str>,
) -> Result<()> {
    if provider_id.is_none() != external_id.is_none() {
        return Err(IdentityError::InvalidProvider(
            "an external id and the provider it belongs to are set together — a directory id \
             with no provider is an account this platform cannot attribute"
                .into(),
        ));
    }
    if provider_id.is_none() && source != IdentitySource::Scim && source != IdentitySource::Local {
        return Err(IdentityError::InvalidProvider(
            "this account claims a directory source but names no provider, so nothing would say \
             which directory provisioned it"
                .into(),
        ));
    }

    let result = sqlx::query(
        "update users \
            set identity_source = $2, provisioned_by_provider_id = $3, external_id = $4 \
          where id = $1",
    )
    .bind(user_id)
    .bind(source.as_str())
    .bind(provider_id)
    .bind(external_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(IdentityError::InvalidUser(
            "no such account to attribute".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_account_is_never_provisioned() {
        let row = Provenance {
            user_id: Uuid::nil(),
            organization_id: None,
            email: "a@b.test".into(),
            identity_source: "local".into(),
            provisioned_by_provider_id: None,
            external_id: None,
        };
        assert!(!row.is_provisioned());
    }

    #[test]
    fn a_scim_account_counts_with_no_provider_id_at_all() {
        // The case the whole breakdown exists for: `0127` deliberately leaves a pushed row
        // unattributed, and a guard that keyed on the provider id would report 0 accounts for a
        // directory that just created eight of them.
        let row = Provenance {
            user_id: Uuid::nil(),
            organization_id: None,
            email: "pushed@b.test".into(),
            identity_source: "scim".into(),
            provisioned_by_provider_id: None,
            external_id: None,
        };
        assert!(row.is_provisioned());
    }

    #[test]
    fn an_unknown_source_still_counts_rather_than_being_dropped() {
        // A value this build predates must not silently reduce the total: a guard that undercounts
        // is worse than one that overcounts, because the operator believes the delete is safe.
        let row = Provenance {
            user_id: Uuid::nil(),
            organization_id: None,
            email: "future@b.test".into(),
            identity_source: "passkey_only".into(),
            provisioned_by_provider_id: None,
            external_id: None,
        };
        assert!(row.is_provisioned());
        assert_eq!(row.source(), None);
    }

    #[test]
    fn the_refusal_names_the_count_and_the_systems() {
        let impact = Impact {
            total: 7,
            by_source: vec![
                SourceCount { source: "ldap".into(), count: 5 },
                SourceCount { source: "scim".into(), count: 2 },
            ],
            unknown_sources: false,
        };
        let message = impact.refusal();
        assert!(message.contains('7'), "{message}");
        assert!(message.contains("5 ldap"), "{message}");
        assert!(message.contains("2 scim"), "{message}");
        // The sentence has to say what to do, or the dialog is a dead end.
        assert!(message.contains("Reassign"), "{message}");
    }

    #[test]
    fn a_single_account_reads_as_one_and_not_as_ones() {
        let impact = Impact {
            total: 1,
            by_source: vec![SourceCount { source: "oidc".into(), count: 1 }],
            unknown_sources: false,
        };
        let message = impact.refusal();
        assert!(message.contains("1 account was "), "{message}");
        assert!(message.contains("1 oidc"), "{message}");
        assert!(!message.contains("1 accounts"), "{message}");
    }

    #[test]
    fn an_empty_impact_does_not_produce_a_singular_sentence() {
        // The guard is asked the question even when the answer is zero, so the wording has to
        // survive it rather than reading "0 account is were provisioned".
        let message = Impact::default().refusal();
        assert!(message.contains("0 accounts were"), "{message}");
    }

    #[test]
    fn an_empty_breaklist_still_produces_a_sentence() {
        let impact = Impact { total: 3, ..Impact::default() };
        let message = impact.refusal();
        assert!(!message.contains("()"), "{message}");
        assert!(message.contains("3 accounts were"), "{message}");
    }

    #[test]
    fn every_stored_source_round_trips() {
        for source in [
            IdentitySource::Local,
            IdentitySource::Ldap,
            IdentitySource::ActiveDirectory,
            IdentitySource::Oidc,
            IdentitySource::Oauth2,
            IdentitySource::Saml,
            IdentitySource::Scim,
        ] {
            assert_eq!(IdentitySource::parse(source.as_str()), Some(source));
        }
    }

    #[test]
    fn a_value_outside_the_vocabulary_is_none_rather_than_a_guess() {
        // Guessing here would write a wrong `identity_source` back on the next write, and the
        // panel would show a source the database never held.
        assert_eq!(IdentitySource::parse("magic"), None);
        assert_eq!(IdentitySource::parse(""), None);
        assert_eq!(IdentitySource::parse("OIDC"), None);
    }
}
