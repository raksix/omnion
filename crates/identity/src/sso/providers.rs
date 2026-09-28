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

/// The provider kinds the platform speaks.
///
/// A directory is a kind here rather than a separate concept on purpose: the registry screen,
/// the sign-in screen and the enable gate are all "a provider", and giving LDAP its own
/// administration surface would mean building that surface twice. What *does* differ is where
/// the identity comes from and how it is proved, which is [`crate::sso::directory`]'s job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenID Connect — discovery + signed ID token.
    Oidc,
    /// Generic OAuth2 — no ID token; identity comes from the userinfo endpoint.
    Oauth2,
    /// SAML 2.0 — a posted, signed assertion.
    Saml,
    /// LDAP — a live directory query (REQ-065).
    Ldap,
    /// Active Directory — LDAP plus UPN matching and the account-disabled flag (REQ-065).
    ActiveDirectory,
}

impl ProviderKind {
    /// The string the database stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::Oauth2 => "oauth2",
            Self::Saml => "saml",
            Self::Ldap => "ldap",
            Self::ActiveDirectory => "active_directory",
        }
    }

    /// Parse the stored string, refusing an unknown kind rather than guessing one.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "oidc" => Ok(Self::Oidc),
            "oauth2" => Ok(Self::Oauth2),
            "saml" => Ok(Self::Saml),
            "ldap" => Ok(Self::Ldap),
            "active_directory" => Ok(Self::ActiveDirectory),
            other => Err(IdentityError::InvalidProvider(format!(
                "unknown provider kind `{other}`"
            ))),
        }
    }

    /// Whether this kind is a directory — a live connection rather than a protocol round trip.
    ///
    /// The registry screen and the sign-in flow branch on this in exactly one place each, and
    /// a new kind that is neither is a bug in the branch rather than in the kind.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Ldap | Self::ActiveDirectory)
    }

    /// Whether this kind requests OAuth scopes at an authorization endpoint. A directory has
    /// none: the search filter is the only question asked, and a `scopes` column full of
    /// directory values would be a lie the sign-in path cannot act on.
    #[must_use]
    pub const fn uses_scopes(self) -> bool {
        !self.is_directory()
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
    /// When the registry screen last ran a connection test, and whether it passed.
    ///
    /// `last_test_ok: None` is a **third** state — never tested — and the enable gate below
    /// depends on it. Collapsing it into "false" would make a freshly created provider look
    /// broken; collapsing it into "true" would let one be switched on untested.
    pub last_test_at: Option<OffsetDateTime>,
    /// Whether the last test passed. `None` when there has never been one.
    pub last_test_ok: Option<bool>,
    /// How often this provider syncs, in minutes. Zero means "not on a schedule".
    pub sync_interval_minutes: i32,
    /// When it last synced, and how that went.
    pub last_sync_at: Option<OffsetDateTime>,
    /// `ok`, `partial` or `failed`.
    pub last_sync_status: Option<String>,
    /// The plugin declaration that produced this row, when it was not a platform kind.
    pub plugin_key: Option<String>,
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
    /// How often to sync, in minutes. `None` keeps the default of 60.
    pub sync_interval_minutes: Option<i32>,
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
    /// New sync interval, in minutes. `Some(0)` is "not on a schedule".
    pub sync_interval_minutes: Option<i32>,
}

