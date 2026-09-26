//! `/api/v1/organizations` and `/api/v1/sites` — the tenancy surface.
//!
//! v0 of the tenant model (docs/01-VISION.md §10, docs/07-IAM.md §7): organizations own sites,
//! sites own the domains that address them. Every route carries a permission guard; this module
//! adds the second half of the rule — an account with a primary organization may only touch its
//! own organization and the sites inside it, while a platform-level account (no primary
//! organization) may work across tenants and is the only one that may open a new one.
//!
//! Every state change writes an audit row in the same request.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_audit::NewAuditEntry;
use omnion_events::{NewEvent, bus};
use omnion_identity::organizations::{self, NewOrganization, Organization, OrganizationChanges};
use omnion_identity::sites::{self, NewSite, Site, SiteChanges, SiteDomain};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::client_ip::ClientAddress;
use crate::error::ApiError;
use crate::scope::{ensure_same_organization, platform_only, resolve_organization};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

/// Public representation of an organization.
#[derive(Debug, Serialize)]
pub struct OrganizationBody {
    /// Organization id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Stable handle, unique platform-wide.
    pub slug: String,
    /// `active`, `suspended` or `archived`.
    pub status: String,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&Organization> for OrganizationBody {
    fn from(organization: &Organization) -> Self {
        Self {
            id: organization.id,
            name: organization.name.clone(),
            slug: organization.slug.clone(),
            status: organization.status.clone(),
            created_at: organization.created_at,
            updated_at: organization.updated_at,
        }
    }
}

/// Response body of the organization endpoints.
#[derive(Debug, Serialize)]
pub struct OrganizationsResponse {
    /// Organizations visible to the caller (their own, or every tenant for the platform).
    pub organizations: Vec<OrganizationBody>,
}

/// Public representation of a site.
#[derive(Debug, Serialize)]
pub struct SiteBody {
    /// Site id.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Stable handle inside the organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// `active` or `archived`.
    pub status: String,
    /// Theme the renderer activates (`themes/<key>`).
    pub theme: String,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last change, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&Site> for SiteBody {
    fn from(site: &Site) -> Self {
        Self {
            id: site.id,
            organization_id: site.organization_id,
            key: site.key.clone(),
            name: site.name.clone(),
            status: site.status.clone(),
            theme: site.theme.clone(),
            created_at: site.created_at,
            updated_at: site.updated_at,
        }
    }
}

/// Response body of the site endpoints.
#[derive(Debug, Serialize)]
pub struct SitesResponse {
    /// Sites visible to the caller.
    pub sites: Vec<SiteBody>,
}

/// Public representation of a site domain.
#[derive(Debug, Serialize)]
pub struct DomainBody {
    /// Domain id.
    pub id: Uuid,
    /// Site it addresses.
    pub site_id: Uuid,
    /// Host name, lowercase.
    pub host: String,
    /// Whether the site prefers this domain for absolute links.
    pub is_primary: bool,
    /// Creation timestamp, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&SiteDomain> for DomainBody {
    fn from(domain: &SiteDomain) -> Self {
        Self {
            id: domain.id,
            site_id: domain.site_id,
            host: domain.host.clone(),
            is_primary: domain.is_primary,
            created_at: domain.created_at,
        }
    }
}

/// Response body of the domain endpoints.
#[derive(Debug, Serialize)]
pub struct DomainsResponse {
    /// The site the domains belong to.
    pub site_id: Uuid,
    /// Domains, primary first.
    pub domains: Vec<DomainBody>,
}

// ---------------------------------------------------------------------------------------------
// Request shapes
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/organizations`.
#[derive(Debug, Deserialize)]
pub struct CreateOrganizationRequest {
    /// Display name.
    pub name: String,
    /// Desired slug (`acme-corp`).
    pub slug: String,
}

/// `PATCH /api/v1/organizations/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateOrganizationRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New status: `active`, `suspended` or `archived`.
    #[serde(default)]
    pub status: Option<String>,
}

impl UpdateOrganizationRequest {
    /// Fold the request into the store's change set.
    fn changes(self) -> OrganizationChanges {
        OrganizationChanges {
            name: self.name,
            status: self.status,
        }
    }
}

/// `POST /api/v1/sites`.
#[derive(Debug, Deserialize)]
pub struct CreateSiteRequest {
    /// Organization to create the site in; defaults to the caller's own.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    /// Stable handle inside the organization (`main`, `careers-tr`).
    pub key: String,
    /// Display name.
    pub name: String,
    /// Theme to render with; the default theme when absent.
    #[serde(default)]
    pub theme: Option<String>,
}

/// `PATCH /api/v1/sites/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateSiteRequest {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New status: `active` or `archived`.
    #[serde(default)]
    pub status: Option<String>,
    /// New theme key (`themes/<key>`).
    #[serde(default)]
    pub theme: Option<String>,
}

