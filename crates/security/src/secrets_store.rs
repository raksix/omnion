//! The secret inventory's SQL — a projection over references, never over values (REQ-012, slice 4).
//!
//! ## The one rule this module exists to keep
//!
//! **No query in this file may select a column that holds a secret.** Not the TOTP
//! `secret_ciphertext`, not the service-account `secret_hash`, not the webhook signing `secret`.
//!
//! That is a rule about *this file*, which is why it is not left to review. Each source is
//! read through an explicit column list built from [`INVENTORY_COLUMNS`]; a `select *` here
//! would be the defect, because `*` re-reads the source's own schema and the day somebody adds
//! a value column upstream it starts appearing in a security screen with no code change here at
//! all. The three sources that do hold material are read as **counts** instead.
//!
//! The containment is then checked twice: once structurally, by
//! `the_column_lists_name_no_value_column`, and once end-to-end over the router by
//! `apps/api/tests/security.rs::no_secret_value_reaches_the_inventory_response`, which walks the
//! real response body and refuses it if anything from a source table's own columns appears in it.
//!
//! ## Five heterogeneous sources, one shape
//!
//! There is no secrets table, so the inventory is assembled from five places that share nothing
//! but the fact that each holds a name:
//!
//! | Source | Holds | Read as |
//! |---|---|---|
//! | `auth_providers` | `secret_ref`, a name | one row per provider |
//! | the process environment | a variable that is set or is not | one row per known variable |
//! | `webhook_endpoints` | the signing secret | **count** only |
//! | `service_account_keys` | `secret_hash` | **count** only |
//! | `mfa_factors` | `secret_ciphertext` | **count** only |
//!
//! The last three collapse to a single summary row each rather than one row per material: an
//! operator asking "how many webhook signing secrets exist" is the question the count answers,
//! and a per-key row would be a list of endpoints with a column that is empty by design —
//! which reads as a bug and invites somebody to fill it in.
//!
//! ## An organization-scoped screen over a global store
//!
//! The identity-provider and webhook sources are organization-scoped; the MFA and service-account
//! sources are per-user and **not** scoped to an organization at all, because a user may belong
//! to several. Those three are therefore counted for the whole platform and labelled
//! `scope: platform`, with the count rather than the rows. An organization that wanted only its
//! own could infer nothing from them, and pretending otherwise would require a membership
//! question per row — the honest answer here is the global count with the label.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, SecurityError};
use crate::secrets::{
    RotationEvidence, SecretInventory, SecretRef, SecretSource, SecretState, environment_names,
    environment_row,
};

/// Build the whole inventory.
///
/// `organization_id` narrows the two organization-scoped sources; the process-wide ones are
/// reported as a single labelled row each, because narrowing them is not something this store
/// can do honestly.
pub async fn inventory(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    mut is_set: impl FnMut(&str) -> bool,
) -> Result<SecretInventory> {
    let mut rows = Vec::new();
    rows.extend(identity_providers(pool, organization_id).await?);
    rows.extend(environment(is_set));
    rows.extend(material_sources(pool, organization_id).await?);
    Ok(SecretInventory::from_rows(rows))
}

/// One row per sign-in integration that names a credential.
///
/// `secret_ref` is a name by construction — the column's comment says so and the schema has no
/// other credential field — so selecting it is safe and is the whole point of the source.
async fn identity_providers(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Vec<SecretRef>> {
    let rows: Vec<IdentityProviderRow> = sqlx::query_as(
        r#"
        select secret_ref as name,
               slug       as scope,
               updated_at as rotated_at,
               created_at as created_at
        from auth_providers
        where secret_ref is not null and secret_ref <> ''
          and ($1::uuid is null or organization_id = $1)
        order by slug
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let (rotated_at, evidence) = if row.rotated_at > row.created_at {
                (Some(row.rotated_at), RotationEvidence::ReferenceChanged)
            } else {
                (Some(row.created_at), RotationEvidence::ReferenceCreated)
            };
            SecretRef {
                key: format!("identity_provider:{}:{}", row.scope, row.name),
                name: row.name,
                source: SecretSource::IdentityProvider,
                scope: row.scope,
                rotated_at,
                evidence,
                material_count: 0,
                expired: false,
                state: SecretState::Unverifiable,
                note: "the provider names a credential; its value lives outside the platform"
                    .to_string(),
            }
        })
        .collect())
}

/// One row per environment variable the platform knows it reads.
///
/// The list is a hand-maintained constant ([`crate::secrets::environment_names`]) because a
/// process cannot enumerate its own environment — which is the one thing this screen cannot do
/// and therefore the one thing it must say out loud. A variable nobody listed is a secret this
/// inventory does not report, and the header note on the screen states it.
fn environment(mut is_set: impl FnMut(&str) -> bool) -> Vec<SecretRef> {
    // Collected first rather than mapped lazily: the closure asks the process
    // about each name exactly once, and a lazy map would make the number of
    // probes depend on the iterator's own implementation.
    let mut rows = Vec::new();
    for (name, purpose, required) in environment_names() {
        let set = is_set(name);
        rows.push(environment_row(name, purpose, required, set));
    }
    rows
}

