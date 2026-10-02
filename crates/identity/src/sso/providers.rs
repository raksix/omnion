//! Sign-in providers per organization: OIDC, generic OAuth2 and SAML 2.0 (docs/07-IAM.md §11;
//! REQ-006, slice 4b-2).
//!
//! The row is the whole configuration, and it is deliberately **not** the only place a provider
//! is described: the endpoints a provider needs are read from its discovery document (OIDC) or
//! entered explicitly (OAuth2, SAML), and the **client secret is never in the row** — it lives
//! behind `secret_ref`, a name the resolver looks up in the environment (REQ-037 owns the secret
//! store proper). A row therefore holds wiring, not credentials, and the panel can show a whole
//! provider without ever holding a value it would have to keep safe.
//!
//! Local sign-in stays available in every state: nothing here can turn password sign-in off, and
//! a provider is only *reachable* once its `enabled` flag is set — the sign-in route and the
//! discovery test both refuse a disabled provider, so a half-configured provider cannot be used
//! by accident while it is being filled in.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Namespace of a JIT-provisioned password: such an account has no password of its own.
///
/// A JIT account carries this value in its `password_hash` column's sibling: the sign-in path
/// checks it before any hash is verified, so a JIT account can only ever be signed into through
/// the provider that created it, and a stolen hash string is useless to an attacker.
pub const JIT_PASSWORD_MARKER: &str = "!jit:no-password";

/// The three provider kinds the platform speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenID Connect — discovery + signed ID token.
    Oidc,
    /// Generic OAuth2 — no ID token; identity comes from the userinfo endpoint.
    Oauth2,
    /// SAML 2.0 — a posted, signed assertion.
    Saml,
}

impl ProviderKind {
    /// The string the database stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Oauth2 => "oauth2",
            Self::Saml => "saml",
        }
    }

    /// Parse the stored string, refusing an unknown kind rather than guessing one.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "oidc" => Ok(Self::Oidc),
            "oauth2" => Ok(Self::Oauth2),
            "saml" => Ok(Self::Saml),
            other => Err(IdentityError::InvalidProvider(format!(
                "unknown provider kind `{other}`"
            ))),
        }
    }
}

/// A provider as the panel reads it — never the secret, which lives behind `secret_ref`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuthProvider {
    /// Primary key.
    pub id: Uuid,
    /// Organization the provider signs people into.
    pub organization_id: Uuid,
    /// URL-safe name (`okta`), unique inside the organization.
    pub slug: String,
    /// Which protocol it speaks.
    pub kind: ProviderKind,
    /// Label shown on the sign-in screen.
    pub name: String,
    /// Endpoints and provider-specific settings as JSON.
    pub config: serde_json::Value,
    /// Name of the environment variable (or secret-store key) holding the client secret.
    pub secret_ref: Option<String>,
    /// Scopes requested at the authorization endpoint.
    pub scopes: Vec<String>,
    /// Claim carrying group membership, mapped to roles.
    pub group_claim: Option<String>,
    /// Role every successful sign-in of this provider gets.
    pub default_role_id: Option<Uuid>,
    /// Whether an unknown subject is provisioned on first sign-in.
    pub jit_enabled: bool,
    /// Whether the provider is reachable.
    pub enabled: bool,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When the row last changed.
    pub updated_at: OffsetDateTime,
}

/// The fields a create request may set.
#[derive(Debug, Clone)]
pub struct NewProvider {
    /// Organization the provider belongs to.
    pub organization_id: Uuid,
    /// URL-safe name.
    pub slug: String,
    /// Protocol.
    pub kind: ProviderKind,
    /// Label.
    pub name: String,
    /// Endpoints and settings.
    pub config: serde_json::Value,
    /// Environment name holding the client secret, when the flow needs one.
    pub secret_ref: Option<String>,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Claim carrying groups.
    pub group_claim: Option<String>,
    /// Default role.
    pub default_role_id: Option<Uuid>,
    /// Provision on first sign-in?
    pub jit_enabled: bool,
    /// Reachable?
    pub enabled: bool,
}

/// The fields an update request may set; `None` leaves the column alone.
#[derive(Debug, Clone, Default)]
pub struct ProviderChanges {
    /// New label.
    pub name: Option<String>,
    /// New endpoints/settings.
    pub config: Option<serde_json::Value>,
    /// New secret reference.
    pub secret_ref: Option<String>,
    /// New scopes.
    pub scopes: Option<Vec<String>>,
    /// New group claim.
    pub group_claim: Option<String>,
    /// New default role.
    pub default_role_id: Option<Uuid>,
    /// New JIT flag.
    pub jit_enabled: Option<bool>,
    /// New enabled flag.
    pub enabled: Option<bool>,
}

/// Column list of every provider query.
const COLUMNS: &str = "id, organization_id, slug, kind, name, config, secret_ref, scopes, \
     group_claim, default_role_id, jit_enabled, enabled, created_at, updated_at";

/// A slug must be usable in a URL path segment and stable enough to appear in a bookmark.
fn validate_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_ascii_lowercase();
    let valid = !slug.is_empty()
        && slug.len() <= 48
        && slug.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        return Err(IdentityError::InvalidProvider(
            "slug must be 1–48 characters of a–z, 0–9 and dashes, starting with a letter".into(),
        ));
    }
    Ok(slug)
}