/// Column list of every provider query.
const COLUMNS: &str = "id, organization_id, slug, kind, name, config, secret_ref, scopes, \
     group_claim, default_role_id, jit_enabled, enabled, last_test_at, last_test_ok, \
     sync_interval_minutes, last_sync_at, last_sync_status, plugin_key, created_at, updated_at";

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
        // SAML asks for nothing, and a directory asks a *filter* rather than a scope. Giving
        // either a list would write values into a column the sign-in path cannot act on.
        ProviderKind::Saml | ProviderKind::Ldap | ProviderKind::ActiveDirectory => &[],
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
    // Checked here rather than by the database so the panel gets a field-level answer: a `check`
    // violation arrives as a constraint name and nothing else.
    validate_sync_interval(new.sync_interval_minutes)?;

    let row = sqlx::query_as::<_, AuthProvider>(&format!(
        "insert into auth_providers (organization_id, slug, kind, name, config, secret_ref, \
             scopes, group_claim, default_role_id, jit_enabled, enabled, sync_interval_minutes) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, coalesce($12, 60)) \
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
    .bind(new.sync_interval_minutes)
    .fetch_optional(pool)
    .await?;

    row.ok_or_else(|| IdentityError::InvalidProvider("the provider could not be created".into()))
}

/// A sync interval outside this range would either hammer somebody else's directory or never
/// run at all. Zero is a real choice — a provider used only interactively — so it is inside.
fn validate_sync_interval(minutes: Option<i32>) -> Result<()> {
    match minutes {
        Some(value) if !(0..=10_080).contains(&value) => Err(IdentityError::InvalidProvider(
            "sync_interval_minutes must be between 0 (never on a schedule) and 10080 (a week)"
                .into(),
        )),
        _ => Ok(()),
    }
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
    validate_sync_interval(changes.sync_interval_minutes)?;
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
             sync_interval_minutes = coalesce($10, sync_interval_minutes), \
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
    .bind(changes.sync_interval_minutes)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Record the outcome of a connection test on the provider row.
///
/// The stored answer is what the enable gate reads. Storing it — rather than re-running the test
/// inside `POST /enable` — is a deliberate choice: an enable that has to reach somebody else's
/// server turns a checkbox into a network call, and it fails for reasons that have nothing to do
/// with the operator holding the button. It also means the registry's `Last test` column is
/// real, instead of being recomputed and re-lying on every page load.
pub async fn record_test(pool: &PgPool, id: Uuid, passed: bool) -> Result<()> {
    sqlx::query("update auth_providers set last_test_at = now(), last_test_ok = $2 where id = $1")
        .bind(id)
        .bind(passed)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a provider may be switched on.
///
/// The gate is the requirement, not a nicety: *"a provider cannot be enabled while its last test
/// has never passed"*. `None` — never tested — is refused alongside a failure, because the
/// difference between them is only interesting to the database, never to the person who just
/// clicked Enable.
///
/// An already-enabled provider is never re-refused by this gate: turning something off is
/// always allowed, and turning it back on is a deliberate act. But an *edit* that changes the
/// connection does invalidate the stored test, and [`invalidate_test`] is how the API says so —
/// a provider whose host was repointed must not keep a green "Last test" from the old host.
#[must_use]
pub fn enable_gate(provider: &AuthProvider) -> Result<()> {
    if provider.enabled {
        return Ok(());
    }
    match provider.last_test_ok {
        Some(true) => Ok(()),
        Some(false) => Err(IdentityError::InvalidProvider(
            "this provider's last connection test failed — fix what it reported and test it again \
             before switching it on"
                .into(),
        )),
        None => Err(IdentityError::InvalidProvider(
            "this provider has never passed a connection test, so it cannot be switched on yet"
                .into(),
        )),
    }
}

/// Forget a stored test result after a change that would make it untrue.
///
/// A test proves *a configuration*, not a provider id. Editing the host, the base DN, the bind
/// DN or the secret reference all change the answer, and keeping the old green would make the
/// registry claim a connection works when nothing has been asked of the new one. Renaming a
/// provider does not — which is why this takes the specific fields rather than "anything was
/// touched".
pub async fn invalidate_test(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("update auth_providers set last_test_ok = null where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a change invalidates the stored test result.
#[must_use]
pub fn edit_invalidates_test(changed: &[&str]) -> bool {
    changed
        .iter()
        .any(|field| matches!(*field, "config" | "secret_ref" | "kind"))
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