impl UpdateSiteRequest {
    /// Fold the request into the store's change set.
    fn changes(self) -> SiteChanges {
        SiteChanges {
            name: self.name,
            status: self.status,
            theme: self.theme,
        }
    }
}

/// Query parameters of `GET /api/v1/sites`.
#[derive(Debug, Deserialize)]
pub struct SitesQuery {
    /// Organization filter; defaults to the caller's own organization.
    #[serde(default)]
    pub organization_id: Option<Uuid>,
}

/// `POST /api/v1/sites/{id}/domains`.
#[derive(Debug, Deserialize)]
pub struct CreateDomainRequest {
    /// Host name that should address the site.
    pub host: String,
    /// Promote the new host to the site's primary domain.
    #[serde(default)]
    pub is_primary: bool,
}

// ---------------------------------------------------------------------------------------------
// Organization handlers
// ---------------------------------------------------------------------------------------------

/// List the organizations visible to the caller.
pub async fn list_organizations(
    State(state): State<AppState>,
    current: CurrentSession,
) -> Result<Json<OrganizationsResponse>, ApiError> {
    let pool = state.db().pool();
    let organizations = match current.user.organization_id {
        Some(own) => organizations::find_organization(pool, own)
            .await?
            .into_iter()
            .collect(),
        None => organizations::list_organizations(pool).await?,
    };

    Ok(Json(OrganizationsResponse {
        organizations: organizations.iter().map(OrganizationBody::from).collect(),
    }))
}

/// Open a new tenant. Platform-level accounts only.
pub async fn create_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateOrganizationRequest>,
) -> Result<(StatusCode, Json<OrganizationBody>), ApiError> {
    platform_only(&current)?;

    let organization = organizations::create_organization(
        state.db().pool(),
        NewOrganization {
            name: body.name,
            slug: body.slug,
        },
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.created")
            .target("organization", organization.id.to_string())
            .metadata(json!({ "slug": organization.slug, "name": organization.name }))
            .ip_address(address.as_text())
            .organization(organization.id),
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(OrganizationBody::from(&organization)),
    ))
}

/// Read one organization.
pub async fn get_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
) -> Result<Json<OrganizationBody>, ApiError> {
    let organization = load_organization(&state, organization_id).await?;
    ensure_same_organization(&current, Some(organization.id))?;

    Ok(Json(OrganizationBody::from(&organization)))
}

/// Edit one organization.
pub async fn update_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<UpdateOrganizationRequest>,
) -> Result<Json<OrganizationBody>, ApiError> {
    let organization = load_organization(&state, organization_id).await?;
    ensure_same_organization(&current, Some(organization.id))?;

    let changes = body.changes();
    let updated =
        organizations::update_organization(state.db().pool(), organization.id, &changes).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.updated")
            .target("organization", updated.id.to_string())
            .metadata(json!({
                "slug": updated.slug,
                "name": updated.name,
                "status": updated.status,
            }))
            .ip_address(address.as_text())
            .organization(updated.id),
    )
    .await?;

    Ok(Json(OrganizationBody::from(&updated)))
}

/// Delete a tenant that no longer owns sites. Platform-level accounts only.
///
/// Sites must go first (or be archived) — deleting a populated tenant in one step would take
/// its content with it, so the API asks the operator to empty it explicitly.
pub async fn delete_organization(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(organization_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    platform_only(&current)?;
    let organization = load_organization(&state, organization_id).await?;

    let sites = sites::count_for_organization(state.db().pool(), organization.id).await?;
    if sites > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "organization_not_empty",
            format!("this organization still owns {sites} site(s); delete them first"),
        ));
    }

    if !organizations::delete_organization(state.db().pool(), organization.id).await? {
        return Err(organization_not_found());
    }

    // The tenant is gone, so the row is platform-level and carries the tenant in its metadata.
    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "organization.deleted")
            .target("organization", organization.id.to_string())
            .metadata(json!({ "slug": organization.slug, "name": organization.name }))
            .ip_address(address.as_text()),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Site handlers