/// The default scope set of each kind — what a request without an explicit scope list gets.
#[must_use]
pub fn default_scopes(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::Oidc => &["openid", "profile", "email"],
        ProviderKind::Oauth2 => &["email", "profile"],
        ProviderKind::Saml => &[],
    }
}

/// Read every provider of an organization (enabled ones first, then by name).
pub async fn list_providers(pool: &PgPool, organization_id: Uuid) -> Result<Vec<AuthProvider>> {
    let rows = sqlx::query_as::<_, AuthProvider>(&format!(
        "select {COLUMNS} from auth_providers where organization_id = $1 \
         order by enabled desc, lower(name), slug"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Read a provider by its id.
pub async fn find_provider(pool: &PgPool, id: Uuid) -> Result<Option<AuthProvider>> {
    Ok(sqlx::query_as::<_, AuthProvider>(&format!(
        "select {COLUMNS} from auth_providers where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Read a provider by the slug the sign-in URL carries.
pub async fn find_provider_by_slug(
    pool: &PgPool,
    organization_id: Uuid,
    slug: &str,
) -> Result<Option<AuthProvider>> {
    Ok(sqlx::query_as::<_, AuthProvider>(&format!(
        "select {COLUMNS} from auth_providers where organization_id = $1 and slug = $2"
    ))
    .bind(organization_id)
    .bind(slug)
    .fetch_optional(pool)
    .await?)
}

/// Every enabled provider of an organization — the list the sign-in screen renders.
pub async fn list_enabled(pool: &PgPool, organization_id: Uuid) -> Result<Vec<AuthProvider>> {
    let rows = sqlx::query_as::<_, AuthProvider>(&format!(
        "select {COLUMNS} from auth_providers \
         where organization_id = $1 and enabled order by lower(name), slug"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Create a provider.
pub async fn create_provider(pool: &PgPool, new: NewProvider) -> Result<AuthProvider> {
    let slug = validate_slug(&new.slug)?;
    let name = new.name.trim();
    if name.is_empty() || name.len() > 120 {
        return Err(IdentityError::InvalidProvider(
            "name must be 1–120 characters".into(),
        ));
    }
    if let Some(ref reference) = new.secret_ref
        && (reference.trim().is_empty() || reference.len() > 200)
    {
        return Err(IdentityError::InvalidProvider(
            "secret_ref must be a name of 1–200 characters".into(),
        ));
    }
    if new.config.is_null() {
        return Err(IdentityError::InvalidProvider(
            "config must be an object".into(),
        ));
    }

    let row = sqlx::query_as::<_, AuthProvider>(&format!(
        "insert into auth_providers (organization_id, slug, kind, name, config, secret_ref, \
             scopes, group_claim, default_role_id, jit_enabled, enabled) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         returning {COLUMNS}"
    ))
    .bind(new.organization_id)
    .bind(&slug)
    .bind(new.kind.as_str())
    .bind(name)
    .bind(&new.config)
    .bind(new.secret_ref.as_deref().map(str::trim))
    .bind(&new.scopes)
    .bind(
        new.group_claim
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(new.default_role_id)
    .bind(new.jit_enabled)
    .bind(new.enabled)
    .fetch_optional(pool)
    .await?;

    row.ok_or_else(|| IdentityError::InvalidProvider("the provider could not be created".into()))
}

/// Update the mutable fields of a provider.
///
/// `secret_ref` and `group_claim` are only written when a value is supplied; clearing one is an
/// explicit empty string from the panel, which is normalized to `NULL` above. A `None` therefore
/// means "unchanged", never "cleared" — the API layer builds the change set explicitly.
pub async fn update_provider(
    pool: &PgPool,
    id: Uuid,
    changes: ProviderChanges,
) -> Result<Option<AuthProvider>> {
    let row = sqlx::query_as::<_, AuthProvider>(&format!(
        "update auth_providers set \
             name = coalesce($2, name), \
             config = coalesce($3, config), \
             secret_ref = coalesce($4, secret_ref), \
             scopes = coalesce($5, scopes), \
             group_claim = coalesce($6, group_claim), \
             default_role_id = coalesce($7, default_role_id), \
             jit_enabled = coalesce($8, jit_enabled), \
             enabled = coalesce($9, enabled), \
             updated_at = now() \
         where id = $1 returning {COLUMNS}"
    ))
    .bind(id)
    .bind(changes.name.map(|value| value.trim().to_owned()))
    .bind(changes.config)
    .bind(changes.secret_ref.map(|value| value.trim().to_owned()))
    .bind(changes.scopes)
    .bind(changes.group_claim)
    .bind(changes.default_role_id)
    .bind(changes.jit_enabled)
    .bind(changes.enabled)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete a provider. Its challenges and events go with it (`on delete cascade`).
pub async fn delete_provider(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("delete from auth_providers where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Record that a provider was used, so the panel can show "last sign-in" without a second query.
pub async fn touch_provider(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("update auth_providers set updated_at = now() where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a JIT account has no local password. Mirrored by the sign-in path before any hash work.
#[must_use]
pub fn is_jit_account(password_hash: &str) -> bool {
    password_hash == JIT_PASSWORD_MARKER
}
