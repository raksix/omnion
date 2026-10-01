//! The database half of OAuth applications: registration, editing, secret rotation and the
//! authorization-code ledger (REQ-033, slice 3b).
//!
//! Everything here is behind the `store` feature for the reason the rest of this crate is: the
//! rules that decide *whether a credential is honoured* are in [`crate::oauth`] and
//! [`crate::model_oauth`], where they can be tested with no database at all, and this module is
//! only the part that has to speak SQL.
//!
//! # Three properties this module is responsible for
//!
//! **1. The secret is minted, hashed and returned in one statement, or not at all.** There is
//! no read path here that can produce a second plaintext: every function that returns one also
//! *writes* the hash it verified against, which is what makes [`MintedApp`] unrepeatable.
//!
//! **2. The rotation's overlap window is written in the same `update` that changes the hash.**
//! Migration `0229` constrains `previous_secret_hash` and `previous_secret_expires_at` to be
//! null *together*, so a partial write is refused by the database rather than by this code
//! being careful — and the verification path reads both columns or neither.
//!
//! **3. Deleting an app is a status change, not a `delete`.** The row keeps its foreign keys
//! because the authorization codes and the audit trail reference it, and "show me what this
//! app did last March" must keep working after somebody retires it. That is the same reason a
//! revoked API key keeps its request log.

use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};
use crate::model_oauth::{AppEdit, AppStatus, MintedApp, NewApp, OAuthApp, app_rules};
use crate::oauth::{
    GrantType, SECRET_OVERLAP_DAYS, hash_client_secret, mint_client_id, mint_client_secret,
    verify_client_secret,
};

/// The columns every app read returns, in one place.
///
/// Written out rather than `select *` so that adding a column to the table cannot silently
/// change a response shape — and, more importantly here, so that the **secret columns are
/// absent from the read projection by construction**. There is no `select *` anywhere in this
/// module for an `oauth_apps` row, which is what makes "the list endpoint returned the hash" a
/// mistake that cannot be made rather than a mistake that must be reviewed.
const APP_COLUMNS: &str = "id, organization_id, name, description, logo_object_key, client_id, \
     redirect_uris, scopes, grant_types, status, previous_secret_expires_at, created_by, \
     created_at, updated_at";

/// The columns the token endpoint needs, which is the one read that *does* want a hash.
///
/// Separate from [`APP_COLUMNS`] and named as such, so the credential read is visible in a
/// grep for `secret_hash` rather than hidden inside a wider projection.
const CLIENT_CREDENTIAL_COLUMNS: &str = "id, organization_id, client_id, client_secret_hash, previous_secret_hash, \
     previous_secret_expires_at, redirect_uris, scopes, grant_types, status";

/// Read a `jsonb` array of strings, tolerating a non-array by returning nothing rather than
/// failing a whole list read over one bad row.
fn string_array(value: serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a stored grant list, dropping a value this build does not know rather than failing.
///
/// A row written by a *later* build with a third grant type must still be listable by this
/// one: the panel shows "authorization code, client credentials" and the developer can delete
/// the app, instead of every list request in the tenant failing on one unfamiliar string. The
/// flow is refused at the token endpoint, which is where refusing it matters.
fn grant_list(value: serde_json::Value) -> Vec<GrantType> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .filter_map(GrantType::parse)
                .collect()
        })
        .unwrap_or_default()
}