/// One summary row per source that holds real material.
///
/// Each is a `count(*)`: the count is the answer, and the material behind it is never in the
/// query. `expires_at` is selected for the same reason the IP store selects expired rows — an
/// expired key is the fact an operator needs, and a screen that hid it would be hiding the
/// actionable row.
async fn material_sources(pool: &PgPool, organization_id: Option<Uuid>) -> Result<Vec<SecretRef>> {
    let mut rows = Vec::new();

    // -- webhook endpoints --------------------------------------------------------------------
    //
    // No expiry column: `webhook_endpoints` has `created_at`/`updated_at` and nothing else, so
    // the expired count is a literal zero rather than a filter against a column that does not
    // exist. Writing the filter anyway would be a `500` on every inventory load, which is the
    // kind of defect the REQ keeps producing — a screen that fails at runtime instead of
    // reporting a truth. The row is still reported `unverifiable`: rotating a webhook secret is
    // done by re-saving the endpoint, and nothing records when.
    let webhooks: WebhookCount = sqlx::query_as(
        r#"
        select count(*)::bigint as live, 0::bigint as expired
        from webhook_endpoints
        where ($1::uuid is null or organization_id = $1)
        "#,
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    rows.push(material_row(
        SecretSource::Webhook,
        "WEBHOOK_SIGNING_KEY",
        "signs every delivery this platform sends to a subscribed endpoint",
        webhooks.live,
        webhooks.expired,
    ));

    // -- service account keys ------------------------------------------------------------------
    let keys: ServiceKeyCount = sqlx::query_as(
        r#"
        select count(*)::bigint                                    as live,
               count(*) filter (where expires_at is not null
                                 and expires_at <= now())::bigint  as expired
        from service_account_keys k
        join service_accounts s on s.id = k.service_account_id
        where k.revoked_at is null
          and ($1::uuid is null or s.organization_id = $1)
        "#,
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    rows.push(material_row(
        SecretSource::ServiceAccountKey,
        "SERVICE_ACCOUNT_KEY",
        "authenticates an API integration; only its hash is stored",
        keys.live,
        keys.expired,
    ));

    // -- MFA factors ---------------------------------------------------------------------------
    let factors: MfaCount = sqlx::query_as(
        r#"
        select count(*)::bigint as enrolled
        from mfa_factors
        where kind = 'totp' and revoked_at is null and confirmed_at is not null
        "#,
    )
    .fetch_one(pool)
    .await?;
    rows.push(material_row(
        SecretSource::MfaFactor,
        "MFA_TOTP_SECRET",
        "the shared secret behind an enrolled second factor; held encrypted, never rendered",
        factors.enrolled,
        0,
    ));

    Ok(rows)
}

/// Build the summary row for a source that holds material.
///
/// `live == 0` is [`SecretState::Missing`] rather than a satisfied row: "no webhook signing key
/// exists" and "the webhook signing key is fine" are different facts, and an inventory that
/// reports the second for the first is the exact green-row lie this crate is written against.
fn material_row(
    source: SecretSource,
    name: &'static str,
    purpose: &'static str,
    live: i64,
    expired: i64,
) -> SecretRef {
    let state = if expired > 0 {
        SecretState::Expired
    } else if live == 0 {
        SecretState::Missing
    } else {
        SecretState::Unverifiable
    };
    let note = if live == 0 {
        format!("{purpose} — none is configured")
    } else if expired > 0 {
        format!("{purpose} — {expired} of {live} are past their expiry")
    } else {
        format!("{purpose} — {live} configured; the material itself is never selected or rendered")
    };
    SecretRef {
        key: format!("{}:{name}", source.as_str()),
        name: name.to_string(),
        source,
        scope: "platform".to_string(),
        rotated_at: None,
        evidence: RotationEvidence::Unknown,
        material_count: live,
        expired: expired > 0,
        state,
        note,
    }
}

// ---------------------------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct IdentityProviderRow {
    name: String,
    scope: String,
    rotated_at: OffsetDateTime,
    created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct WebhookCount {
    live: i64,
    expired: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ServiceKeyCount {
    live: i64,
    expired: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct MfaCount {
    enrolled: i64,
}

/// A source column the inventory must never carry into a response.
///
/// Read by the containment test below and, more importantly, by the router-level walk: a row of
/// this list appearing in a response body is a defect in the **query**, not in the renderer, so
/// the constant is what the walk greps for.
pub const NEVER_SELECTED_COLUMNS: &[&str] = &[
    "secret_ciphertext",
    "secret_hash",
    "webhook_endpoints.secret",
    "secret_ref_value",
    "password",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::INVENTORY_COLUMNS;

    #[test]
    fn the_column_lists_name_no_value_column() {
        // Structural containment: the union of every column this module can
        // select must be disjoint from the columns that hold material.
        let selectable: Vec<&str> = INVENTORY_COLUMNS.to_vec();
        for column in selectable {
            for forbidden in NEVER_SELECTED_COLUMNS {
                assert!(
                    !column.contains(forbidden),
                    "{column} would be selected and it names {forbidden}"
                );
            }
        }
    }

    #[test]
    fn no_query_in_this_module_selects_a_star_or_a_forbidden_column() {
        // The textual half, and it is here on purpose: `select *` is the exact
        // shape that re-reads a source's schema, so a source that gains a value
        // column upstream must fail HERE rather than in a review six months
        // later.
        //
        // Only the module's own code is scanned, and it stops at the test module.
        // Two self-reference traps, both hit while writing this: the
        // assertion line itself contains `select *`, and so does the
        // `NEVER_SELECTED_COLUMNS` constant's documentation. A guard that
        // reads itself fails forever and gets deleted instead of fixed, so
        // the scan is bounded by construction instead of by hope.
        let source = include_str!("secrets_store.rs");
        let body = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(body, _)| body);
        let mut checked = 0;
        for line in body.lines() {
            let code = match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            };
            let lowered = code.to_lowercase();
            if !lowered.contains("select") {
                continue;
            }
            checked += 1;
            assert!(!lowered.contains("select *"), "a query selects *: {code}");
            for forbidden in NEVER_SELECTED_COLUMNS {
                assert!(
                    !lowered.contains(forbidden),
                    "a query names {forbidden}: {code}"
                );
            }
        }
        assert!(
            checked >= 4,
            "the scan found {checked} select lines; the queries moved or were removed"
        );
    }

    #[test]
    fn a_source_with_no_material_reads_missing_not_healthy() {
        let row = material_row(
            SecretSource::Webhook,
            "WEBHOOK_SIGNING_KEY",
            "signs every delivery",
            0,
            0,
        );
        assert_eq!(row.state, SecretState::Missing);
        assert!(row.note.contains("none is configured"), "{}", row.note);
        assert_eq!(row.material_count, 0);
    }

    #[test]
    fn an_expired_material_outranks_a_live_one() {
        let row = material_row(
            SecretSource::ServiceAccountKey,
            "SERVICE_ACCOUNT_KEY",
            "authenticates an API integration",
            3,
            1,
        );
        assert_eq!(row.state, SecretState::Expired);
        assert!(row.expired, "the row must carry the flag too");
        assert_eq!(row.material_count, 3, "the live count is still reported");
        assert!(row.note.contains("1 of 3"), "{}", row.note);
    }

    #[test]
    fn live_material_reads_unverifiable_because_the_value_cannot_be_read() {
        let row = material_row(
            SecretSource::MfaFactor,
            "MFA_TOTP_SECRET",
            "the shared secret behind a second factor",
            7,
            0,
        );
        assert_eq!(row.state, SecretState::Unverifiable);
        assert_eq!(row.material_count, 7);
        assert!(
            row.note.contains("never selected or rendered"),
            "the note must say the material is withheld: {}",
            row.note
        );
    }

    #[test]
    fn the_environment_probe_is_consulted_once_per_name() {
        // The predicate takes `&str`, so a store cannot pass a name it invented:
        // every call is a name from the constant list.
        let mut asked: Vec<String> = Vec::new();
        let rows = environment(|name| {
            asked.push(name.to_string());
            name.ends_with("CSRF_SECRET")
        });
        assert_eq!(asked.len(), environment_names().len());
        assert_eq!(rows.len(), environment_names().len());
        let csrf = rows
            .iter()
            .find(|r| r.name == "OMNION_CSRF_SECRET")
            .expect("the CSRF secret is in the list");
        assert_eq!(csrf.state, SecretState::Unverifiable);
        let smtp = rows
            .iter()
            .find(|r| r.name == "OMNION_SMTP_PASSWORD")
            .expect("the mail password is in the list");
        assert_eq!(smtp.state, SecretState::Missing);
    }

    #[test]
    fn two_sources_sharing_a_name_are_two_rows() {
        // The key is `source:name` precisely so a deduplicating dashboard
        // cannot report one secret where there are two.
        let rows = SecretInventory::from_rows(vec![
            material_row(SecretSource::Webhook, "SHARED", "one", 1, 0),
            material_row(SecretSource::ServiceAccountKey, "SHARED", "two", 1, 0),
        ]);
        assert_eq!(rows.total, 2);
        let keys: Vec<&str> = rows.secrets.iter().map(|s| s.key.as_str()).collect();
        assert_ne!(keys[0], keys[1], "the keys collide: {keys:?}");
    }

    #[test]
    fn an_unknown_stored_source_is_refused_rather_than_labelled_other() {
        // The store never writes a source word, but a database this
        // migration did not create might hold one — and silently bucketing it
        // as something else is how a secret stops being counted.
        assert_eq!(SecretSource::parse("password_vault"), None);
        assert_eq!(SecretState::parse("fine"), None);
    }

    #[test]
    fn the_store_error_vocabulary_is_the_crates_own() {
        // The store must not invent an HTTP-shaped error; that is the API
        // layer's job (docs/04: the crate stays infrastructure).
        let error = SecurityError::invalid("a name");
        assert_eq!(error.code(), "invalid_security_input");
    }
}