// ---------------------------------------------------------------------------------------------

/// List the sites visible to the caller.
pub async fn list_sites(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(query): Query<SitesQuery>,
) -> Result<Json<SitesResponse>, ApiError> {
    let pool = state.db().pool();
    let sites = match (current.user.organization_id, query.organization_id) {
        (Some(own), Some(requested)) if own != requested => {
            return Err(crate::scope::cross_organization());
        }
        (Some(own), _) => sites::list_sites_for_organization(pool, own).await?,
        (None, Some(requested)) => sites::list_sites_for_organization(pool, requested).await?,
        (None, None) => sites::list_sites(pool).await?,
    };

    Ok(Json(SitesResponse {
        sites: sites.iter().map(SiteBody::from).collect(),
    }))
}

/// Create a site inside an organization.
pub async fn create_site(
    State(state): State<AppState>,
    current: CurrentSession,
    address: ClientAddress,
    Json(body): Json<CreateSiteRequest>,
) -> Result<(StatusCode, Json<SiteBody>), ApiError> {
    let organization_id = resolve_organization(&current, body.organization_id)?;

    if organizations::find_organization(state.db().pool(), organization_id)
        .await?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "organization_not_found",
            "no such organization",
        ));
    }

    let site = sites::create_site(
        state.db().pool(),
        NewSite {
            organization_id,
            key: body.key,
            name: body.name,
            theme: body.theme,
        },
    )
    .await?;

    // A new site is a new thing to find (REQ-002): the bus carries it, the index follows.
    bus::emit(
        state.db().pool(),
        NewEvent::new("site.created")
            .organization(site.organization_id)
            .site(site.id)
            .actor(current.user.id)
            .payload(json!({
                "site_id": site.id,
                "key": site.key,
                "name": site.name,
                "theme": site.theme,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.created")
            .target("site", site.id.to_string())
            .metadata(json!({ "key": site.key, "name": site.name }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(SiteBody::from(&site))))
}

/// Read one site.
pub async fn get_site(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<SiteBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    Ok(Json(SiteBody::from(&site)))
}

/// Edit one site.
pub async fn update_site(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<UpdateSiteRequest>,
) -> Result<Json<SiteBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let updated = sites::update_site(state.db().pool(), site.id, &body.changes()).await?;

    // A rename or a theme change is worth finding again: the index re-reads the site row.
    bus::emit(
        state.db().pool(),
        NewEvent::new("site.updated")
            .organization(updated.organization_id)
            .site(updated.id)
            .actor(current.user.id)
            .payload(json!({
                "site_id": updated.id,
                "key": updated.key,
                "name": updated.name,
                "status": updated.status,
                "theme": updated.theme,
            })),
    )
    .await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.updated")
            .target("site", updated.id.to_string())
            .metadata(json!({
                "key": updated.key,
                "name": updated.name,
                "status": updated.status,
                "theme": updated.theme,
            }))
            .ip_address(address.as_text())
            .organization(updated.organization_id),
    )
    .await?;

    Ok(Json(SiteBody::from(&updated)))
}

/// Delete a site; its domains follow.
pub async fn delete_site(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;

    if !sites::delete_site(state.db().pool(), site.id).await? {
        return Err(site_not_found());
    }

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.deleted")
            .target("site", site.id.to_string())
            .metadata(json!({ "key": site.key, "name": site.name }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Domain handlers
// ---------------------------------------------------------------------------------------------

/// List the domains of a site, primary first.
pub async fn list_domains(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
) -> Result<Json<DomainsResponse>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let domains = sites::list_domains(state.db().pool(), site.id).await?;

    Ok(Json(DomainsResponse {
        site_id: site.id,
        domains: domains.iter().map(DomainBody::from).collect(),
    }))
}

/// Bind a host to a site.
pub async fn add_domain(
    State(state): State<AppState>,
    current: CurrentSession,
    Path(site_id): Path<Uuid>,
    address: ClientAddress,
    Json(body): Json<CreateDomainRequest>,
) -> Result<(StatusCode, Json<DomainBody>), ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let domain = sites::add_domain(state.db().pool(), site.id, &body.host, body.is_primary).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.domain.added")
            .target("site", site.id.to_string())
            .metadata(json!({ "host": domain.host, "primary": domain.is_primary }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok((StatusCode::CREATED, Json(DomainBody::from(&domain))))
}

/// Promote one domain of a site to the primary one.
pub async fn set_primary_domain(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, domain_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
) -> Result<Json<DomainBody>, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let domain = sites::set_primary_domain(state.db().pool(), site.id, domain_id).await?;

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.domain.primary_changed")
            .target("site", site.id.to_string())
            .metadata(json!({ "host": domain.host }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(Json(DomainBody::from(&domain)))
}

/// Remove a domain from a site.
pub async fn remove_domain(
    State(state): State<AppState>,
    current: CurrentSession,
    Path((site_id, domain_id)): Path<(Uuid, Uuid)>,
    address: ClientAddress,
) -> Result<StatusCode, ApiError> {
    let site = site_in_scope(&state, &current, site_id).await?;
    let removed = sites::remove_domain(state.db().pool(), site.id, domain_id).await?;
    let Some(removed) = removed else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "domain_not_found",
            "no such domain on this site",
        ));
    };

    record(
        &state,
        NewAuditEntry::by_user(current.user.id, "site.domain.removed")
            .target("site", site.id.to_string())
            .metadata(json!({ "host": removed.host, "primary": removed.is_primary }))
            .ip_address(address.as_text())
            .organization(site.organization_id),
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Write an audit row; a privileged action is not reported as successful without one.
async fn record(state: &AppState, entry: NewAuditEntry) -> Result<(), ApiError> {
    omnion_audit::record(state.db().pool(), entry).await?;
    Ok(())
}

/// Load an organization or answer `404 organization_not_found`.
async fn load_organization(state: &AppState, id: Uuid) -> Result<Organization, ApiError> {
    organizations::find_organization(state.db().pool(), id)
        .await?
        .ok_or_else(organization_not_found)
}

/// Load a site or answer `404 site_not_found`.
async fn load_site(state: &AppState, id: Uuid) -> Result<Site, ApiError> {
    sites::find_site(state.db().pool(), id)
        .await?
        .ok_or_else(site_not_found)
}

/// Load a site and refuse it when it lives outside the caller's organization.
async fn site_in_scope(
    state: &AppState,
    current: &CurrentSession,
    id: Uuid,
) -> Result<Site, ApiError> {
    let site = load_site(state, id).await?;
    ensure_same_organization(current, Some(site.organization_id))?;
    Ok(site)
}

fn site_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "site_not_found", "no such site")
}

fn organization_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "organization_not_found",
        "no such organization",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_requests_fold_into_change_sets() {
        let organization = UpdateOrganizationRequest {
            name: Some("  Acme  ".to_owned()),
            status: Some("suspended".to_owned()),
        }
        .changes();
        assert_eq!(organization.name.as_deref(), Some("  Acme  "));
        assert_eq!(organization.status.as_deref(), Some("suspended"));
        assert!(!organization.is_empty());

        let site = UpdateSiteRequest {
            name: None,
            status: None,
            theme: None,
        }
        .changes();
        assert!(site.is_empty(), "an empty patch changes nothing");

        let site = UpdateSiteRequest {
            name: None,
            status: None,
            theme: Some("minimal".to_owned()),
        }
        .changes();
        assert!(!site.is_empty(), "a theme patch changes the site");
        assert_eq!(site.theme.as_deref(), Some("minimal"));
    }

    #[test]
    fn create_site_defaults_to_the_callers_organization() {
        let body: CreateSiteRequest =
            serde_json::from_str(r#"{"key":"main","name":"Main"}"#).expect("valid body");
        assert!(body.organization_id.is_none(), "the field is optional");

        let body: CreateSiteRequest =
            serde_json::from_str(r#"{"organization_id":null,"key":"main","name":"Main"}"#)
                .expect("valid body");
        assert!(body.organization_id.is_none());
    }

    #[test]
    fn a_domain_request_defaults_to_a_secondary_host() {
        let body: CreateDomainRequest =
            serde_json::from_str(r#"{"host":"www.acme.test"}"#).expect("valid body");
        assert!(!body.is_primary, "adding a host does not move the primary");
    }
}