/// One `oauth_apps` row, exactly as the table stores it.
///
/// **Private, and separate from [`OAuthApp`] on purpose.** `OAuthApp` derives `Serialize` for
/// the API, so it cannot also be sqlx's decode target without every `jsonb` column being
/// decoded straight into its final type — which would turn a `grant_types` array holding one
/// value a *future* build wrote into a hard decode error for *this* one, and take down every
/// app screen in the tenant because a later release added a flow. Keeping the row type means
/// the three `jsonb` columns are read as `serde_json::Value` and interpreted by functions whose
/// tolerance is a decision rather than a derive.
///
/// It is also what makes the secret columns structurally absent from the panel's read path: this
/// struct has no `client_secret_hash` field, so `select`ing one and forgetting to drop it is a
/// compile error rather than a leak.
#[derive(sqlx::FromRow)]
struct AppRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    description: Option<String>,
    logo_object_key: Option<String>,
    client_id: String,
    redirect_uris: serde_json::Value,
    scopes: serde_json::Value,
    grant_types: serde_json::Value,
    status: String,
    previous_secret_expires_at: Option<OffsetDateTime>,
    created_by: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl AppRow {
    /// Interpret this row as the shape the API returns.
    ///
    /// `status` is parsed rather than trusted, because the one place a bad value could appear
    /// is a row written by a build whose vocabulary was wider — and answering "this app is
    /// active" for a status we do not recognise is how a retired integration keeps issuing
    /// codes.
    fn into_app(self) -> Result<OAuthApp> {
        Ok(OAuthApp {
            id: self.id,
            organization_id: self.organization_id,
            name: self.name,
            description: self.description,
            logo_object_key: self.logo_object_key,
            client_id: self.client_id,
            redirect_uris: string_array(self.redirect_uris),
            scopes: string_array(self.scopes),
            grant_types: grant_list(self.grant_types),
            status: AppStatus::parse(&self.status)?,
            previous_secret_expires_at: self.previous_secret_expires_at,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Register an app, returning the client secret exactly once.
///
/// The name and the redirect list are validated **before** the transaction opens, so a
/// malformed submission never holds a transaction open while it is refused — and, more
/// importantly, so the panel gets a field-level answer instead of a constraint name.
///
/// The client id is generated here rather than submitted: a caller that chose its own would be
/// choosing the string every authorization request resolves an app by, and a guessable one
/// (`reporting`, `app1`) makes the tenant enumerable from a login screen.
pub async fn create_app(
    pool: &PgPool,
    organization_id: Uuid,
    new_app: &NewApp,
) -> Result<MintedApp> {
    app_rules::validate_name(&new_app.name)?;
    let redirect_uris = app_rules::validate_redirect_uris(&new_app.redirect_uris)?;
    app_rules::validate_scopes(&new_app.scopes)?;
    app_rules::validate_grant_types(&new_app.grant_types)?;

    let client_id = mint_client_id();
    let minted = mint_client_secret();
    let stored_hash = hash_client_secret(&minted.plaintext);

    let grants: Vec<&str> = new_app
        .grant_types
        .iter()
        .map(|grant| grant.as_str())
        .collect();
    let mut transaction = pool.begin().await?;

    let row = sqlx::query_as::<_, AppRow>(&format!(
        "insert into oauth_apps (organization_id, name, description, logo_object_key, client_id, \
         client_secret_hash, redirect_uris, scopes, grant_types, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         returning {APP_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(new_app.name.trim())
    .bind(
        new_app
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty()),
    )
    .bind(new_app.logo_object_key.as_deref())
    .bind(&client_id)
    .bind(&stored_hash)
    .bind(serde_json::json!(redirect_uris))
    .bind(serde_json::json!(new_app.scopes))
    .bind(serde_json::json!(grants))
    .bind(new_app.created_by)
    .fetch_one(&mut *transaction)
    .await
    .map_err(map_app_write_error)?;

    transaction.commit().await?;

    Ok(MintedApp {
        app: row.into_app()?,
        plaintext: minted.plaintext,
        // No overlap on creation: there is no previous secret to keep alive. The field is
        // `None` rather than "now", because "the previous secret expires at the instant it was
        // created" is a statement that invites a reader to wonder what it was.
        previous_secret_expires_at: None,
    })
}

/// Translate the unique-index violations an app write can hit.
///
/// `23505` is `unique_violation`, and the index name says which constraint. The name clash is
/// the one worth translating — the panel puts "that name is taken" next to the name box, which
/// is the difference between a form a person can fix and a `500` with a PostgreSQL constraint
/// name in it. `oauth_apps_client_id_key` needs no translation: a client-id collision is a
/// 96-bit random draw colliding, and the honest answer for that is a `500` and a log line.
fn map_app_write_error(error: sqlx::Error) -> DeveloperError {
    if let sqlx::Error::Database(ref inner) = error
        && inner.code().as_deref() == Some("23505")
        && inner.constraint() == Some("oauth_apps_org_name_key")
    {
        return DeveloperError::AppNameTaken(String::new());
    }
    DeveloperError::Database(error)
}

/// List an organization's apps, newest first.
///
/// Deleted apps are excluded rather than returned with a status the panel would have to filter:
/// a list screen that shows retired integrations and offers an "Edit" button on them is a list
/// screen where the only safe action is "delete again".
pub async fn list_apps(pool: &PgPool, organization_id: Uuid) -> Result<Vec<OAuthApp>> {
    let rows = sqlx::query_as::<_, AppRow>(&format!(
        "select {APP_COLUMNS} from oauth_apps \
         where organization_id = $1 and status <> 'deleted' \
         order by created_at desc, id"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut apps = Vec::with_capacity(rows.len());
    for row in rows {
        apps.push(row.into_app()?);
    }
    Ok(apps)
}

/// Read one app, scoped to its organization.
///
/// The organization is part of the `where` rather than checked afterwards, so another tenant's
/// app is *not found* rather than forbidden — the same answer as an id that never existed, and
/// not a `403` that confirms it does.
pub async fn get_app(pool: &PgPool, organization_id: Uuid, app_id: Uuid) -> Result<OAuthApp> {
    let row = sqlx::query_as::<_, AppRow>(&format!(
        "select {APP_COLUMNS} from oauth_apps \
         where organization_id = $1 and id = $2 and status <> 'deleted'"
    ))
    .bind(organization_id)
    .bind(app_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::AppNotFound)?;

    row.into_app()
}

/// Apply a partial edit to an app.
///
/// One statement, built with a [`QueryBuilder`] rather than hand-numbered `$1, $2, …`, for the
/// reason the request log's filters do the same: a placeholder and its value are written side
/// by side, so a field added later cannot leave a numbering that reads the wrong column. The
/// alternative — a `select`, mutate in Rust, `update` everything — would let two concurrent
/// editors overwrite each other's field with the value the other one did not change.
///
/// Validation runs first, over exactly the fields the edit touches. An edit that does not
/// mention `redirect_uris` must not be refused because the *stored* list would fail today's
/// rule: an app registered under a rule this build has since tightened has to stay editable,
/// or the only way to fix it is to re-register and break every client holding the old id.
pub async fn edit_app(
    pool: &PgPool,
    organization_id: Uuid,
    app_id: Uuid,
    edit: &AppEdit,
    now: OffsetDateTime,
) -> Result<OAuthApp> {
    if let Some(name) = edit.name.as_deref() {
        app_rules::validate_name(name)?;
    }
    if let Some(uris) = edit.redirect_uris.as_deref() {
        app_rules::validate_redirect_uris(uris)?;
    }
    if let Some(scopes) = edit.scopes.as_deref() {
        app_rules::validate_scopes(scopes)?;
    }
    if let Some(grants) = edit.grant_types.as_deref() {
        app_rules::validate_grant_types(grants)?;
    }
    if edit
        .description
        .as_ref()
        .and_then(|text| text.as_deref())
        .is_some_and(|text| text.chars().count() > app_rules::MAX_DESCRIPTION)
    {
        return Err(DeveloperError::AppDescriptionTooLong {
            max: app_rules::MAX_DESCRIPTION,
        });
    }

    let mut builder = QueryBuilder::<Postgres>::new("update oauth_apps set updated_at = ");
    builder.push_bind(now);

    if let Some(name) = edit.name.as_deref() {
        builder.push(", name = ");
        builder.push_bind(name.trim().to_owned());
    }
    if let Some(description) = edit.description.as_ref() {
        builder.push(", description = ");
        match description {
            Some(text) => {
                builder.push_bind(Some(text.trim().to_owned()));
            }
            // `= null` rather than skipped: an omitted field means "leave it", an explicit
            // null means "clear it", and a form's "remove this description" button has to be
            // able to say so.
            None => {
                builder.push("null");
            }
        }
    }
    if let Some(logo) = edit.logo_object_key.as_ref() {
        builder.push(", logo_object_key = ");
        match logo {
            Some(key) => {
                builder.push_bind(Some(key.clone()));
            }
            None => {
                builder.push("null");
            }
        }
    }
    if let Some(uris) = edit.redirect_uris.as_deref() {
        builder.push(", redirect_uris = ");
        builder.push_bind(serde_json::json!(uris));
    }
    if let Some(scopes) = edit.scopes.as_deref() {
        builder.push(", scopes = ");
        builder.push_bind(serde_json::json!(scopes));
    }
    if let Some(grants) = edit.grant_types.as_deref() {
        let list: Vec<&str> = grants.iter().map(|grant| grant.as_str()).collect();
        builder.push(", grant_types = ");
        builder.push_bind(serde_json::json!(list));
    }
    if let Some(status) = edit.status {
        builder.push(", status = ");
        builder.push_bind(status.as_str().to_owned());
    }

    builder.push(" where organization_id = ");
    builder.push_bind(organization_id);
    builder.push(" and id = ");
    builder.push_bind(app_id);
    builder.push(" and status <> 'deleted' returning ");
    // The projection is appended as a literal, not a bind: it is a compile-time constant in
    // this file, so a bind would only turn a fixed string into a parameter the server has to
    // re-plan.
    builder.push(APP_COLUMNS);

    builder
        .build_query_as::<AppRow>()
        .fetch_optional(pool)
        .await?
        .ok_or(DeveloperError::AppNotFound)?
        .into_app()
}

/// Rotate a client secret, keeping the old one valid until the overlap expires.
///
/// This is the one place in the platform where two credentials for one row are live at the same
/// time, and the migration is written so that state is *expressible but not half-writable*:
/// `previous_secret_hash` and `previous_secret_expires_at` are constrained to be null together.
///
/// Two decisions worth stating:
///
/// * **The old hash moves into the overlap slot rather than being overwritten.** The alternative
///   — keeping both hashes in one column and comparing against either — makes "which one
///   authenticated this request" unanswerable from the row, and the request log's whole value
///   is that an operator can tell an old-deployment request from a new one.
/// * **The overlap is [`SECRET_OVERLAP_DAYS`], not a request parameter.** A caller who can pick
///   the window can pick it forever; the number belongs in the code and in the row, where it can
///   be audited.
pub async fn rotate_secret(
    pool: &PgPool,
    organization_id: Uuid,
    app_id: Uuid,
    now: OffsetDateTime,
) -> Result<MintedApp> {
    let minted = mint_client_secret();
    let stored_hash = hash_client_secret(&minted.plaintext);
    let overlap_until = now + time::Duration::days(SECRET_OVERLAP_DAYS);

    let mut transaction = pool.begin().await?;

    // The current hash is moved into the overlap slot *by this statement*, so the two columns
    // are written together and cannot disagree. The read of `client_secret_hash` and the write
    // of `previous_secret_hash` are the same expression, which is the whole trick: there is no
    // window in which the old secret is in neither slot.
    let row = sqlx::query_as::<_, AppRow>(&format!(
        "update oauth_apps set \
             client_secret_hash = $3, \
             previous_secret_hash = client_secret_hash, \
             previous_secret_expires_at = $4, \
             updated_at = $5 \
         where organization_id = $1 and id = $2 and status <> 'deleted' \
         returning {APP_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(app_id)
    .bind(&stored_hash)
    .bind(overlap_until)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(DeveloperError::AppNotFound)?;

    transaction.commit().await?;

    Ok(MintedApp {
        app: row.into_app()?,
        plaintext: minted.plaintext,
        previous_secret_expires_at: Some(overlap_until),
    })
}

/// Withdraw an app.
///
/// A **status change and a `deleted_at` stamp in one statement**, never a `delete`: the row is
/// referenced by authorization codes, by the audit trail and by any token a client is still
/// presenting, and a cascade would take the evidence with it. The row stops appearing in the
/// list immediately, and the authorization endpoint refuses it with `AppNotActive`.
///
/// Idempotent: withdrawing twice is the same end state, not an error.
pub async fn delete_app(
    pool: &PgPool,
    organization_id: Uuid,
    app_id: Uuid,
    now: OffsetDateTime,
) -> Result<OAuthApp> {
    let row = sqlx::query_as::<_, AppRow>(&format!(
        "update oauth_apps set status = 'deleted', deleted_at = coalesce(deleted_at, $3), \
         updated_at = $3 \
         where organization_id = $1 and id = $2 \
         returning {APP_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(app_id)
    .bind(now)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::AppNotFound)?;

    row.into_app()
}

/// Look an app up by its public `client_id`, for the authorization endpoint.
///
/// **The one read here that is not organization-scoped**, because a browser reaches the
/// authorization endpoint before anybody has said who they are. It therefore returns the
/// tenant's id alongside the row, and every caller must use *that* organization for the
/// consent decision — resolving the tenant from the app row rather than from the session is
/// what stops a request for tenant A's app from writing a code into tenant B's ledger.
pub async fn find_by_client_id(pool: &PgPool, client_id: &str) -> Result<Option<OAuthApp>> {
    let row = sqlx::query_as::<_, AppRow>(&format!(
        "select {APP_COLUMNS} from oauth_apps where client_id = $1"
    ))
    .bind(client_id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(row.into_app()?)),
        None => Ok(None),
    }
}

/// The tenant an app belongs to, resolved from the app's own row.
///
/// Separate from [`find_by_client_id`] because the two callers need different things: the
/// authorization endpoint needs the whole row (to render consent) *and* the tenant, while the
/// token endpoint needs only the credential material and the tenant. One projection per caller
/// is what keeps the wider projection — which names every redirect URI — off the token path.
pub async fn owning_organization(pool: &PgPool, app_id: Uuid) -> Result<Uuid> {
    let organization_id: Uuid =
        sqlx::query_scalar("select organization_id from oauth_apps where id = $1")
            .bind(app_id)
            .fetch_optional(pool)
            .await?
            .ok_or(DeveloperError::AppNotFound)?;
    Ok(organization_id)
}

/// The credential material the token endpoint verifies against, plus the tenant it belongs to.
///
/// Returns `(app_id, organization_id, current_hash, previous_hash, previous_expiry)`. The
/// previous pair is `None` unless both columns are present — and the migration guarantees they
/// are present together, so the `None` case means "no open overlap", not "a broken row".
pub async fn client_credentials(
    pool: &PgPool,
    client_id: &str,
) -> Result<Option<ClientCredentialsRow>> {
    let row = sqlx::query(&format!(
        "select {CLIENT_CREDENTIAL_COLUMNS} from oauth_apps where client_id = $1"
    ))
    .bind(client_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let previous_hash: Option<String> = row.try_get("previous_secret_hash")?;
    let previous_expires_at: Option<OffsetDateTime> = row.try_get("previous_secret_expires_at")?;

    Ok(Some(ClientCredentialsRow {
        app_id: row.try_get("id")?,
        organization_id: row.try_get("organization_id")?,
        client_secret_hash: row.try_get("client_secret_hash")?,
        // The pair is taken together rather than independently, and that is the point: a row
        // with a hash but no expiry would otherwise authenticate an old secret for ever, which
        // is precisely the half-overlap the migration refuses to store.
        previous_secret_hash: previous_hash.zip(previous_expires_at),
    }))
}

/// What the token endpoint needs to authenticate a client.
#[derive(Debug, Clone)]
pub struct ClientCredentialsRow {
    /// Which app.
    pub app_id: Uuid,
    /// Its tenant. The caller must scope everything it writes to this, never to the session's
    /// organization: the two are equal for a first-party client and different for a machine one.
    pub organization_id: Uuid,
    /// The current secret's hash.
    pub client_secret_hash: String,
    /// The previous secret's hash and the instant it stops working — `None` unless a rotation's
    /// overlap is open *and* unexpired.
    pub previous_secret_hash: Option<(String, OffsetDateTime)>,
}

/// Whether a presented secret matches this row, and which one it was.
///
/// The overlap is honoured only **while it is open**, so the pair is filtered by the clock
/// before it is compared: a rotated secret presented on the eighth day fails, exactly as the
/// UI said it would. Constant-time comparisons come from [`verify_client_secret`]; this function
/// returns which slot matched so the audit entry can distinguish an old-deployment request from
/// a new one, which is the only reason the overlap exists.
#[must_use]
pub fn which_secret_matched(
    row: &ClientCredentialsRow,
    presented: &str,
    now: OffsetDateTime,
) -> Option<&'static str> {
    if verify_client_secret(presented, &row.client_secret_hash) {
        return Some("current");
    }
    if let Some((hash, expires_at)) = row.previous_secret_hash.as_ref()
        && *expires_at > now
        && verify_client_secret(presented, hash)
    {
        return Some("previous");
    }
    None
}

/// A code being redeemed, resolved from its hash.
///
/// The code itself is never stored: `oauth_authorization_codes.code_hash` is the primary key and
/// is what a lookup uses, so a dump of the table hands out no usable credential. This struct is
/// the *result* of resolving one, and it carries the app and user a code was minted for — which
/// is what the token endpoint then uses instead of anything the caller submitted.
#[derive(Debug, Clone)]
pub struct RedeemedCode {
    /// The app the code was issued to.
    pub app_id: Uuid,
    /// The user who consented.
    pub user_id: Uuid,
    /// The redirect the code is bound to.
    pub redirect_uri: String,
    /// The scopes consented to.
    pub scopes: Vec<String>,
    /// The PKCE challenge **and its method, as one pair** — not two optional fields that could
    /// disagree. Half a PKCE pair is a challenge that cannot be verified, and the token
    /// endpoint must not be able to see one without the other; migration `0231`'s
    /// `oauth_codes_challenge_is_whole` says the same in the database, and a type that could
    /// hold half a pair would say the opposite in the read path.
    pub code_challenge: Option<(String, String)>,
}

/// Write an authorization code, returning the plaintext **once**.
///
/// Single use by construction: the row is inserted with its own hash as the key and no
/// `used_at`, so a code that is never redeemed is still a live credential until it expires —
/// which is why [`CODE_TTL_MINUTES`] is ten and not an hour.
pub async fn issue_code(
    pool: &PgPool,
    code_hash: &str,
    app_id: Uuid,
    user_id: Uuid,
    redirect_uri: &str,
    scopes: &[String],
    code_challenge: Option<&str>,
    code_challenge_method: Option<&str>,
    expires_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "insert into oauth_authorization_codes \
         (code_hash, app_id, user_id, redirect_uri, scopes, code_challenge, \
          code_challenge_method, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(code_hash)
    .bind(app_id)
    .bind(user_id)
    .bind(redirect_uri)
    .bind(serde_json::json!(scopes))
    .bind(code_challenge)
    .bind(code_challenge_method)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Redeem a code: resolve it **and mark it spent in the same transaction**.
///
/// The `used_at is null` predicate is inside the `update`, not in a read that precedes it.
/// That is the single most important line in this function: a read-then-update lets two
/// simultaneous redemptions both see `used_at is null` and both issue a token, which turns a
/// one-time code into a two-time code and is invisible in every test that redeems it once.
pub async fn redeem_code(
    pool: &PgPool,
    code_hash: &str,
    now: OffsetDateTime,
) -> Result<RedeemedCode> {
    let mut transaction = pool.begin().await?;

    let row = sqlx::query(
        "update oauth_authorization_codes set used_at = $2 \
         where code_hash = $1 and used_at is null and expires_at > $2 \
         returning app_id, user_id, redirect_uri, scopes, code_challenge, code_challenge_method",
    )
    .bind(code_hash)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?
    // One answer for unknown, already-spent and expired: a caller probing codes learns nothing
    // from which of the three it hit, and an expired code is not distinguishable from one that
    // was never issued.
    .ok_or(DeveloperError::InvalidCode)?;

    let redeemed = RedeemedCode {
        app_id: row.try_get("app_id")?,
        user_id: row.try_get("user_id")?,
        redirect_uri: row.try_get("redirect_uri")?,
        scopes: string_array(row.try_get::<serde_json::Value, _>("scopes")?),
        // `zip`, not two independent reads: a row where one column is null and the other is
        // not arrives here as `None` for the pair, which the token endpoint refuses — the
        // same degradation the database constraint prevents in the first place.
        code_challenge: row
            .try_get::<Option<String>, _>("code_challenge")?
            .zip(row.try_get::<Option<String>, _>("code_challenge_method")?),
    };

    transaction.commit().await?;
    Ok(redeemed)
}

/// Remove codes that are spent or long expired.
///
/// Two predicates, not one, because the two kinds of row have different reasons to go and a
/// single `expires_at < now` would leave every redeemed code of the last ten minutes in the
/// table forever — the ledger of a busy installation grows without bound and the retention
/// worker's range scan walks more dead rows each time.
///
/// Returns how many rows went, so the caller can log the sweep rather than run it silently.
pub async fn purge_codes(pool: &PgPool, before: OffsetDateTime) -> Result<u64> {
    let removed = sqlx::query(
        "delete from oauth_authorization_codes \
         where expires_at < $1 or (used_at is not null and used_at < $1)",
    )
    .bind(before)
    .execute(pool)
    .await?;
    Ok(removed.rows_affected())
}

/// How many live codes an app is holding, for the app's detail screen.
///
/// "Live" means issued and not yet expired, spent or not: an app with forty rows in the table
/// and none redeemable is not holding forty codes, and a panel that says it is would send an
/// operator hunting for a compromise that never happened.
pub async fn live_code_count(pool: &PgPool, app_id: Uuid, now: OffsetDateTime) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from oauth_authorization_codes \
         where app_id = $1 and used_at is null and expires_at > $2",
    )
    .bind(app_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// Write an access token, returning the plaintext **once**.
///
/// The scopes are written as the grant's, never re-read from the app: see the migration's
/// comment for why that denormalisation is the safe direction. `user_id` is `Some` only for a
/// token that stands for a person, and the table's `oauth_tokens_provenance_is_whole`
/// constraint refuses the half-pairs, so a caller that passes the wrong one here gets a
/// constraint violation rather than a token that misattributes the app's traffic.
pub async fn issue_token(
    pool: &PgPool,
    token_hash: &str,
    app_id: Uuid,
    user_id: Option<Uuid>,
    grant_type: &str,
    scopes: &[String],
    expires_at: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "insert into oauth_access_tokens \
         (token_hash, app_id, user_id, grant_type, scopes, expires_at) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(token_hash)
    .bind(app_id)
    .bind(user_id)
    .bind(grant_type)
    .bind(serde_json::json!(scopes))
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A token row resolved from its hash, and whether it is still usable at a given instant.
///
/// The clock is applied **in the query** rather than read back and compared, for the same reason
/// [`redeem_code`] spends a code inside its own `where`: a read-then-check invites two
/// simultaneous requests to both see a live token. Here the token is not single-use so the race
/// is benign, but the *answer* would still differ between the two callers if expiry were applied
/// in Rust, and an answer that depends on when the read happened is an answer worth not having.
pub async fn find_token(
    pool: &PgPool,
    token_hash: &str,
    now: OffsetDateTime,
) -> Result<Option<TokenRow>> {
    let row = sqlx::query(
        "select token_hash, app_id, user_id, grant_type, scopes, expires_at, revoked_at \
         from oauth_access_tokens \
         where token_hash = $1 and revoked_at is null and expires_at > $2",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(TokenRow {
        app_id: row.try_get("app_id")?,
        user_id: row.try_get("user_id")?,
        grant_type: row.try_get("grant_type")?,
        scopes: string_array(row.try_get("scopes")?),
        expires_at: row.try_get("expires_at")?,
    }))
}

/// What a resolved token grants.
#[derive(Debug, Clone)]
pub struct TokenRow {
    /// Which app it belongs to.
    pub app_id: Uuid,
    /// The person it stands for, or `None` for a machine token.
    pub user_id: Option<Uuid>,
    /// The grant that produced it.
    pub grant_type: String,
    /// What it may do, as fixed at issue time.
    pub scopes: Vec<String>,
    /// When it stops working.
    pub expires_at: OffsetDateTime,
}

/// Revoke a token, returning whether a live one was revoked.
///
/// `None` when it was already revoked or already expired, and the caller answers the same thing
/// either way: a client presenting a revoked token must not learn whether it was revoked
/// deliberately or ran out, because that is one bit about a credential's history.
pub async fn revoke_token(pool: &PgPool, token_hash: &str) -> Result<bool> {
    let updated = sqlx::query(
        "update oauth_access_tokens set revoked_at = now() \
         where token_hash = $1 and revoked_at is null",
    )
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Remove tokens that are revoked-and-old or long expired.
///
/// Two predicates for the same reason [`purge_codes`] has two: a token deliberately revoked
/// months ago and a token nobody used have different reasons to go, and one predicate would
/// either keep revoked rows for ever or delete live ones. `revoked_at` is also what an incident
/// review wants, so the retention question is a policy decision rather than a sweep's guess —
/// which is why the caller passes the cutoff.
pub async fn purge_tokens(pool: &PgPool, before: OffsetDateTime) -> Result<u64> {
    let removed = sqlx::query(
        "delete from oauth_access_tokens \
         where expires_at < $1 or (revoked_at is not null and revoked_at < $1)",
    )
    .bind(before)
    .execute(pool)
    .await?;
    Ok(removed.rows_affected())
}

/// How many live tokens an app is holding, for the app's detail screen beside the code count.
///
/// Same definition as [`live_code_count`] and the same reason: rows in the table are not tokens
/// in use, and a panel that reported the row count would send an operator hunting for a
/// compromise that never happened.
pub async fn live_token_count(pool: &PgPool, app_id: Uuid, now: OffsetDateTime) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from oauth_access_tokens \
         where app_id = $1 and revoked_at is null and expires_at > $2",
    )
    .bind(app_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_presented_secret_matches_the_current_slot_and_says_so() {
        let minted = mint_client_secret();
        let row = ClientCredentialsRow {
            app_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            client_secret_hash: hash_client_secret(&minted.plaintext),
            previous_secret_hash: None,
        };
        assert_eq!(
            which_secret_matched(&row, &minted.plaintext, datetime!(2026-10-01 12:00 UTC)),
            Some("current")
        );
        assert_eq!(
            which_secret_matched(&row, "wrong", datetime!(2026-10-01 12:00 UTC)),
            None
        );
    }

    #[test]
    fn a_previous_secret_is_honoured_only_inside_its_overlap_and_the_slot_is_reported() {
        // The rotation behaviour the request asks for, stated as the two boundaries that matter
        // rather than as prose: the second before expiry works, and the same secret after it
        // does not. Reporting *which* slot matched is what lets the audit entry distinguish a
        // deployment that has not yet redeployed from one that has.
        let old = mint_client_secret();
        let new = mint_client_secret();
        let expiry = datetime!(2026-10-08 12:00 UTC);
        let row = ClientCredentialsRow {
            app_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            client_secret_hash: hash_client_secret(&new.plaintext),
            previous_secret_hash: Some((hash_client_secret(&old.plaintext), expiry)),
        };

        assert_eq!(
            which_secret_matched(&row, &new.plaintext, datetime!(2026-10-01 12:00 UTC)),
            Some("current")
        );
        assert_eq!(
            which_secret_matched(&row, &old.plaintext, datetime!(2026-10-01 12:00 UTC)),
            Some("previous")
        );
        // The instant of expiry is the boundary: at it, the secret is gone.
        assert_eq!(which_secret_matched(&row, &old.plaintext, expiry), None);
        assert_eq!(
            which_secret_matched(&row, &old.plaintext, expiry - time::Duration::seconds(1)),
            Some("previous")
        );
    }

    #[test]
    fn a_previous_secret_is_never_consulted_when_there_is_no_open_overlap() {
        let minted = mint_client_secret();
        let row = ClientCredentialsRow {
            app_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            client_secret_hash: "omnion-oauth-secret.v1$not-a-real-hash-but-well-formed".to_owned(),
            previous_secret_hash: Some((
                hash_client_secret(&minted.plaintext),
                datetime!(2020-01-01 00:00 UTC),
            )),
        };
        assert_eq!(
            which_secret_matched(&row, &minted.plaintext, datetime!(2026-10-01 12:00 UTC)),
            None,
            "an overlap that expired years ago must authenticate nobody"
        );
    }

    #[test]
    fn a_row_whose_hash_this_build_cannot_read_authenticates_nobody() {
        // A row written by another scheme — an API key hash, an Argon2 password hash, an empty
        // column — must fail rather than accidentally comparing equal. `verify_client_secret`
        // owns that check; the assertion here is that the *store's* entry point routes through
        // it rather than doing its own `==`.
        let row = ClientCredentialsRow {
            app_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            client_secret_hash: "$argon2id$v=19$m=1$aaaa$bbbb".to_owned(),
            previous_secret_hash: None,
        };
        assert_eq!(
            which_secret_matched(
                &row,
                "$argon2id$v=19$m=1$aaaa$bbbb",
                datetime!(2026-10-01 12:00 UTC)
            ),
            None
        );
    }

    #[test]
    fn an_unknown_grant_type_in_a_stored_list_is_dropped_rather_than_failing_the_row() {
        // A row written by a later build with a third grant must still be listable by this one.
        // The flow is refused at the token endpoint, which is where refusing it matters; a list
        // endpoint that fails would take down every app screen in the tenant.
        let stored = serde_json::json!(["authorization_code", "device_code", "client_credentials"]);
        assert_eq!(
            grant_list(stored),
            vec![GrantType::AuthorizationCode, GrantType::ClientCredentials]
        );
        // A malformed column — not an array at all — is the same shape of tolerance.
        assert!(grant_list(serde_json::json!("authorization_code")).is_empty());
    }
}
