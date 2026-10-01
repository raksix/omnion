//! HTTP route table for the Omnion API.
//!
//! API routes are versioned under `/api/v1` from the first release; operational endpoints
//! (`/healthz`, `/readyz`) stay unversioned so probes never depend on the API version.
//!
//! Routes that change state or read privileged data carry a permission guard
//! (`crate::guards::require`); `GET /api/v1/me` and sign-in/out stay open to any signed-in
//! account, and `GET /api/v1/iam/effective-permissions` resolves the caller's own set without a
//! permission because it answers "what may I do here".
//!
//! The tenancy surface (`/organizations`, `/sites`) is guarded by the `organizations.*`,
//! `sites.*` and `domains.manage` permissions and additionally scoped in the handlers
//! (`crate::scope`): organization accounts only ever see and change their own organization.
//!
//! The content surface (`/pages`) is guarded by the `content.pages.*` permissions; its
//! handlers apply the same scope rule through the site a page belongs to.
//!
//! The public surface (`/public`) is the one unauthenticated read route of the API: it serves
//! published content to the public site renderer (`apps/web`) and resolves the addressed site
//! from the request — see `crate::routes::public` for the resolution order.
//!
//! The media surface (`/media`) is the library a site's content points at: uploads are guarded
//! by `media.upload`, reads by `media.read` and removals by `media.delete`, all scoped through
//! the site the file belongs to. Its bytes are served twice — on the panel surface behind the
//! session and on the public surface for the renderer — see `crate::routes::media`.
//!
//! The workflow surface (`/workflows`, `/workflow-executions`) is the automation engine of P09:
//! definitions are read with `workflows.read`, written with `workflows.manage`, started and
//! cancelled with `workflows.run` — see `crate::routes::workflows`. The background runner that
//! advances the runs lives in `crate::workflow_runner`.
//!
//! The onboarding surface (`/onboarding`) is the first-run flow of a fresh installation
//! (REQ-050, P10): it carries no permission guard because there is nothing to check against
//! until an account exists — the flow itself decides who may act (see
//! `crate::routes::onboarding`).
//!
//! The AI Hub surface (`/ai`) is the platform's door to AI (docs/06-AI-HUB.md, P11): providers
//! and the model registry are read with `ai.providers.read` and written with
//! `ai.providers.manage`, and `POST /ai/chat` answers as `text/event-stream` behind the
//! `ai.chat` key — using the platform's AI and connecting a provider are separate powers. See
//! `crate::routes::ai`.
//!
//! The events and webhooks surface (`/webhooks`, `/events`) is the platform's own bus
//! (docs/01-VISION.md §13, P12): endpoints are read with `webhooks.read` and written with
//! `webhooks.manage`, the queue history of one endpoint rides the read key, and the event feed
//! is read with `events.read`. Publishing a page records `page.published` and queues a signed
//! delivery per subscribed endpoint; the worker that sends them lives in `crate::event_runner`.
//! See `crate::routes::webhooks`.
//!
//! The automation surface (`/automations`) is trigger → condition → action (docs/requests/
//! REQ-003, P13): a rule is a workflow whose trigger is an event, so it rides the workflow
//! permission keys and the background matcher in `crate::automation_runner` starts one durably
//! stepped run per match — the same engine P09's manual and scheduled runs use. The vocabulary a
//! rule is written in is closed and readable at `/automations/catalogue`; the actions that touch
//! the world live in `omnion-automation` (see `crate::workflow_runner`, which installs them).
//!
//! The search surface (`/search`, `/search/suggest`, `/search/status`, `/search/reindex`) is the
//! platform's one search box over the `omnion-search` index (docs/requests/REQ-002): searching
//! is `search.read` — the box every signed-in account holds — while the answer is narrowed to
//! the providers whose own read permissions the caller carries, and rebuilding the index is the
//! separate `search.manage`. The indexer that keeps the documents fresh from the event bus is
//! `crate::search_runner`. See `crate::routes::search`.
//!
//! The analytics surface (`/analytics/*`, docs/requests/REQ-007) is the platform's own
//! measurement engine: the panel side (`/analytics/settings`, `/analytics/snippet`) is read
//! with `analytics.read` and written with `analytics.settings.manage` in the caller's own
//! organization, while the collection endpoint (`POST /api/v1/public/analytics/collect`) is
//! public by nature — a rendered site's tracking script posts to it — and protected by a body
//! cap, a per-site rate limit and a collector that decides before it writes. The worker that
//! keeps the rollups fresh is `crate::analytics_runner`.

pub mod ai;
pub mod analytics;
pub mod auth;
pub mod automation;
pub mod backups;
pub mod cdn;
pub mod cdn_cache;
pub mod cdn_purge;
pub mod developer;
pub mod commands;
pub mod content;
pub mod deployment;
pub mod deployment_cluster;
pub mod deployment_ops;
pub mod deployment_run;
pub mod environments;
pub mod health;
pub mod health_incidents;
pub mod health_panel;
pub mod iam;
pub mod iam_approvals;
pub mod iam_policy;
pub mod iam_providers;
pub mod iam_provisioning;
pub mod iam_security;
pub mod iam_subjects;
pub mod me;
pub mod media;
pub mod media_duplicates;
pub mod media_files;
pub mod media_grants;
pub mod media_retention;
pub mod media_scan;
mod media_settings;
pub mod media_shares;
pub mod media_transform;
pub mod media_usage;
pub mod media_versions;
pub mod notifications;
pub mod notifications_admin;
pub mod notifications_test;
pub mod onboarding;
pub mod promotions;
pub mod public;
pub mod readyz;
pub mod restore_jobs;
pub mod scim;
pub mod search;
pub mod security;
pub mod security_events;
pub mod security_headers;
pub mod security_ip;
pub mod security_limiter;
pub mod security_secrets;
pub mod sso;
pub mod tenancy;
pub mod tenancy_departments;
pub mod tenancy_limits;
pub mod tenancy_member_detail;
pub mod tenancy_members;
pub mod webauthn;
pub mod webhooks;
pub mod workflows;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::MethodRouter;
use axum::routing::{delete, get, patch, post, put};

use std::convert::Infallible;

use crate::guards;
use crate::state::AppState;

/// Body cap of the public invitation routes (REQ-005): a preview sends nothing and an
/// acceptance carries a display name and a password, so a few kilobytes is generous.
const INVITATION_BODY_LIMIT: usize = 16 * 1024;

/// Build the application router around the shared [`AppState`].
pub fn router(state: AppState) -> Router {
    let roles = get(iam::list_roles)
        .layer(guards::require(&state, "iam.roles.read"))
        .merge(post(iam::create_role).layer(guards::require(&state, "iam.roles.manage")));

    // Role depth (REQ-006, slice 1): reading a role and its history is `iam.roles.read`, while
    // editing, cloning, previewing and deleting are `iam.roles.manage`.
    let role_detail = get(iam::get_role)
        .layer(guards::require(&state, "iam.roles.read"))
        .merge(patch(iam::update_role).layer(guards::require(&state, "iam.roles.manage")))
        .merge(delete(iam::delete_role).layer(guards::require(&state, "iam.roles.manage")));

    let role_versions =
        get(iam::list_role_versions).layer(guards::require(&state, "iam.roles.read"));

    let role_members = get(iam::list_role_members).layer(guards::require(&state, "iam.roles.read"));

    let role_duplicate =
        post(iam::duplicate_role).layer(guards::require(&state, "iam.roles.manage"));

    let role_preview =
        post(iam::preview_role_permissions).layer(guards::require(&state, "iam.roles.manage"));

    let bindings = get(iam::list_bindings)
        .layer(guards::require(&state, "iam.bindings.read"))
        .merge(post(iam::create_binding).layer(guards::require(&state, "iam.bindings.manage")));

    let binding_detail =
        delete(iam::delete_binding).layer(guards::require(&state, "iam.bindings.manage"));

    // Subjects, scopes, groups, machine identities and the simulator (REQ-006, slice 2):
    // reading a screen is its read key, every mutation its manage key, and the simulator asks
    // with `iam.simulate` because it exposes the decision path.
    let iam_users = get(iam_subjects::list_users)
        .layer(guards::require(&state, "users.read"))
        .merge(post(iam_subjects::create_user).layer(guards::require(&state, "users.create")));

    let iam_user = get(iam_subjects::get_user)
        .layer(guards::require(&state, "users.read"))
        .merge(patch(iam_subjects::update_user).layer(guards::require(&state, "users.update")));

    let iam_groups = get(iam_subjects::list_groups)
        .layer(guards::require(&state, "iam.groups.read"))
        .merge(
            post(iam_subjects::create_group).layer(guards::require(&state, "iam.groups.manage")),
        );

    let iam_group = get(iam_subjects::get_group)
        .layer(guards::require(&state, "iam.groups.read"))
        .merge(
            patch(iam_subjects::update_group).layer(guards::require(&state, "iam.groups.manage")),
        )
        .merge(
            delete(iam_subjects::delete_group).layer(guards::require(&state, "iam.groups.manage")),
        );

    let iam_group_members =
        put(iam_subjects::set_group_members).layer(guards::require(&state, "iam.groups.manage"));

    let iam_service_accounts = get(iam_subjects::list_service_accounts)
        .layer(guards::require(&state, "iam.serviceaccounts.read"))
        .merge(
            post(iam_subjects::create_service_account)
                .layer(guards::require(&state, "iam.serviceaccounts.manage")),
        );

    let iam_service_account = get(iam_subjects::get_service_account)
        .layer(guards::require(&state, "iam.serviceaccounts.read"))
        .merge(
            delete(iam_subjects::delete_service_account)
                .layer(guards::require(&state, "iam.serviceaccounts.manage")),
        );

    let iam_service_account_keys = post(iam_subjects::issue_service_account_key)
        .layer(guards::require(&state, "iam.serviceaccounts.manage"));

    let iam_service_account_key = delete(iam_subjects::revoke_service_account_key)
        .layer(guards::require(&state, "iam.serviceaccounts.manage"));

    let iam_simulations = post(iam_subjects::run_simulation)
        .layer(guards::require_or_machine(&state, "iam.simulate"));

    let iam_overview = get(iam_subjects::overview).layer(guards::require(&state, "iam.roles.read"));

    // ABAC policies (REQ-006, slice 4a): the list, the builder's save and the dry run. Reading a
    // policy and testing one touch nothing (`iam.policies.read`); saving and removing do
    // (`iam.policies.manage`).
    let iam_policies = get(iam_policy::list_policies)
        .layer(guards::require(&state, "iam.policies.read"))
        .merge(
            post(iam_policy::create_policy).layer(guards::require(&state, "iam.policies.manage")),
        );

    let iam_policy = get(iam_policy::get_policy)
        .layer(guards::require(&state, "iam.policies.read"))
        .merge(put(iam_policy::update_policy).layer(guards::require(&state, "iam.policies.manage")))
        .merge(
            delete(iam_policy::delete_policy).layer(guards::require(&state, "iam.policies.manage")),
        );

    let iam_policy_versions =
        get(iam_policy::list_policy_versions).layer(guards::require(&state, "iam.policies.read"));

    let iam_policy_test =
        post(iam_policy::test_policy).layer(guards::require(&state, "iam.policies.read"));

    // Permission requests and approvals (REQ-006, slice 4b): asking needs only a session, the
    // inbox needs `iam.approvals.read` and deciding needs `iam.approvals.decide`.
    let iam_approvals =
        get(iam_approvals::list_approvals).layer(guards::require(&state, "iam.approvals.read"));
    let iam_approval_decide =
        post(iam_approvals::decide_approval).layer(guards::require(&state, "iam.approvals.decide"));
    let iam_requests =
        get(iam_approvals::list_my_requests).merge(post(iam_approvals::create_request));

    // SCIM 2.0 provisioning (REQ-006, slice 4b): authenticated by a provisioning token, so the
    // surface sits outside the session guard and verifies its own bearer credential.
    let scim_users = get(scim::list_users).merge(post(scim::create_user));
    let scim_user = get(scim::get_user)
        .merge(put(scim::replace_user))
        .merge(patch(scim::patch_user))
        .merge(delete(scim::delete_user));
    let scim_groups = get(scim::list_groups).merge(post(scim::create_group));
    let scim_group = get(scim::get_group)
        .merge(patch(scim::patch_group))
        .merge(delete(scim::delete_group));
    let scim_config = get(scim::service_provider_config);
    let scim_schemas = get(scim::schemas);

    // Provisioning tokens and their sync log (REQ-006, slice 4b): one management key covers the
    // tokens and the log, because both describe how the directory talks to this platform.
    let iam_provisioning_tokens = get(iam_provisioning::list_tokens)
        .merge(post(iam_provisioning::create_token))
        .layer(guards::require(&state, "iam.provisioning.manage"));
    let iam_provisioning_token = delete(iam_provisioning::revoke_token)
        .layer(guards::require(&state, "iam.provisioning.manage"));
    let iam_provisioning_log =
        get(iam_provisioning::list_log).layer(guards::require(&state, "iam.provisioning.manage"));

    // Enterprise sign-in providers (REQ-006, slice 4b-2): reading the connected providers is
    // `iam.providers.read`, connecting/changing/testing/removing one is `iam.providers.manage`.
    // The browser half of the same feature (`/auth/sso/...`) is public by nature — it *is* the
    // sign-in — and lives in `crate::routes::sso`, which re-derives its own trust from the
    // single-use challenge rather than from a session.
    let iam_providers = get(iam_providers::list_providers)
        .layer(guards::require(&state, "iam.providers.read"))
        .merge(
            post(iam_providers::create_provider)
                .layer(guards::require(&state, "iam.providers.manage")),
        );

    let iam_provider = get(iam_providers::get_provider)
        .layer(guards::require(&state, "iam.providers.read"))
        .merge(
            patch(iam_providers::update_provider)
                .layer(guards::require(&state, "iam.providers.manage")),
        )
        .merge(
            delete(iam_providers::delete_provider)
                .layer(guards::require(&state, "iam.providers.manage")),
        );

    let iam_provider_test =
        post(iam_providers::test_provider).layer(guards::require(&state, "iam.providers.manage"));

    let iam_provider_events = get(iam_providers::list_provider_events)
        .layer(guards::require(&state, "iam.providers.read"));

    // The public sign-in surface: no guard, because there is no session yet — the same reason
    // `auth/login` and `auth/mfa/verify` carry none. `sso/{slug}/saml` is the panel page a SAML
    // provider posts the assertion back from, and `sso/{slug}/callback` answers both a `code`
    // query and a posted assertion.
    let sso_providers = get(sso::list_providers);
    let sso_start = get(sso::start);
    let sso_saml_page = get(sso::saml_page);
    let sso_callback = get(sso::callback);
    let sso_saml_callback = post(sso::saml_callback);

    // Security policy, sessions, devices and second factors (REQ-006, slice 3). Reading a list
    // needs its read key; every mutation carries its own, and the dangerous ones (resetting
    // factors, revoking sessions) additionally demand a fresh step-up inside the handler.
    let iam_security_policy = get(iam_security::get_security_policy)
        .layer(guards::require(&state, "iam.security.read"))
        .merge(
            put(iam_security::update_security_policy)
                .layer(guards::require(&state, "iam.security.manage")),
        );

    let iam_sessions =
        get(iam_security::list_sessions).layer(guards::require(&state, "iam.sessions.read"));

    let iam_session =
        delete(iam_security::revoke_session).layer(guards::require(&state, "iam.sessions.revoke"));

    let iam_sign_out_all =
        post(iam_security::sign_out_all).layer(guards::require(&state, "iam.sessions.revoke"));

    let iam_devices =
        get(iam_security::list_devices).layer(guards::require(&state, "iam.devices.read"));

    let iam_device_trust =
        post(iam_security::trust_device).layer(guards::require(&state, "iam.devices.manage"));

    let iam_device =
        delete(iam_security::forget_device).layer(guards::require(&state, "iam.devices.manage"));

    // A factor belongs to an account, so the routes read with `users.read` and write with
    // `users.update` — the same keys that guard editing the account itself.
    let iam_user_mfa = get(iam_security::list_factors)
        .layer(guards::require(&state, "users.read"))
        .merge(post(iam_security::enroll_totp).layer(guards::require(&state, "users.update")));

    let iam_user_mfa_confirm =
        post(iam_security::confirm_totp).layer(guards::require(&state, "users.update"));

    let iam_user_mfa_reset =
        post(iam_security::reset_mfa).layer(guards::require(&state, "users.update"));

    let iam_user_factor =
        delete(iam_security::revoke_factor).layer(guards::require(&state, "users.update"));

    // The second factor of a sign-in is a public route (the sign-in is half done; there is no
    // session yet), and the step-up route needs the session it is improving.
    let auth_mfa_verify = post(iam_security::verify_mfa_login);
    let auth_step_up = post(iam_security::step_up);

    // Passkeys (REQ-006, slice 3b): enrolment runs behind the caller's own session (a passkey
    // belongs to the account at the keyboard), and the sign-in half sits beside `auth/mfa/verify`
    // — a half-finished sign-in that a verified assertion turns into a session.
    let webauthn_passkeys = get(webauthn::list_passkeys);
    let webauthn_passkey = delete(webauthn::revoke_passkey);
    let webauthn_register_begin = post(webauthn::register_begin);
    let webauthn_register_complete = post(webauthn::register_complete);
    let auth_webauthn_begin = post(webauthn::authenticate_begin);
    let auth_webauthn_complete = post(webauthn::authenticate_complete);

    // Tenancy: reading needs a read permission, every mutation its own key.
    let organizations = get(tenancy::list_organizations)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            post(tenancy::create_organization)
                .layer(guards::require(&state, "organizations.manage")),
        );

    // The tenant's own people (REQ-005, slice 1): memberships and invitations of one
    // organization, plus the public invitation routes and the switcher's two session-scoped
    // routes under `/me`. Reads need `organizations.read`, writes `organizations.manage`; the
    // public invitation preview carries no guard because the token is its own credential, so
    // it is rate-limited and answers uninformatively instead.
    let organization_members = get(tenancy_members::list_members)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            post(tenancy_members::add_member)
                .layer(guards::require(&state, "organizations.manage")),
        );

    // The member drawer's read is the `GET` of the same resource (REQ-005, slice 4) rather than a
    // `/detail` sub-path: a member *is* the row, and a screen that had to ask for a second
    // address to see one member would be a second route to keep in step with this one.
    let organization_member = get(tenancy_member_detail::get_member)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            patch(tenancy_members::update_member)
                .layer(guards::require(&state, "organizations.manage")),
        )
        .merge(
            delete(tenancy_members::remove_member)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let organization_invitations = get(tenancy_members::list_invitations)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            post(tenancy_members::create_invitation)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let organization_invitation = delete(tenancy_members::revoke_invitation)
        .layer(guards::require(&state, "organizations.manage"));

    // The `owner_approval` queue (REQ-005, slice 3). Reading it is the same `organizations.read`
    // the Members tab already has; releasing is `organizations.manage` *plus* the owner check
    // inside the handler, because "who may release" is a per-tenant fact the permission catalogue
    // cannot express — a manager manages the organization and still cannot approve here.
    let organization_invitation_queue = get(tenancy_members::list_queued_invitations)
        .layer(guards::require(&state, "organizations.read"));

    let organization_invitation_approval = post(tenancy_members::approve_invitation)
        .layer(guards::require(&state, "organizations.manage"));

    // Departments (REQ-005, slice 2). Reads ride `organizations.read`, writes
    // `organizations.manage`; binding a role to a department is an IAM change, so it asks for
    // `iam.bindings.manage` as well rather than being reachable by a tenant administrator alone.
    let organization_departments = get(tenancy_departments::list_departments)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            post(tenancy_departments::create_department)
                .layer(guards::require(&state, "organizations.manage")),
        );
    let organization_department = get(tenancy_departments::get_department)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            patch(tenancy_departments::update_department)
                .layer(guards::require(&state, "organizations.manage")),
        )
        .merge(
            post(tenancy_departments::archive_department)
                .layer(guards::require(&state, "organizations.manage")),
        )
        .merge(
            delete(tenancy_departments::delete_department)
                .layer(guards::require(&state, "organizations.manage")),
        );
    let department_members = post(tenancy_departments::add_department_member)
        .layer(guards::require(&state, "organizations.manage"))
        .merge(
            delete(tenancy_departments::remove_department_member)
                .layer(guards::require(&state, "organizations.manage")),
        );
    let department_roles = post(tenancy_departments::bind_department_role)
        .layer(guards::require(&state, "organizations.manage"))
        .layer(guards::require(&state, "iam.bindings.manage"))
        .merge(
            delete(tenancy_departments::unbind_department_role)
                .layer(guards::require(&state, "organizations.manage"))
                .layer(guards::require(&state, "iam.bindings.manage")),
        );
    let member_departments = get(tenancy_departments::list_member_departments)
        .layer(guards::require(&state, "organizations.read"));

    // The member drawer (REQ-005, slice 4): one read of everything the drawer shows, and the
    // three binding operations beside it. The read rides `organizations.read`; the writes need
    // *both* `organizations.manage` (they are changes to this tenant's people) and
    // `iam.bindings.manage` (they are role grants) — exactly as the department-role route above
    // already does, because a grant is a grant whoever asks for it through a tenant path.
    let organization_member_role_bindings: MethodRouter<AppState, Infallible> =
        post(tenancy_member_detail::grant_member_role)
            .layer(guards::require(&state, "organizations.manage"))
            .layer(guards::require(&state, "iam.bindings.manage"));
    let organization_member_role_binding: MethodRouter<AppState, Infallible> =
        patch(tenancy_member_detail::extend_member_role)
            .merge(delete(tenancy_member_detail::revoke_member_role))
            .layer(guards::require(&state, "organizations.manage"))
            .layer(guards::require(&state, "iam.bindings.manage"));

    // Settings, modules, limits and usage (REQ-005, slice 3). Reads ride
    // `organizations.read`, writes `organizations.manage`. The usage route is a `GET` that
    // answers either a JSON payload or a CSV download, so it is one handler behind a query
    // flag rather than two routes — the numbers on screen and the numbers in the file then
    // come from the same call and cannot drift apart.
    let organization_settings = get(tenancy_limits::get_settings)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            put(tenancy_limits::update_settings)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let organization_modules = get(tenancy_limits::get_modules)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            put(tenancy_limits::update_modules)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let organization_limits = get(tenancy_limits::get_limits)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            put(tenancy_limits::update_limits)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let organization_usage =
        get(tenancy_limits::get_usage).layer(guards::require(&state, "organizations.read"));

    // The Audit tab. `audit.read`, not `organizations.read`: a trail names every privileged act
    // in the tenant, and "can see the member list" is not a reason to be able to see it. The
    // tenant is still resolved from the session, so the id in the path is a selection rather
    // than an access decision.
    let organization_audit =
        get(tenancy_limits::get_audit).layer(guards::require(&state, "audit.read"));

    // The switcher's two routes are session-scoped by design (the request's own table says
    // "session only"): they only ever return the caller's own memberships and switch to one
    // they already hold. A permission guard here would be a dead end - the caller's role
    // binding lives in the organization they are leaving, so the second switch would answer
    // `403` and the panel could never come back. The membership check inside the handler is
    // the real authorization: a caller with no membership in the named tenant is refused.
    let my_organizations = get(tenancy_members::my_organizations);

    let switch_organization = post(tenancy_members::switch_organization);

    // The public invitation routes carry no permission guard — the token is the credential —
    // so they get their own small body cap (a preview sends nothing, an acceptance carries a
    // name and a password) instead of the router-wide limit.
    let invitation_preview = Router::new()
        .route(
            "/invitations/{token}",
            get(tenancy_members::preview_invitation),
        )
        .route(
            "/invitations/{token}/accept",
            post(tenancy_members::accept_invitation),
        )
        .layer(DefaultBodyLimit::max(INVITATION_BODY_LIMIT));

    let organization = get(tenancy::get_organization)
        .layer(guards::require(&state, "organizations.read"))
        .merge(
            patch(tenancy::update_organization)
                .layer(guards::require(&state, "organizations.manage")),
        )
        .merge(
            delete(tenancy::delete_organization)
                .layer(guards::require(&state, "organizations.manage")),
        );

    let sites = get(tenancy::list_sites)
        .layer(guards::require(&state, "sites.read"))
        .merge(post(tenancy::create_site).layer(guards::require(&state, "sites.create")));

    let site = get(tenancy::get_site)
        .layer(guards::require(&state, "sites.read"))
        .merge(patch(tenancy::update_site).layer(guards::require(&state, "sites.update")))
        .merge(delete(tenancy::delete_site).layer(guards::require(&state, "sites.delete")));

    let domains = get(tenancy::list_domains)
        .layer(guards::require(&state, "sites.read"))
        .merge(post(tenancy::add_domain).layer(guards::require(&state, "domains.manage")));

    let domain = delete(tenancy::remove_domain).layer(guards::require(&state, "domains.manage"));

    let domain_primary =
        post(tenancy::set_primary_domain).layer(guards::require(&state, "domains.manage"));

    // Content: pages and their revision history (docs/05-VERSIONING.md §4–§7). Reading the
    // history needs the read key; every mutation carries its own.
    // The content surface is also the developer surface (REQ-033, slice 1): it is what a
    // developer's first integration calls, so it is the surface that accepts an API key.
    //
    // Both guards are installed, session first. `guards::require` resolves the cookie and
    // `developer_auth::require_or_key` stands aside when it finds one, so a signed-in panel is
    // unaffected and a bearer token gets the *key's* scope list rather than a role's — which is
    // the whole difference between the two machine paths documented in `developer_auth`.
    //
    // The scope-enforcement boundary this creates is the acceptance criterion: a key holding
    // only `content.pages.read` reaches `GET /pages` and is refused on `POST /pages` with a 403
    // naming the permission it lacks.
    let pages = get(content::list_pages)
        .layer(guards::require(&state, "content.pages.read"))
        .layer(crate::developer_auth::require_or_key(&state, "content.pages.read"))
        .merge(
            post(content::create_page)
                .layer(guards::require(&state, "content.pages.create"))
                .layer(crate::developer_auth::require_or_key(&state, "content.pages.create")),
        );

    let page = get(content::get_page)
        .layer(guards::require(&state, "content.pages.read"))
        .layer(crate::developer_auth::require_or_key(&state, "content.pages.read"))
        .merge(
            patch(content::update_page)
                .layer(guards::require(&state, "content.pages.update"))
                .layer(crate::developer_auth::require_or_key(&state, "content.pages.update")),
        )
        .merge(
            delete(content::delete_page)
                .layer(guards::require(&state, "content.pages.delete"))
                .layer(crate::developer_auth::require_or_key(&state, "content.pages.delete")),
        );

    let page_publish =
        post(content::publish_page).layer(guards::require(&state, "content.pages.publish"));

    let page_restore =
        post(content::restore_revision).layer(guards::require(&state, "content.pages.restore"));

    let page_revisions =
        get(content::list_revisions).layer(guards::require(&state, "content.pages.read"));

    let page_revision =
        get(content::get_revision).layer(guards::require(&state, "content.pages.read"));

    let page_revision_comments =
        get(content::list_revision_comments).layer(guards::require(&state, "content.pages.read"));

    let page_translations =
        get(content::list_translations).layer(guards::require(&state, "content.pages.read"));

    let page_translation =
        put(content::set_translations).layer(guards::require(&state, "content.pages.update"));

    // Media: the library of a site (docs/01-VISION.md §5). Reading the library and removing a
    // file each carry their own permission; the upload route also lifts the body limit to the
    // library's own file limit, so a legitimate maximum-size file fits while a larger one is
    // refused as it is read — hence its own router.
    let media_upload = Router::new()
        .route(
            "/media",
            post(media::upload_media).layer(guards::require(&state, "media.upload")),
        )
        .layer(DefaultBodyLimit::max(
            omnion_media::MAX_UPLOAD_BYTES as usize + media::UPLOAD_BODY_SLACK,
        ));

    let media = get(media::list_media).layer(guards::require(&state, "media.read"));

    let media_entry = get(media::get_media)
        .layer(guards::require(&state, "media.read"))
        .merge(delete(media::delete_media).layer(guards::require(&state, "media.delete")));

    // The raw path takes `?preset=` (REQ-010 slice 3). Without the parameter it serves the
    // original bytes exactly as before, so every existing caller and every published page keeps
    // working; with it, the answer is a generated derivative.
    // The raw path takes `?preset=` (REQ-010 slice 3) and refuses a file the scanning policy
    // holds (slice 4) — a derivative of a quarantined file is still a quarantined file, and
    // the obvious place to forget that is the path a *published page* fetches.
    let media_raw: MethodRouter<AppState, Infallible> =
        get(media_transform::raw_with_preset).layer(guards::require(&state, "media.read"));

    // File manager (REQ-010, slice 1): folders, the browser listing with its filters, the bulk bar
    // and the trash. Reading the tree needs `media.read`; changing the shape of the library —
    // creating, renaming or moving a folder, moving or deleting files, emptying the trash — needs
    // `media.manage`, which is the folder-and-storage power the catalogue already grants to an
    // editor. The trash routes are `media.manage` too: emptying it is irreversible.
    let media_folders = get(media_files::folder_tree).layer(guards::require(&state, "media.read"));
    let media_folder_create =
        post(media_files::create_folder).layer(guards::require(&state, "media.manage"));
    let media_folder = patch(media_files::move_folder)
        .merge(delete(media_files::delete_folder))
        .layer(guards::require(&state, "media.manage"));

    let media_files_route =
        get(media_files::list_files).layer(guards::require(&state, "media.read"));
    let media_file = get(media_files::get_file)
        .layer(guards::require(&state, "media.read"))
        .merge(patch(media_files::update_file).layer(guards::require(&state, "media.update")))
        .merge(delete(media_files::trash_file).layer(guards::require(&state, "media.delete")));
    let media_file_restore =
        post(media_files::restore_file).layer(guards::require(&state, "media.update"));
    let media_file_purge =
        post(media_files::purge_file).layer(guards::require(&state, "media.manage"));

    let media_trash = get(media_files::list_trash).layer(guards::require(&state, "media.read"));
    let media_trash_empty =
        post(media_files::empty_trash).layer(guards::require(&state, "media.manage"));
    let media_bulk = post(media_files::bulk_action).layer(guards::require(&state, "media.manage"));
    // The version history (REQ-010, slice 2). Reading a version is `media.read`; replacing the
    // bytes and restoring an old one are `media.upload` / `media.update`, the same power an
    // ordinary upload carries — a restore *is* an upload of bytes that already exist.
    let media_versions =
        get(media_versions::list_versions).layer(guards::require(&state, "media.read"));
    let media_version_create =
        post(media_versions::create_version).layer(guards::require(&state, "media.upload"));
    let media_version_restore =
        post(media_versions::restore_version).layer(guards::require(&state, "media.update"));
    let media_version_raw =
        get(media_versions::raw_version).layer(guards::require(&state, "media.read"));
    let media_version_download =
        get(media_versions::download_version).layer(guards::require(&state, "media.read"));

    // Transformation presets (REQ-010, slice 3). Reading the list is `media.read`, because the
    // browser shows a preset picker on every image; changing the set is
    // `media.settings.manage`, which is deliberately *not* `media.manage` — a team that may
    // organise a library does not get to change what every published page renders.
    let media_presets: MethodRouter<AppState, Infallible> =
        get(media_transform::list).layer(guards::require(&state, "media.read"));
    let media_preset_create: MethodRouter<AppState, Infallible> =
        post(media_transform::create).layer(guards::require(&state, "media.settings.manage"));
    let media_preset: MethodRouter<AppState, Infallible> = patch(media_transform::update)
        .merge(delete(media_transform::delete))
        .layer(guards::require(&state, "media.settings.manage"));

    // Storage settings (REQ-010, slice 3). Reading them is `media.read` — the browser's own
    // settings screen is not the only caller, and knowing the upload ceiling is not a secret.
    // Writing them and proving a connection are `media.settings.manage`, the same power the
    // presets carry, and deliberately not `media.manage`: organising a library is not the same
    // permission as repointing where every file in it lives.
    let media_settings_route: MethodRouter<AppState, Infallible> =
        get(media_settings::read).layer(guards::require(&state, "media.read"));
    let media_settings_write: MethodRouter<AppState, Infallible> =
        put(media_settings::write).layer(guards::require(&state, "media.settings.manage"));
    let media_settings_test: MethodRouter<AppState, Infallible> =
        post(media_settings::test_connection)
            .layer(guards::require(&state, "media.settings.manage"));

    // Share links (REQ-010, slice 3). Creating and revoking a link is `media.share`, which is
    // deliberately *not* `media.manage`: handing a file to somebody outside the platform is a
    // different power from organising the library, and a team that may move files around has no
    // business handing them to a client. Reading the list is `media.read` — the file-detail
    // screen shows a file's links to anyone who can open the file.
    let media_shares_route: MethodRouter<AppState, Infallible> =
        get(media_shares::list).layer(guards::require(&state, "media.read"));
    let media_share_create: MethodRouter<AppState, Infallible> =
        post(media_shares::create).layer(guards::require(&state, "media.share"));
    let media_share_revoke: MethodRouter<AppState, Infallible> =
        delete(media_shares::revoke).layer(guards::require(&state, "media.share"));
    let media_share_revoke_all: MethodRouter<AppState, Infallible> =
        post(media_shares::revoke_all).layer(guards::require(&state, "media.share"));

    // The public token route carries no guard and no session, because the token *is* the
    // credential — that is what a share link is. Everything it checks is about the token.
    let public_media_shared = get(media_shares::public_shared);

    // Duplicate detection and merge (REQ-010, slice 3). Reading the report is `media.read` —
    // knowing what a site stores twice costs nothing and helps everybody. Merging is
    // `media.manage`: it rewrites which row a published page resolves to, which is a different
    // power from being able to organise the library.
    let media_duplicates_route: MethodRouter<AppState, Infallible> =
        get(media_duplicates::report).layer(guards::require(&state, "media.read"));
    let media_duplicates_merge: MethodRouter<AppState, Infallible> =
        post(media_duplicates::merge).layer(guards::require(&state, "media.manage"));

    // Scanning (REQ-010, slice 4). Reading the policy, the run log and the quarantine list is
    // `media.read` — the file browser shows a scan badge and an editor needs to know what it
    // means. Changing the policy, running a sweep, probing a scanner and **releasing** a
    // quarantined file are all `media.scan.manage`, which is deliberately its own key: a
    // release is the action that undoes a safety decision, and neither `media.manage`
    // (organise a library) nor `media.delete` (remove a file) is that power.
    let media_scan_route: MethodRouter<AppState, Infallible> =
        get(media_scan::read).layer(guards::require(&state, "media.read"));
    let media_scan_write: MethodRouter<AppState, Infallible> =
        put(media_scan::write).layer(guards::require(&state, "media.scan.manage"));
    let media_scan_run: MethodRouter<AppState, Infallible> =
        post(media_scan::run_now).layer(guards::require(&state, "media.scan.manage"));
    let media_scan_runs_route: MethodRouter<AppState, Infallible> =
        get(media_scan::runs).layer(guards::require(&state, "media.read"));
    let media_quarantine: MethodRouter<AppState, Infallible> =
        get(media_scan::list_held).layer(guards::require(&state, "media.read"));
    let media_quarantine_release: MethodRouter<AppState, Infallible> =
        post(media_scan::release).layer(guards::require(&state, "media.scan.manage"));
    let media_scan_test: MethodRouter<AppState, Infallible> =
        post(media_scan::test_scanner).layer(guards::require(&state, "media.scan.manage"));

    // Folder and file grants (REQ-010, slice 4). Reading a grant table and asking what the
    // platform decided for you are both `media.read` — the file browser shows who can see a
    // file, and a person who cannot open one needs to be able to ask *why*. Writing is
    // `media.manage`, deliberately: narrowing somebody out of a folder is a library
    // organisation decision, and a contributor who may upload is not somebody who should be
    // able to decide who else may read what they uploaded.
    let media_folder_grants: MethodRouter<AppState, Infallible> =
        get(media_grants::folder_grants).layer(guards::require(&state, "media.read"));
    let media_folder_grant_write: MethodRouter<AppState, Infallible> =
        put(media_grants::put_folder_grant).layer(guards::require(&state, "media.manage"));
    let media_file_grants: MethodRouter<AppState, Infallible> =
        get(media_grants::file_grants).layer(guards::require(&state, "media.read"));
    let media_file_grant_write: MethodRouter<AppState, Infallible> =
        put(media_grants::put_file_grant).layer(guards::require(&state, "media.manage"));
    // A grant is removed by its own id alone — the row knows the node it was written on, so
    // putting the node in the URL as well would make a two-parameter path with a one-parameter
    // handler, which axum rejects with a bare `500` and no body. `grant-subjects` and this are
    // both *static* segments under `/media/`, so they rank ahead of `/media/{id}/…` and are
    // never read as a media id.
    let media_grant_delete: MethodRouter<AppState, Infallible> =
        delete(media_grants::delete_one).layer(guards::require(&state, "media.manage"));
    let media_subjects: MethodRouter<AppState, Infallible> =
        get(media_grants::subjects).layer(guards::require(&state, "media.read"));
    let media_grant_effective: MethodRouter<AppState, Infallible> =
        get(media_grants::effective).layer(guards::require(&state, "media.read"));

    // Retention (REQ-010, slice 4). Reading a policy, the run log and the trash's own numbers
    // is `media.read` — a person who cannot change a window must still be able to ask what the
    // site promises to keep, because "my file was deleted by a policy" is exactly that
    // question. Writing a policy, running a sweep, setting a hold and repairing references are
    // `media.settings.manage`, the key slice 3 already gave the storage screen: retention is
    // the destructive half of the same screen, so it is the same key.
    // Backup centre (REQ-013). Four keys, and the split is the point: reading the list is
    // `backup.read`, taking one and verifying one is `backup.create`, and schedules,
    // retention and settings are `backup.manage` — which deliberately does NOT include
    // `backup.restore`, so the schedule editor cannot overwrite live content.
    let backups_read: MethodRouter<AppState, Infallible> =
        get(backups::list).layer(guards::require(&state, "backup.read"));
    let backups_create: MethodRouter<AppState, Infallible> =
        post(backups::create).layer(guards::require(&state, "backup.create"));
    let backups_status: MethodRouter<AppState, Infallible> =
        get(backups::status).layer(guards::require(&state, "backup.read"));
    let backups_detail: MethodRouter<AppState, Infallible> =
        get(backups::detail).layer(guards::require(&state, "backup.read"));
    let backups_manifest: MethodRouter<AppState, Infallible> =
        get(backups::manifest).layer(guards::require(&state, "backup.read"));
    let backups_verify: MethodRouter<AppState, Infallible> =
        post(backups::verify).layer(guards::require(&state, "backup.create"));
    let backups_delete: MethodRouter<AppState, Infallible> =
        delete(backups::delete).layer(guards::require(&state, "backup.manage"));
    // The manual retention sweep. `backup.manage`, not `backup.create`: this removes data,
    // and the key that lets an operator take a backup is not the key that lets one remove it.
    // The preview is a GET that writes nothing, so it sits under `backup.read`: reading a
    // warning is free and non-destructive, and gating it behind `backup.restore` would mean
    // the first time an operator meets this screen is a 403 that never showed them what
    // they were agreeing to.
    let backups_restore_preview: MethodRouter<AppState, Infallible> =
        get(backups::restore_preview).layer(guards::require(&state, "backup.read"));
    // The destructive call, and the only route in this file behind `backup.restore`. The
    // preview stays under `backup.read` because reading a warning changes nothing; pressing
    // the button overwrites live data, so it needs the key that means that. A platform
    // where the schedule editor can also overwrite content is one where the nightly job and
    // an operator's button are the same authority.
    let backups_restore: MethodRouter<AppState, Infallible> =
        post(backups::restore).layer(guards::require(&state, "backup.restore"));
    let backups_sweep: MethodRouter<AppState, Infallible> =
        post(backups::sweep).layer(guards::require(&state, "backup.manage"));
    // Slice 2c. Three routes, and the key split is the point: queueing and cancelling are
    // `backup.restore` (the same authority as pressing the button — a permission that lets an
    // operator *un*press something they could never press is a way to deny the service to
    // somebody who can only see the job), while *listing* the jobs is `backup.read`, because
    // seeing that a restore is queued changes nothing.
    let backups_restore_queue: MethodRouter<AppState, Infallible> =
        post(restore_jobs::queue_restore).layer(guards::require(&state, "backup.restore"));
    let backups_restore_jobs: MethodRouter<AppState, Infallible> =
        get(restore_jobs::list_jobs).layer(guards::require(&state, "backup.read"));
    let restore_job_cancel: MethodRouter<AppState, Infallible> =
        post(restore_jobs::cancel_job).layer(guards::require(&state, "backup.restore"));
    let backup_schedules_read: MethodRouter<AppState, Infallible> =
        get(backups::list_schedules).layer(guards::require(&state, "backup.read"));
    // Editing a schedule is `backup.manage`, the same key as the settings screen: both are
    // unattended decisions about what the platform will do on its own at 02:00. And "run now"
    // is `backup.create`, NOT `backup.manage` — pressing it produces a backup and changes
    // nothing else, so it is the same power as the drawer's own button. An operator who may
    // take a backup must be able to test that their schedule works.
    let backup_schedules_write: MethodRouter<AppState, Infallible> =
        post(backups::create_schedule).layer(guards::require(&state, "backup.manage"));
    let backup_schedule: MethodRouter<AppState, Infallible> =
        put(backups::update_schedule).layer(guards::require(&state, "backup.manage"));
    let backup_schedule_delete: MethodRouter<AppState, Infallible> =
        delete(backups::delete_schedule).layer(guards::require(&state, "backup.manage"));
    let backup_schedule_run: MethodRouter<AppState, Infallible> =
        post(backups::run_schedule_now).layer(guards::require(&state, "backup.create"));
    let backup_settings_read: MethodRouter<AppState, Infallible> =
        get(backups::read_settings).layer(guards::require(&state, "backup.read"));
    let backup_settings_write: MethodRouter<AppState, Infallible> =
        put(backups::write_settings).layer(guards::require(&state, "backup.manage"));

    let media_retention: MethodRouter<AppState, Infallible> =
        get(media_retention::read).layer(guards::require(&state, "media.read"));
    let media_retention_create: MethodRouter<AppState, Infallible> =
        post(media_retention::create).layer(guards::require(&state, "media.settings.manage"));
    let media_retention_update: MethodRouter<AppState, Infallible> =
        put(media_retention::update).layer(guards::require(&state, "media.settings.manage"));
    let media_retention_delete: MethodRouter<AppState, Infallible> =
        delete(media_retention::delete).layer(guards::require(&state, "media.settings.manage"));
    let media_retention_run: MethodRouter<AppState, Infallible> =
        post(media_retention::run_now).layer(guards::require(&state, "media.settings.manage"));
    let media_retention_runs: MethodRouter<AppState, Infallible> =
        get(media_retention::runs).layer(guards::require(&state, "media.read"));
    let media_retention_repair: MethodRouter<AppState, Infallible> =
        post(media_retention::repair).layer(guards::require(&state, "media.settings.manage"));
    let media_file_hold: MethodRouter<AppState, Infallible> =
        put(media_retention::set_file_hold).layer(guards::require(&state, "media.settings.manage"));

    // Usage and activity (REQ-010, slice 4). Both reads are `media.read` on purpose: where a
    // file is used and what has been done to it are the same power as opening it, and neither
    // list contains anything the caller could not already read from the file itself. A separate
    // key would be a second opinion rather than a boundary — and the reader who most needs the
    // trail is the one with the fewest keys.
    let media_references: MethodRouter<AppState, Infallible> =
        get(media_usage::references).layer(guards::require(&state, "media.read"));
    let media_activity: MethodRouter<AppState, Infallible> =
        get(media_usage::activity).layer(guards::require(&state, "media.read"));

    // Public: the unauthenticated read surface of the site renderer. It serves published
    // content only, so it carries no permission guard — and no mutation can be reached here.
    //
    // The `noindex` layer sits *above* the handler rather than inside it, because "is this
    // address staging" is a fact about the host and not about the status the handler chose
    // (REQ-017 slice 4). A handler marks its own `200` and forgets the `404`; a layer marks
    // whatever leaves, so a staging host is never indexable at one path and indexable at
    // another.
    let public_pages = get(public::get_published_page).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        public::noindex_staging_hosts,
    ));

    // A published page points at its own assets, so the library's read side is public too.
    let public_media = get(media::public_media);

    // Workflows: the definitions and their run history (docs/requests/REQ-003). Reading needs
    // `workflows.read`, writing a definition `workflows.manage`, and starting or cancelling a
    // run `workflows.run`; every handler applies the tenancy scope rule through the workflow's
    // organization.
    let workflows = get(workflows::list_workflows)
        .layer(guards::require(&state, "workflows.read"))
        .merge(post(workflows::create_workflow).layer(guards::require(&state, "workflows.manage")));

    let workflow = get(workflows::get_workflow)
        .layer(guards::require(&state, "workflows.read"))
        .merge(put(workflows::update_workflow).layer(guards::require(&state, "workflows.manage")))
        .merge(
            delete(workflows::delete_workflow).layer(guards::require(&state, "workflows.manage")),
        );

    let workflow_run =
        post(workflows::run_workflow).layer(guards::require(&state, "workflows.run"));

    let workflow_executions =
        get(workflows::list_executions).layer(guards::require(&state, "workflows.read"));

    let workflow_execution =
        get(workflows::get_execution).layer(guards::require(&state, "workflows.read"));

    let workflow_execution_cancel =
        post(workflows::cancel_execution).layer(guards::require(&state, "workflows.run"));

    // Onboarding: the first-run flow (REQ-050). No permission guard — the flow itself decides
    // who may act, and it must be reachable before any account, role or binding exists.
    let onboarding_owner = post(onboarding::create_owner);
    let onboarding_organization = post(onboarding::create_organization);
    let onboarding_site = post(onboarding::create_site);
    let onboarding_theme = post(onboarding::choose_theme);
    let onboarding_ai = post(onboarding::decide_ai);
    let onboarding_complete = post(onboarding::complete);

    // AI Hub (docs/06-AI-HUB.md): connecting a provider is `ai.providers.manage`, reading the
    // registry `ai.providers.read`, and using the chat its own key — so a team can talk to the
    // platform's AI without being able to point it at another endpoint.
    let ai_providers = get(ai::list_providers)
        .layer(guards::require(&state, "ai.providers.read"))
        .merge(post(ai::create_provider).layer(guards::require(&state, "ai.providers.manage")));

    let ai_provider = patch(ai::update_provider)
        .layer(guards::require(&state, "ai.providers.manage"))
        .merge(delete(ai::delete_provider).layer(guards::require(&state, "ai.providers.manage")));

    let ai_provider_models =
        put(ai::replace_provider_models).layer(guards::require(&state, "ai.providers.manage"));

    let ai_provider_discover =
        post(ai::discover_provider_models).layer(guards::require(&state, "ai.providers.manage"));

    let ai_models = get(ai::list_models).layer(guards::require(&state, "ai.providers.read"));

    let ai_model = patch(ai::update_model).layer(guards::require(&state, "ai.providers.manage"));

    let ai_chat = post(ai::chat).layer(guards::require(&state, "ai.chat"));

    // Events and webhooks (docs/01-VISION.md §13, P12): reading the endpoints and their queue
    // history is `webhooks.read`, connecting, changing, testing and removing them is
    // `webhooks.manage`, and the platform's event feed is read with `events.read`. Every
    // handler applies the tenancy scope rule through the endpoint's organization.
    let webhooks = get(webhooks::list_webhooks)
        .layer(guards::require(&state, "webhooks.read"))
        .merge(post(webhooks::create_webhook).layer(guards::require(&state, "webhooks.manage")));

    let webhook = get(webhooks::get_webhook)
        .layer(guards::require(&state, "webhooks.read"))
        .merge(patch(webhooks::update_webhook).layer(guards::require(&state, "webhooks.manage")))
        .merge(delete(webhooks::delete_webhook).layer(guards::require(&state, "webhooks.manage")));

    let webhook_deliveries =
        get(webhooks::list_deliveries).layer(guards::require(&state, "webhooks.read"));

    let webhook_test =
        post(webhooks::test_webhook).layer(guards::require(&state, "webhooks.manage"));

    // The delivery operations (REQ-016 slice 2). Reading an endpoint's history and its summary
    // is the same power as reading the endpoint — the history belongs to it — while sending one
    // again is not: a replay is an outbound request to somebody else's server, so it is
    // `webhooks.manage` and never `webhooks.read`. A read-only auditor must not be able to make
    // the platform POST to a third party by pressing a button.
    let webhook_stats =
        get(webhooks::endpoint_stats).layer(guards::require(&state, "webhooks.read"));

    let webhook_secret_rotate =
        post(webhooks::rotate_secret).layer(guards::require(&state, "webhooks.manage"));

    // Declared before the single-delivery path on purpose: `/deliveries/redeliver` is a literal
    // segment and `/deliveries/{delivery_id}/redeliver` would read `redeliver` as an id if the
    // two were registered the other way round, answering `404 no such delivery` for a request
    // that is perfectly valid.
    let webhook_redeliver_batch =
        post(webhooks::redeliver_many).layer(guards::require(&state, "webhooks.manage"));

    let webhook_redeliver_one =
        post(webhooks::redeliver_one).layer(guards::require(&state, "webhooks.manage"));

    let events = get(webhooks::list_events).layer(guards::require(&state, "events.read"));

    // The catalogue is the platform's own registry of event names (REQ-016 slice 1), read with
    // the same key as the feed: describing what an event means is reading the bus, not
    // administering an endpoint. It is a sibling of `/events`, not a child, so the literal
    // `catalogue` segment can never be read as an event id.
    let event_catalogue =
        get(webhooks::list_catalogue).layer(guards::require(&state, "events.read"));

    // The event bus's own retention (REQ-016 slice 3). Reading the window, the counts and the
    // last sweep is reading the bus, so it rides `events.read`; **changing** the window and
    // running a sweep are `webhooks.manage`, because shortening a window destroys an audit
    // trail and a read-only auditor must not be able to trigger that from a link.
    //
    // Declared before `/events/{id}` for the same reason `/events/catalogue` is: `retention`
    // is a literal segment, and a parameterised sibling registered first would read it as an
    // event id and answer `404 no such event` for a request that is perfectly valid.
    let event_retention = get(webhooks::retention_status)
        .layer(guards::require(&state, "events.read"))
        // The PATCH rides the same router as the GET because the two are one read/write pair on
        // one path; a separate `patch(...)` bound to `/events/retention` would need a second
        // `.route()` line and axum panics at boot when a path carries two `MethodRouter`s from
        // different calls. The guards differ, and that is fine: the GET's layer answers the
        // GET and the PATCH's layer answers the PATCH, and a caller holding only `events.read`
        // reaches the GET and is refused on the PATCH — which is exactly the split the two
        // powers are for.
        .merge(patch(webhooks::set_retention).layer(guards::require(&state, "webhooks.manage")));
    let event_retention_sweep =
        post(webhooks::sweep_retention).layer(guards::require(&state, "webhooks.manage"));

    // Automations (docs/requests/REQ-003, P13): a rule is an event-triggered workflow, so its
    // read and write powers are the workflow keys the engine already defines — being allowed to
    // define an automation and being allowed to run it are the same two powers a workflow
    // carries. The handler checks the tenancy scope through the rule's organization.
    let automations = get(automation::list_automations)
        .layer(guards::require(&state, "workflows.read"))
        .merge(
            post(automation::create_automation).layer(guards::require(&state, "workflows.manage")),
        );

    let automation_catalogue =
        get(automation::get_catalogue).layer(guards::require(&state, "workflows.read"));

    let automation_entry = get(automation::get_automation)
        .layer(guards::require(&state, "workflows.read"))
        .merge(
            put(automation::update_automation).layer(guards::require(&state, "workflows.manage")),
        )
        .merge(
            delete(automation::delete_automation)
                .layer(guards::require(&state, "workflows.manage")),
        );

    // The CDN / edge surface (docs/requests/REQ-011, slice 1): reading rules and settings is
    // `cdn.read`, writing a rule is `cdn.manage`, and invalidating cached content is the
    // separate `cdn.purge` — a blunt act an operator may grant without letting the same
    // account rewrite the policy that decides what may be cached. The purge tables
    // themselves are slice 2; only the rules and settings ship here.
    let cdn_rules = get(cdn::list_rules)
        .layer(guards::require(&state, "cdn.read"))
        .merge(post(cdn::create_rule).layer(guards::require(&state, "cdn.manage")));
    let cdn_rule = get(cdn::get_rule)
        .layer(guards::require(&state, "cdn.read"))
        .merge(put(cdn::update_rule).layer(guards::require(&state, "cdn.manage")))
        .merge(delete(cdn::delete_rule).layer(guards::require(&state, "cdn.manage")));
    // Reorder is its own path, not a second `POST` merged onto `/cdn/rules`. Two handlers
    // for the same method on one path is not an ambiguous route to axum — it is a panic
    // while the router is *built*, which takes down every route in the application, not
    // just this one. It read as a working feature because the API tests had never been
    // run against this router.
    let cdn_rule_reorder = post(cdn::reorder_rules).layer(guards::require(&state, "cdn.manage"));
    let cdn_rule_toggle = post(cdn::toggle_rule).layer(guards::require(&state, "cdn.manage"));

    // The purge surface (REQ-011, slice 2). Three keys, deliberately: reading what the
    // cache did is `cdn.read`, asking for an invalidation is `cdn.purge`, and making a
    // failed one run again is `cdn.manage`. Merging them would let anyone who can fix a
    // typo in a cache rule also flush an entire production zone.
    let cdn_status = get(cdn_purge::status).layer(guards::require(&state, "cdn.read"));
    let cdn_purges = get(cdn_purge::list_purges)
        .layer(guards::require(&state, "cdn.read"))
        .merge(post(cdn_purge::create_purge).layer(guards::require(&state, "cdn.purge")));
    let cdn_purge_one = get(cdn_purge::get_purge).layer(guards::require(&state, "cdn.read"));
    let cdn_purge_retry = post(cdn_purge::retry_purge).layer(guards::require(&state, "cdn.manage"));
    // Settings and the adapter catalogue. These are read by the provider screen that
    // shipped with slice 1, so leaving them unregistered would have made two of its three
    // fetches 404 — a screen that loads, renders an error and has no way to say so.
    let cdn_settings = get(cdn_purge::get_settings)
        .layer(guards::require(&state, "cdn.read"))
        .merge(put(cdn_purge::put_settings).layer(guards::require(&state, "cdn.manage")));
    let cdn_settings_test =
        post(cdn_purge::test_settings).layer(guards::require(&state, "cdn.manage"));
    let cdn_adapters = get(cdn_purge::adapters).layer(guards::require(&state, "cdn.read"));

    // The developer platform (docs/requests/REQ-033, slice 1). Two keys and the split is the
    // point: reading a key's metadata is an auditor's question, minting one is a much larger
    // power, so a role that can see which integrations exist cannot thereby add another.
    //
    // `/api-keys` merges a GET and a POST, and `/api-keys/{id}` merges a GET and a DELETE, for
    // the reason `/events/retention` already does: one path with one `MethodRouter` per verb.
    // Rotate is a **sub-path** rather than a second `POST` on `/api-keys/{id}` — two handlers
    // for the same method on one path is not an ambiguous route to axum, it is a panic while the
    // router is *built*, which takes down every route in the application. The REQ-024 wizard
    // shipped with that bug for one tick and 339 passing tests did not see it.
    let api_keys = get(developer::list_keys)
        .layer(guards::require(&state, "developer.keys.read"))
        .merge(
            post(developer::create_key).layer(guards::require(&state, "developer.keys.manage")),
        );
    let api_key_one = get(developer::get_key)
        .layer(guards::require(&state, "developer.keys.read"))
        .merge(
            delete(developer::revoke_key).layer(guards::require(&state, "developer.keys.manage")),
        );
    let api_key_rotate = post(developer::rotate_key)
        .layer(guards::require(&state, "developer.keys.manage"));
    // The request log rides the read key — it is the same question as the key list, answered one
    // row at a time.
    let request_logs =
        get(developer::list_request_logs).layer(guards::require(&state, "developer.keys.read"));
    // The single-row read is its own path for the same reason `/events/catalogue` is a literal
    // sibling rather than a child: `{id}` registered first would read `catalogue` as an id. Here
    // the ids are numbers and the paths do not collide, but the drawer and the page are
    // different shapes and a `GET` cannot answer both from one route.
    let request_log_one =
        get(developer::get_request_log).layer(guards::require(&state, "developer.keys.read"));

    // The deployment centre (REQ-024, slice 1). Six reads and one write, and the write is
    // `deployment.manage` because "check for updates now" reaches out to the network and
    // rewrites the release cache — it is not a read even though it answers a question.
    //
    // `deployment.read` covers all six, rather than one key per surface: every value they
    // return is already visible on the environment cards, which the same key gates. A finer
    // split would create a key that means "may see what is installed" and a second that means
    // "may see what could be installed", and the second is the one an operator needs to compare
    // a release's notes against the running core — so it would be the key nobody holds.
    let deployment_version =
        get(deployment::get_version).layer(guards::require(&state, "deployment.read"));
    let deployment_environments =
        get(deployment::list_environments).layer(guards::require(&state, "deployment.read"));
    let deployment_environment_one =
        get(deployment::get_environment).layer(guards::require(&state, "deployment.read"));
    let deployment_releases =
        get(deployment::list_releases).layer(guards::require(&state, "deployment.read"));
    let deployment_release_one =
        get(deployment::get_release).layer(guards::require(&state, "deployment.read"));
    let deployment_history =
        get(deployment::list_history).layer(guards::require(&state, "deployment.read"));
    let deployment_checks =
        get(deployment::get_checks).layer(guards::require(&state, "deployment.read"));
    // The one write in slice 1. Named `checks/run` rather than merged onto the checks route so
    // a panel that links to `/deployment/checks` can never turn a GET into a network call.
    let deployment_checks_run =
        post(deployment::run_check_now).layer(guards::require(&state, "deployment.manage"));

    // The deploy wizard (REQ-024, slice 2). Pre-flight and deploy are two POSTs, and they are
    // **sub-paths** rather than two methods on `/deployment/environments/{environment}`: a path
    // carrying two POSTs is not a thing axum can merge, and asking it to panics at startup —
    // `Overlapping method route`, which `cargo check` cannot see because the conflict is built at
    // runtime. The client has always called `/preflight` and `/deploy` (apps/admin/lib/api.ts), and
    // the handlers have always documented those paths; only the mount disagreed. So the
    // environment route is a plain GET again and each write owns a segment — the same rule
    // `/deployment/cluster/{environment}/restart` already followed.
    let deployment_environment_deploy = post(deployment_run::start_deploy)
        .layer(guards::require(&state, "deployment.manage"));
    // The job routes. Reading a job and its log is `deployment.read` — they return the same
    // values the history screen already shows — while cancel is `deployment.manage`, because
    // stopping a run is an action on the environment, not a read of it.
    let deployment_job = get(deployment_run::get_job)
        .layer(guards::require(&state, "deployment.read"))
        .merge(
            post(deployment_run::cancel_job).layer(guards::require(&state, "deployment.manage")),
        );
    // The log, as a stream and as a cursor poll. One key, one route, one handler: the two shapes
    // answer the same URL and differ only by `?cursor=`, so they are dispatched inside
    // `deployment_run::log` rather than registered as two GETs on one segment.
    let deployment_job_log =
        get(deployment_run::log).layer(guards::require(&state, "deployment.read"));

    // Rollback and the maintenance window (REQ-024, slice 3).
    //
    // Rollback is `deployment.rollback` — its own key, not `deployment.manage`, because the
    // request's permission table says so and because the two deserve different answers when
    // they are missing: an operator who may deploy but not roll back is a normal configuration
    // (they fix forward), and an operator who may roll back but not deploy is another (they
    // handle incidents). One key for both would make one of those configurations impossible.
    let deployment_rollback =
        post(deployment_ops::start_rollback).layer(guards::require(&state, "deployment.rollback"));
    // The window is read by every admin session (the shell banner) and written by
    // `deployment.maintenance`, which is a *different* key from `deployment.manage` on purpose:
    // opening a maintenance window is what an operator does *instead of* deploying, and
    // requiring the deploy key to stop the world would mean the only people allowed to pause a
    // release are the ones allowed to start one.
    let deployment_maintenance =
        get(deployment_ops::list_maintenance).layer(guards::require(&state, "deployment.read"));

    // The cluster panel (REQ-024, slice 4). The read is `deployment.cluster.read` and not
    // `deployment.read` on purpose: it is a *different kind* of information — host-level replica
    // and resource numbers, read from the runtime rather than from this database — and an
    // account allowed to read a deploy's history is not automatically an account whose job is to
    // see how much CPU the cluster is using. The two writes are `deployment.manage`, the same key
    // as "check for updates now", because both reach out of the database and change something
    // outside it.
    let deployment_cluster = get(deployment_cluster::get_cluster)
        .layer(guards::require(&state, "deployment.cluster.read"));
    let deployment_cluster_samples = get(deployment_cluster::get_samples)
        .layer(guards::require(&state, "deployment.cluster.read"));
    let deployment_cluster_restart = post(deployment_cluster::restart_workload)
        .layer(guards::require(&state, "deployment.manage"));
    let deployment_cluster_sample =
        post(deployment_cluster::run_sample).layer(guards::require(&state, "deployment.manage"));
    let deployment_maintenance_one = put(deployment_ops::update_maintenance)
        .layer(guards::require(&state, "deployment.maintenance"));

    // Staging environments (REQ-017). Reading the list and one environment is `deployment.read`;
    // creating one, re-cloning it and cancelling a clone is `deployment.preview`; archiving one
    // is `deployment.rollback`. Three keys rather than one, because looking at a staging copy,
    // filling it and throwing it away are three different amounts of trust.
    let environments = get(environments::list_environments)
        .layer(guards::require(&state, "deployment.read"))
        .merge(
            post(environments::create_environment)
                .layer(guards::require(&state, "deployment.preview")),
        );
    let environment_one = get(environments::get_environment)
        .layer(guards::require(&state, "deployment.read"))
        .merge(
            delete(environments::archive_environment)
                .layer(guards::require(&state, "deployment.rollback")),
        );
    // Re-clone is its own path (the request's own API table puts it at `/clone`), so it gets a
    // POST-only router rather than being merged onto `/environments/{id}`.
    let environment_one_clone =
        post(environments::start_clone).layer(guards::require(&state, "deployment.preview"));
    let environment_jobs =
        get(environments::list_clone_jobs).layer(guards::require(&state, "deployment.read"));
    // Cancel is its own path rather than a merged `POST` on the job collection: it acts on one
    // job and is destructive, and a collection-level POST that cancels "the current one" is a
    // route whose meaning depends on state the caller cannot see.
    let environment_job_cancel =
        post(environments::cancel_clone).layer(guards::require(&state, "deployment.preview"));
    // The change set is a *read*: it says what staging holds that production does not, and every
    // row in it is a row the same caller can already open in the editor. Slice 3's promotion is
    // what writes, and it is guarded separately as `deployment.promote` — so the tab can be read
    // by somebody who can look at a staging copy without being able to push it to production.
    let environment_changes =
        get(environments::list_changes).layer(guards::require(&state, "deployment.read"));

    // Promotions (REQ-017 slice 3). Reading a promotion is `deployment.read`; *requesting* one
    // and withdrawing it is `deployment.preview`, because a request records an intent and changes
    // nothing — an account that fills a staging copy must be able to say "this is ready" without
    // being able to push it. Approving is `deployment.deploy`: the only route in this request that
    // writes to production.
    let environment_promotions = get(promotions::list_promotions)
        .layer(guards::require(&state, "deployment.read"))
        .merge(
            post(promotions::request_promotion)
                .layer(guards::require(&state, "deployment.preview")),
        );
    let promotion_one =
        get(promotions::get_promotion).layer(guards::require(&state, "deployment.read"));
    let promotion_approve =
        post(promotions::approve_promotion).layer(guards::require(&state, "deployment.deploy"));
    let promotion_cancel =
        post(promotions::cancel_promotion).layer(guards::require(&state, "deployment.preview"));

    // Search (docs/requests/REQ-002): the one search box and its index. Searching is
    // `search.read` — the box every signed-in account holds — and the handler narrows the
    // answer to the providers the caller's own read permissions cover; rebuilding the index
    // is the separate `search.manage`. See `crate::routes::search`.
    let search_route = get(search::search).layer(guards::require(&state, "search.read"));
    let search_suggest = get(search::suggest).layer(guards::require(&state, "search.read"));
    let search_status = get(search::status).layer(guards::require(&state, "search.read"));
    let search_reindex = post(search::reindex).layer(guards::require(&state, "search.manage"));
    // Exporting is reading: the file holds exactly the rows the same caller may already see.
    let search_export = get(search::export).layer(guards::require(&state, "search.read"));
    // Reading the settings is `search.read`; changing them is the separate `search.manage`, so
    // the two halves carry their own guards.
    let search_settings_read = get(search::settings).layer(guards::require(&state, "search.read"));
    let search_settings_write =
        put(search::save_settings).layer(guards::require(&state, "search.manage"));
    // The caller's own history: session-scoped by construction — it needs no permission of its
    // own beyond being signed in.
    let search_recent = get(search::recent).merge(delete(search::clear_recent));

    // Command centre (docs/requests/REQ-032): the palette's own surface — the commands the
    // caller may run (projected through their effective permissions), the suggestions for the
    // screen they are on, and their own recents. Reading and writing recents is `search.read`,
    // the box every signed-in account holds; a command's own key is checked when it is recorded.
    // `POST /commands/{id}/run` executes an action command through its owning service and carries
    // no route-level guard on purpose: every command has its own key (`search.manage`,
    // `content.pages.create`, …), so the handler re-checks the one the registry names — and it
    // refuses a navigation command outright, because a command that opens a screen is not a job.
    // See `crate::routes::commands`.
    let commands_route = get(commands::list_commands).layer(guards::require(&state, "search.read"));
    let command_resolve = post(commands::resolve).layer(guards::require(&state, "search.read"));
    let command_context = get(commands::context).layer(guards::require(&state, "search.read"));
    let command_recent = get(commands::recent)
        .merge(post(commands::record))
        .merge(delete(commands::clear))
        .layer(guards::require(&state, "search.read"));
    let command_run = post(commands::run);

    // Notifications (docs/requests/REQ-021, slice 1). Two powers, split by *whose* inbox:
    // `notifications.read` is a person's own (owner-scoped in the store, so it grants nothing
    // about anybody else and belongs to every role), and `notifications.send` writes into
    // *other* people's inboxes — the one worth guarding, because an account that may only
    // notify itself cannot be used to reach the rest of the organization.
    //
    // The static segments are declared before `/notifications/{id}` so axum ranks them ahead
    // of the parameter route — the same reason `/media/settings` is spelled as a literal.
    let notifications_list =
        get(notifications::list).layer(guards::require(&state, "notifications.read"));
    let notifications_summary =
        get(notifications::summary).layer(guards::require(&state, "notifications.read"));
    let notifications_bulk =
        post(notifications::bulk).layer(guards::require(&state, "notifications.read"));
    let notifications_mark_all =
        post(notifications::mark_all_read).layer(guards::require(&state, "notifications.read"));
    let notifications_emit =
        post(notifications::emit).layer(guards::require(&state, "notifications.send"));
    let notifications_entry = get(notifications::get)
        .layer(guards::require(&state, "notifications.read"))
        .merge(delete(notifications::delete).layer(guards::require(&state, "notifications.read")));
    let notifications_read =
        post(notifications::set_read).layer(guards::require(&state, "notifications.read"));
    // Slice 2's own surface: the reader's own channel configuration, which is a *different*
    // power from reading one's own inbox. `notifications.read` is granted to every role
    // because it grants nothing about anybody else; `notifications.manage` changes what the
    // organization will send this person and how, so it is deliberately absent from the base
    // role and belongs to a person who has been given it on purpose.
    let notifications_preferences = get(notifications::get_preferences)
        .layer(guards::require(&state, "notifications.manage"))
        .merge(
            put(notifications::put_preferences)
                .layer(guards::require(&state, "notifications.manage")),
        );

    // Slice 3 splits by *scope* rather than by action, and the split is the whole point of the
    // slice:
    //
    // * a person's own devices are `notifications.manage` — the same key as their preferences,
    //   because registering a phone is the browser half of "tell me how to reach me";
    // * channel readiness is `notifications.manage` too, for the same reason: it is about the
    //   reader's own matrix;
    // * the outbox and the router's rules are `notifications.admin`, the one key that reads
    //   *anybody's* activity. The outbox shows who was told what and whether it arrived, so
    //   granting it "because somebody can manage notifications" would be the quiet widening
    //   this platform cannot audit later.
    //
    // The static segments are declared before `/notifications/{id}` so axum ranks them ahead of
    // the parameter route — the same reason `/media/settings` is spelled as a literal.
    let notifications_push = Router::new()
        .route(
            "/notifications/push-subscriptions",
            post(notifications_admin::register_push).merge(get(notifications_admin::list_push)),
        )
        .route(
            "/notifications/push-subscriptions/{id}",
            delete(notifications_admin::remove_push),
        )
        .route_layer(guards::require(&state, "notifications.manage"));
    let notifications_channels =
        get(notifications_admin::channels).layer(guards::require(&state, "notifications.manage"));
    // The installation's VAPID public key: what a browser subscribes with. `notifications.manage`
    // for the same reason the device list is — a person who can manage their own notifications
    // needs the key to register the browser they are sitting in front of, and the key is public
    // by definition (it is the half the push service sees). Declared beside `channels` and
    // before the `{id}` routes so the literal segment wins the rank.
    let notifications_push_key =
        get(notifications_admin::push_key).layer(guards::require(&state, "notifications.manage"));
    // The settings screen's per-channel `Test delivery`. Declared next to the other
    // `notifications.manage` surface and, like `preferences` above, before the `{id}` routes:
    // `POST /notifications/preferences/test` is two static segments, and axum ranks static
    // ahead of parameter, so the order only matters as a promise that the literal keeps
    // winning. Guarded by the same key as the preferences it tests.
    let notifications_test = post(notifications_test::test_delivery)
        .layer(guards::require(&state, "notifications.manage"));
    let notifications_outbox = Router::new()
        .route(
            "/notifications/outbox",
            get(notifications_admin::list_outbox),
        )
        .route(
            "/notifications/outbox/{id}/retry",
            post(notifications_admin::retry_outbox),
        )
        .route_layer(guards::require(&state, "notifications.admin"));
    let notifications_routes = Router::new()
        .route(
            "/notifications/routes",
            get(notifications_admin::list_routes).merge(post(notifications_admin::create_route)),
        )
        .route(
            "/notifications/routes/{id}",
            delete(notifications_admin::delete_route),
        )
        // Running one event through the router is an administrator's *proof*, not a feature:
        // the claim of slice 3 is that a bus fact becomes a notification with no direct call
        // between the two modules, and this is the only way to show that from a browser.
        .route("/notifications/route", post(notifications_admin::run_route))
        .route_layer(guards::require(&state, "notifications.admin"));

    // Analytics (docs/requests/REQ-007): reading a site's tracking settings and its snippet is
    // `analytics.read`, changing them is the separate `analytics.settings.manage`, and both
    // resolve the site through the caller's own organization. The collection endpoint is the
    // public half — a site's own script posts beacons to it — so it carries no guard; the body
    // cap below is the one limit the router itself enforces.
    let analytics_settings_read =
        get(analytics::get_settings).layer(guards::require(&state, "analytics.read"));
    let analytics_settings_write =
        put(analytics::put_settings).layer(guards::require(&state, "analytics.settings.manage"));
    let analytics_snippet =
        get(analytics::snippet).layer(guards::require(&state, "analytics.read"));

    // The reports (docs/requests/REQ-007, slice 2): reading them is `analytics.read`, and taking
    // one out as a file is the separate `analytics.export` — a screen that may read a report and
    // an account that may walk away with the data are two different powers. The page series rides
    // with the read key: it is one page's numbers, nothing more than the table already shows.
    // The security centre (REQ-012, slice 1) is its OWN router, and it is no longer a child
    // of the analytics reports router it was written inside.
    //
    // **What the nesting cost.** `analytics_reports` ends in
    // `route_layer(guards::require(&state, "analytics.read"))`, and a `route_layer` applies to
    // every route declared on that router *including the ones declared above it in the same
    // builder*. So `/security/overview` — which declares its own, correct `security.read` guard
    // on the handler — additionally required `analytics.read`. An account holding `security.read`
    // and nothing else was refused with `403 this action requires the "analytics.read"
    // permission`, on the screen whose entire purpose is to be readable by the person doing the
    // diagnosing. A deployment that granted the least would have found the security centre
    // unreadable, and the natural response to that is to grant more.
    //
    // **Why it is invisible.** Every walk that read the posture screen signed in as an account
    // holding *both* keys — the platform owner's role is granted everything, and a walk that
    // also touches analytics needs them. A guard that is only ever satisfied is not a guard that
    // was checked. The only way to see it is an account that holds one key and refuses the
    // other, which is the account the backup walk that found this signs in as: `security.read`
    // and no backup key at all, being the operator who is told their backups are stale and
    // cannot take one.
    //
    // **The rule this re-establishes, for the next group added here.** A `route_layer` is a
    // property of the router it is written on, and it reaches every route that router declares —
    // including routes a later commit appends above the analytics block by accident. A new group
    // gets its own `let ... = Router::new()` and its own `merge`, or it inherits a key that has
    // nothing to do with it. Nesting routers is how the security centre ended up behind a key
    // named for a different feature.
    let security_reports = Router::new()
        // Security centre (docs/requests/REQ-012, slice 1). The split is by *power*, not by
        // verb: `security.read` sees the posture and the findings, `security.scan` re-runs the
        // checks and ingests a report, and `security.manage` changes a finding's status.
        //
        // `security.scan` is a separate key from `security.manage` on purpose. Re-running the
        // checks is read-only in effect — it changes no configuration — while acknowledging or
        // ignoring a finding is a decision somebody will later be asked to justify, and a
        // deployment that grants both lets an account that can only look also dismiss what it
        // saw. The static segments come first so axum ranks them ahead of
        // `/security/findings/{id}`.
        // System health (REQ-014, slice 1). Two keys, and the split is the one the request
        // draws: seeing that a dependency is unhappy is `health.read`, and everything that
        // *writes* is `health.manage`.
        //
        // `POST /health/checks/run` rides `health.manage` rather than `health.read` even
        // though it "only runs probes", because it is a mutation: it records a sample per
        // metric. An account that could trigger a run on demand could fill the retention
        // window with rows of its own choosing, one press at a time, and the trends would
        // become a fiction nobody could audit. Reading a status screen and *causing* the
        // platform to record something are different powers.
        //
        // `/healthz` and `/readyz` are NOT here and must not be: they stay unversioned and
        // unguarded so an orchestrator's probe never depends on a session or a permission
        // (see `crate::routes::health` and `crate::routes::readyz`).
        .route(
            "/health/overview",
            get(health_panel::overview).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/checks/run",
            post(health_panel::run_checks).layer(guards::require(&state, "health.manage")),
        )
        .route(
            "/health/services/{key}",
            get(health_panel::service).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/samples",
            get(health_panel::samples).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/host",
            get(health_panel::host_metrics).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/summary",
            get(health_panel::summary).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/metrics",
            get(health_panel::metrics).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/metrics.csv",
            get(health_panel::metrics_csv).layer(guards::require(&state, "health.read")),
        )
        // Pruning is destructive and irreversible, so it is a POST behind the managing key
        // and not a side effect of a settings save.
        .route(
            "/health/maintenance/prune",
            post(health_panel::prune).layer(guards::require(&state, "health.manage")),
        )
        // -------------------------------------------------------------------------------------
        // Incidents and threshold policy (REQ-014 slice 3).
        //
        // Every write here is `health.manage` and every read is `health.read`, which is why
        // the two live on separately-built method routers that get `.merge()`d: axum applies
        // `.layer()` to the routers it is chained onto, so a single `route_layer` over a path
        // that serves both a GET and a PATCH would demand the *managing* key from the reader
        // who only opens an incident to read it. `guards::require` resolves its name from the
        // permission catalogue, so both keys must exist there (`crates/permissions`).
        // -------------------------------------------------------------------------------------
        .route(
            "/health/incidents",
            get(health_incidents::incidents).layer(guards::require(&state, "health.read")),
        )
        .route(
            "/health/incidents/{id}",
            get(health_incidents::incident)
                .layer(guards::require(&state, "health.read"))
                .merge(
                    patch(health_incidents::patch_incident)
                        .layer(guards::require(&state, "health.manage")),
                ),
        )
        // Settings split the same way: `GET` shows the policy, `PUT` changes it. A single
        // route cannot, because the reader is exactly the person who should see *which*
        // thresholds are configured without being able to rewrite them.
        .route(
            "/health/settings",
            get(health_incidents::get_settings)
                .layer(guards::require(&state, "health.read"))
                .merge(
                    put(health_incidents::put_settings)
                        .layer(guards::require(&state, "health.manage")),
                ),
        )
        .route(
            "/health/maintenance-windows",
            get(health_incidents::list_windows)
                .layer(guards::require(&state, "health.read"))
                // Creating a window is a write even though it only *suppresses* alerts: an
                // operator who can silence a whole service has to be the operator who can
                // change its thresholds, or the screen is a mute button for anyone with a
                // login.
                .merge(
                    post(health_incidents::create_window)
                        .layer(guards::require(&state, "health.manage")),
                ),
        )
        .route(
            "/health/maintenance-windows/{id}",
            delete(health_incidents::delete_window).layer(guards::require(&state, "health.manage")),
        )
        .route(
            "/security/overview",
            get(security::overview).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/checks/run",
            post(security::run_checks).layer(guards::require(&state, "security.scan")),
        )
        .route(
            "/security/findings/import",
            post(security::import).layer(guards::require(&state, "security.scan")),
        )
        .route(
            "/security/findings/bulk",
            post(security::bulk).layer(guards::require(&state, "security.manage")),
        )
        .route(
            "/security/findings.csv",
            get(security::export).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/findings",
            get(security::list).layer(guards::require(&state, "security.read")),
        )
        // Header policy (REQ-012, slice 2). Reading the policy is `security.read` — it is the
        // same read the overview's CSP row already makes. Changing it is `security.manage`, the
        // same power that dismisses a finding, because the two are the same decision: an
        // operator who can weaken the response headers can also make the findings stop
        // mattering. A deployment that split them would let an account that can only look
        // quietly un-look.
        .route(
            "/security/headers",
            get(security_headers::get).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/headers",
            put(security_headers::put).layer(guards::require(&state, "security.manage")),
        )
        // Rate limiting and sign-in protection (REQ-012, slice 3).
        //
        // Reading either document is `security.read` — the same read the overview already makes,
        // and a deployment where a viewer could not see its own limits would make the screen
        // useless to the person diagnosing a refusal. Writing is `security.manage`, the same
        // power that dismisses a finding, because raising a limit until nothing is refused is
        // the same act as making the refusals stop mattering.
        //
        // The tester is `security.read`, not `security.scan`: it changes nothing, and it is the
        // screen an operator has open at 3am with a client being refused. Requiring a write power
        // to *look* at why something was refused would make the screen unusable exactly when it
        // is needed.
        .route(
            "/security/rate-limits",
            get(security_limiter::get_rate_limits)
                .layer(guards::require(&state, "security.read"))
                .merge(
                    put(security_limiter::put_rate_limits)
                        .layer(guards::require(&state, "security.manage")),
                ),
        )
        .route(
            "/security/rate-limits/test",
            post(security_limiter::test_rate_limit).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/sign-in-protection",
            get(security_limiter::get_sign_in_protection)
                .layer(guards::require(&state, "security.read"))
                .merge(
                    put(security_limiter::put_sign_in_protection)
                        .layer(guards::require(&state, "security.manage")),
                ),
        )
        .route(
            "/security/sign-in-protection/probe",
            post(security_limiter::probe_lockout).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/locked-accounts",
            get(security_limiter::get_locked_accounts)
                .layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/locked-accounts/{user_id}/unlock",
            post(security_limiter::unlock).layer(guards::require(&state, "security.manage")),
        )
        // IP access lists (REQ-012 slice 4). Reading them is `security.read` — the same read the
        // overview's IP-allow-list check already makes, and the same read an operator needs to
        // understand a refusal. Changing them is `security.ip.manage`, its OWN key rather than
        // `security.manage`, because an allow/deny list is the one screen in the centre that can
        // lock every administrator out of the platform at once. Splitting it means holding the
        // "manage findings and settings" power does not silently confer the power to deny the
        // CEO's office — a grant nobody would think twice about.
        .route(
            "/security/ip-rules",
            get(security_ip::get)
                .layer(guards::require(&state, "security.read"))
                .merge(
                    post(security_ip::post).layer(guards::require(&state, "security.ip.manage")),
                ),
        )
        .route(
            "/security/ip-rules/test",
            // The tester changes nothing, so it is `security.read` — the same reasoning as the
            // rate-limit tester: an operator diagnosing a refusal must not need the power to
            // change the policy in order to be told what the policy says.
            post(security_ip::test).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/ip-rules/{id}",
            delete(security_ip::delete).layer(guards::require(&state, "security.ip.manage")),
        )
        // Security-event timeline (REQ-012 slice 4). `security.read` for both, including the
        // CSV: an export changes nothing, and an operator who is allowed to read the trail must
        // be allowed to take it away with them — a separate `security.manage` on the download
        // would make the read-only auditor unable to do the one thing their role exists for.
        //
        // The static `.csv` segment is registered before nothing else on this path (there is no
        // `/security/events/{id}`), and the events route carries no `{id}` for the same reason
        // `/findings/{id}` sits below `/findings/import`: axum ranks a static segment ahead of
        // a parameter, and a `events.csv` served as JSON is a client that has to guess.
        // The secret inventory (REQ-012 slice 4). Read-only by design: management of secrets
        // belongs to the secrets manager request, so there is deliberately no PUT or DELETE
        // here — a screen that can edit a reference invites an operator to believe it can rotate
        // a secret, and rotating one means replacing a value in an environment and redeploying.
        .route(
            "/security/secrets",
            get(security_secrets::get).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/events",
            get(security_events::get).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/events.csv",
            get(security_events::export).layer(guards::require(&state, "security.read")),
        )
        .route(
            "/security/findings/{id}",
            get(security::get)
                .layer(guards::require(&state, "security.read"))
                .merge(
                    patch(security::patch_status).layer(guards::require(&state, "security.manage")),
                ),
        );
    let analytics_reports = Router::new()
        .route("/analytics/overview", get(analytics::overview))
        .route("/analytics/pages", get(analytics::pages))
        .route("/analytics/pages/series", get(analytics::page_series))
        .route("/analytics/sources", get(analytics::sources))
        .route("/analytics/audience", get(analytics::audience))
        .route("/analytics/events", get(analytics::events))
        .route("/analytics/events/{name}", get(analytics::event_detail))
        .route("/analytics/downloads", get(analytics::downloads))
        .route("/analytics/forms", get(analytics::forms))
        .route_layer(guards::require(&state, "analytics.read"));

    let analytics_export =
        get(analytics::export).layer(guards::require(&state, "analytics.export"));

    // Goals and realtime (docs/requests/REQ-007, slice 3): reading a goal, its funnel and the
    // realtime snapshot is `analytics.read`, while creating, changing or deleting a goal is the
    // separate `analytics.goals.manage` — an editor who may read the numbers should not be able
    // to silence a conversion by accident.
    let analytics_goals_read = Router::new()
        .route("/analytics/goals", get(analytics::goals_index))
        .route("/analytics/goals/{id}", get(analytics::goal_get))
        .route("/analytics/goals/{id}/funnel", get(analytics::goal_funnel))
        .route("/analytics/realtime", get(analytics::realtime))
        .route(
            "/analytics/realtime/stream",
            get(analytics::realtime_stream),
        )
        .route_layer(guards::require(&state, "analytics.read"));

    let analytics_goals_write = Router::new()
        .route("/analytics/goals", post(analytics::goal_create))
        .route("/analytics/goals/{id}", patch(analytics::goal_patch))
        .route("/analytics/goals/{id}", delete(analytics::goal_delete))
        .route_layer(guards::require(&state, "analytics.goals.manage"));

    // The privacy operations (docs/requests/REQ-007, slice 4): running the retention purge and
    // erasing one visitor change what is stored, which is the settings permission — a reader who
    // may look at the numbers is not the one who decides how long they live.
    let analytics_privacy = Router::new()
        .route("/analytics/purge", post(analytics::purge))
        .route(
            "/analytics/visitors/{hash}",
            delete(analytics::erase_visitor),
        )
        .route_layer(guards::require(&state, "analytics.settings.manage"));

    let analytics_collect = Router::new()
        .route("/public/analytics/collect", post(analytics::collect))
        .layer(DefaultBodyLimit::max(
            omnion_module_analytics::collect::MAX_BODY_BYTES,
        ));

    let v1 = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/sso/providers", sso_providers)
        .route("/auth/sso/{slug}/start", sso_start)
        .route("/auth/sso/{slug}/saml", sso_saml_page)
        .route(
            "/auth/sso/{slug}/callback",
            sso_callback.merge(sso_saml_callback),
        )
        .route("/auth/mfa/verify", auth_mfa_verify)
        .route("/auth/step-up", auth_step_up)
        .route("/auth/webauthn/passkeys", webauthn_passkeys)
        .route("/auth/webauthn/passkeys/{factor_id}", webauthn_passkey)
        .route("/auth/webauthn/register/begin", webauthn_register_begin)
        .route(
            "/auth/webauthn/register/complete",
            webauthn_register_complete,
        )
        .route("/auth/webauthn/authenticate/begin", auth_webauthn_begin)
        .route(
            "/auth/webauthn/authenticate/complete",
            auth_webauthn_complete,
        )
        .route("/me", get(me::me))
        .route("/search", search_route)
        .route("/search/suggest", search_suggest)
        .route("/search/status", search_status)
        .route("/search/reindex", search_reindex)
        .route("/search/export", search_export)
        .route(
            "/search/settings",
            search_settings_read.merge(search_settings_write),
        )
        .route("/search/recent", search_recent)
        .route("/commands", commands_route)
        .route("/commands/{id}/run", command_run)
        .route("/command-center/context", command_context)
        .route("/command-center/resolve", command_resolve)
        .route("/command-center/recent", command_recent)
        // Notifications (REQ-021, slice 1). The static segments (`summary`, `bulk`,
        // `mark-all-read`, `emit`) are declared before the `{id}` routes, which is what makes
        // axum rank them ahead of the parameter route.
        .route("/notifications", notifications_list)
        .route("/notifications/summary", notifications_summary)
        .route("/notifications/bulk", notifications_bulk)
        .route("/notifications/mark-all-read", notifications_mark_all)
        .route("/notifications/emit", notifications_emit)
        // Slice 2. `preferences` is a *literal* segment and is declared before the `{id}`
        // routes for exactly the reason `summary` is above: axum ranks a static segment ahead
        // of a parameter one, and `PUT /notifications/preferences` would otherwise be parsed
        // as a `PUT` on an id called "preferences" — which is a `400` a reader would report
        // as "the settings screen is broken".
        .route("/notifications/preferences", notifications_preferences)
        .route("/notifications/preferences/test", notifications_test)
        // Slice 3's four sub-routers, merged rather than spelled out route by route. Each is a
        // `Router` with its own `route_layer`, so the guard travels with the group and a future
        // fifth endpoint joins the right one by being added inside its block.
        .merge(notifications_push)
        .route("/notifications/channels", notifications_channels)
        .route("/notifications/push-key", notifications_push_key)
        .merge(notifications_outbox)
        .merge(notifications_routes)
        .route("/notifications/{id}", notifications_entry)
        .route("/notifications/{id}/read", notifications_read)
        .route(
            "/analytics/settings",
            analytics_settings_read.merge(analytics_settings_write),
        )
        .route("/analytics/snippet", analytics_snippet)
        .merge(security_reports)
        .merge(analytics_reports)
        .route("/analytics/export", analytics_export)
        .merge(analytics_goals_read)
        .merge(analytics_goals_write)
        .merge(analytics_privacy)
        .merge(analytics_collect)
        .merge(invitation_preview)
        .route(
            "/iam/permissions",
            get(iam::list_permissions).layer(guards::require(&state, "iam.permissions.read")),
        )
        .route("/iam/roles", roles)
        .route("/iam/roles/{id}", role_detail)
        .route("/iam/roles/{id}/versions", role_versions)
        .route("/iam/roles/{id}/members", role_members)
        .route("/iam/roles/{id}/duplicate", role_duplicate)
        .route("/iam/roles/{id}/preview", role_preview)
        .route(
            "/iam/roles/{id}/permissions",
            put(iam::set_role_permissions).layer(guards::require(&state, "iam.roles.manage")),
        )
        .route("/iam/bindings", bindings)
        .route("/iam/bindings/{id}", binding_detail)
        .route("/iam/overview", iam_overview)
        .route("/iam/users", iam_users)
        .route("/iam/users/{id}", iam_user)
        .route("/iam/groups", iam_groups)
        .route("/iam/groups/{id}", iam_group)
        .route("/iam/groups/{id}/members", iam_group_members)
        .route("/iam/service-accounts", iam_service_accounts)
        .route("/iam/service-accounts/{id}", iam_service_account)
        .route("/iam/service-accounts/{id}/keys", iam_service_account_keys)
        .route(
            "/iam/service-accounts/{id}/keys/{key_id}",
            iam_service_account_key,
        )
        .route("/iam/simulations", iam_simulations)
        .route("/iam/policies", iam_policies)
        .route("/iam/policies/{id}", iam_policy)
        .route("/iam/policies/{id}/versions", iam_policy_versions)
        .route("/iam/policies/{id}/test", iam_policy_test)
        .route("/iam/approvals", iam_approvals)
        .route("/iam/approvals/{id}/decide", iam_approval_decide)
        .route("/iam/requests", iam_requests)
        .route("/iam/provisioning/tokens", iam_provisioning_tokens)
        .route("/iam/provisioning/tokens/{id}", iam_provisioning_token)
        .route("/iam/provisioning/log", iam_provisioning_log)
        .route("/iam/providers", iam_providers)
        .route("/iam/providers/{id}", iam_provider)
        .route("/iam/providers/{id}/test", iam_provider_test)
        .route("/iam/providers/{id}/events", iam_provider_events)
        .route("/scim/v2/ServiceProviderConfig", scim_config)
        .route("/scim/v2/Schemas", scim_schemas)
        .route("/scim/v2/Users", scim_users)
        .route("/scim/v2/Users/{id}", scim_user)
        .route("/scim/v2/Groups", scim_groups)
        .route("/scim/v2/Groups/{id}", scim_group)
        .route("/iam/security-policies", iam_security_policy)
        .route("/iam/sessions", iam_sessions)
        .route("/iam/sessions/{id}", iam_session)
        .route("/iam/users/{id}/sign-out-all", iam_sign_out_all)
        .route("/iam/devices", iam_devices)
        .route("/iam/devices/{id}/trust", iam_device_trust)
        .route("/iam/devices/{id}", iam_device)
        .route("/iam/users/{id}/mfa", iam_user_mfa)
        .route(
            "/iam/users/{id}/mfa/{factor_id}/confirm",
            iam_user_mfa_confirm,
        )
        .route("/iam/users/{id}/mfa/{factor_id}", iam_user_factor)
        .route("/iam/users/{id}/reset-mfa", iam_user_mfa_reset)
        .route(
            "/iam/effective-permissions",
            get(iam::effective_permissions),
        )
        .route(
            "/iam/audit",
            get(iam::list_audit).layer(guards::require(&state, "audit.read")),
        )
        .route("/me/organizations", my_organizations)
        .route("/me/organization", switch_organization)
        .route("/organizations", organizations)
        .route("/organizations/{id}", organization)
        .route("/organizations/{id}/members", organization_members)
        .route("/organizations/{id}/members/{user_id}", organization_member)
        .route("/organizations/{id}/invitations", organization_invitations)
        .route("/organizations/{id}/departments", organization_departments)
        .route(
            "/organizations/{id}/departments/{department_id}",
            organization_department,
        )
        .route(
            "/organizations/{id}/departments/{department_id}/members",
            department_members,
        )
        .route(
            "/organizations/{id}/departments/{department_id}/members/{user_id}",
            delete(tenancy_departments::remove_department_member)
                .layer(guards::require(&state, "organizations.manage")),
        )
        .route(
            "/organizations/{id}/departments/{department_id}/roles",
            department_roles,
        )
        .route(
            "/organizations/{id}/departments/{department_id}/roles/{binding_id}",
            delete(tenancy_departments::unbind_department_role)
                .layer(guards::require(&state, "organizations.manage"))
                .layer(guards::require(&state, "iam.bindings.manage")),
        )
        .route(
            "/organizations/{id}/members/{user_id}/departments",
            member_departments,
        )
        .route(
            "/organizations/{id}/members/{user_id}/role-bindings",
            organization_member_role_bindings,
        )
        .route(
            "/organizations/{id}/members/{user_id}/role-bindings/{binding_id}",
            organization_member_role_binding,
        )
        .route("/organizations/{id}/settings", organization_settings)
        .route("/organizations/{id}/modules", organization_modules)
        .route("/organizations/{id}/limits", organization_limits)
        .route("/organizations/{id}/usage", organization_usage)
        .route("/organizations/{id}/audit", organization_audit)
        .route(
            "/organizations/{id}/invitations/{invitation_id}",
            organization_invitation,
        )
        .route(
            "/organizations/{id}/invitations/queue",
            organization_invitation_queue,
        )
        .route(
            "/organizations/{id}/invitations/{invitation_id}/release",
            organization_invitation_approval,
        )
        .route("/sites", sites)
        .route("/sites/{id}", site)
        .route("/sites/{id}/domains", domains)
        .route("/sites/{id}/domains/{domain_id}", domain)
        .route("/sites/{id}/domains/{domain_id}/primary", domain_primary)
        .route("/pages", pages)
        .route("/pages/{id}", page)
        .route("/pages/{id}/publish", page_publish)
        .route("/pages/{id}/restore", page_restore)
        .route("/pages/{id}/revisions", page_revisions)
        .route("/pages/{id}/revisions/{revision_id}", page_revision)
        .route(
            "/pages/{id}/revisions/{revision_id}/translations",
            page_translations,
        )
        .route(
            "/pages/{id}/revisions/{revision_id}/translations/{language}",
            page_translation,
        )
        .route("/media", media)
        .merge(media_upload)
        .route("/media/{id}", media_entry)
        .route("/media/{id}/raw", media_raw)
        // The file manager's own paths. `/media/files` and `/media/folders` sit beside the v0
        // collection route rather than replacing it, so a client written against `{site_id, media}`
        // keeps working while the browser moves to the file system.
        .route("/media/files", media_files_route)
        .route("/media/files/{id}", media_file)
        .route("/media/files/{id}/restore", media_file_restore)
        .route("/media/files/{id}/purge", media_file_purge)
        .route("/media/folders", media_folders)
        .route("/media/folders", media_folder_create)
        .route("/media/folders/{id}", media_folder)
        .route("/media/trash", media_trash)
        .route("/media/trash/empty", media_trash_empty)
        .route("/media/bulk", media_bulk)
        .route("/media/{id}/versions", media_versions)
        .route("/media/{id}/versions", media_version_create)
        .route(
            "/media/{id}/versions/{version}/restore",
            media_version_restore,
        )
        .route("/media/{id}/versions/{version}/raw", media_version_raw)
        // Share links (REQ-010, slice 3). `/media/{id}/shares` is a collection and
        // `/media/{id}/shares/{share_id}` one link, so a revoke addresses a link without
        // touching the rest of the file's links.
        .route("/media/{id}/shares", media_shares_route)
        .route("/media/{id}/shares", media_share_create)
        .route("/media/{id}/shares/revoke-all", media_share_revoke_all)
        .route("/media/{id}/shares/{share_id}", media_share_revoke)
        // Usage and activity (REQ-010, slice 4): the two reads the file-detail screen's last
        // two tabs are made of.
        .route("/media/{id}/references", media_references)
        .route("/media/{id}/activity", media_activity)
        .route("/media/transformation-presets", media_presets)
        // The duplicate report and its merge. `duplicates` is a static segment declared before
        // `/media/{id}/…`, so axum ranks it ahead of the parameter route — same rule the
        // settings row below relies on, and the reason it is a literal here.
        .route("/media/duplicates", media_duplicates_route)
        .route("/media/duplicates/merge", media_duplicates_merge)
        // `/media/settings` is a *static* segment and `/media/{id}/…` is a parameter one.
        // Axum ranks the static match first, so the settings row is never read as a media
        // id — which is why the literal is declared here and not spelled as `{id}`.
        .route("/media/settings", media_settings_route)
        .route("/media/settings", media_settings_write)
        .route("/media/settings/test-connection", media_settings_test)
        // Scanning. `scan-settings`, `scan` and `quarantine` are all *static* segments declared
        // here, so axum ranks them ahead of `/media/{id}/…` — the same reason `/media/settings`
        // is spelled as a literal rather than a parameter.
        .route("/media/scan-settings", media_scan_route)
        .route("/media/scan-settings", media_scan_write)
        .route("/media/scan/test", media_scan_test)
        .route("/media/scan/run", media_scan_run)
        .route("/media/scan/runs", media_scan_runs_route)
        .route("/media/quarantine", media_quarantine)
        .route("/media/quarantine/{id}/release", media_quarantine_release)
        .route("/media/folders/{id}/grants", media_folder_grants)
        .route("/media/grants/{grant_id}", media_grant_delete)
        .route("/media/folders/{id}/grants", media_folder_grant_write)
        .route("/media/{id}/grants", media_file_grants)
        .route("/media/{id}/grants", media_file_grant_write)
        .route("/media/grant-subjects", media_subjects)
        .route("/media/{id}/grant-effective", media_grant_effective)
        // Retention. `retention`, `retention/runs` and `retention/repair` are *static*
        // segments declared here, so axum ranks them ahead of `/media/{id}/…` — the same
        // reason `/media/scan-settings` is spelled as a literal rather than a parameter.
        .route("/media/retention", media_retention)
        .route("/media/retention", media_retention_create)
        .route("/media/retention/runs", media_retention_runs)
        .route("/media/retention/run", media_retention_run)
        .route("/media/retention/repair", media_retention_repair)
        .route("/media/retention/{id}", media_retention_update)
        .route("/media/retention/{id}", media_retention_delete)
        .route("/backups", backups_read)
        .route("/backups", backups_create)
        .route("/backups/status", backups_status)
        // Registered BEFORE `/backups/{id}` and not after it. A `POST` against
        // `/backups/sweep` would otherwise match `{id}` and fail to parse `sweep` as a UUID
        // — a 500 that reads like a router bug on the one route whose whole point is to be
        // callable by hand.
        .route("/backups/sweep", backups_sweep)
        .route("/backups/{id}", backups_detail)
        .route("/backups/{id}", backups_delete)
        .route("/backups/{id}/manifest", backups_manifest)
        .route("/backups/{id}/restore-preview", backups_restore_preview)
        .route("/backups/{id}/restore", backups_restore)
        .route("/backups/{id}/restore-queue", backups_restore_queue)
        .route("/backups/{id}/restore-jobs", backups_restore_jobs)
        // A SEPARATE prefix, not `/backups/{id}/…`, because a cancel is addressed by the
        // JOB's id and not the run's — two different resources, and a route that accepted
        // either would let a cancel for one run's job stop another run's restore.
        .route("/restore-jobs/{id}/cancel", restore_job_cancel)
        .route("/backups/{id}/verify", backups_verify)
        .route("/backup-schedules", backup_schedules_read)
        .route("/backup-schedules", backup_schedules_write)
        // Registered before any `/backup-schedules/{id}` route, and not after it, for the
        // same reason `/backups/sweep` sits above `/backups/{id}`: a `POST` against a
        // non-UUID segment would otherwise match `{id}` and fail to parse it.
        .route("/backup-schedules/{id}", backup_schedule)
        .route("/backup-schedules/{id}", backup_schedule_delete)
        .route("/backup-schedules/{id}/run", backup_schedule_run)
        .route("/backup-settings", backup_settings_read)
        .route("/backup-settings", backup_settings_write)
        // The hold is on a *file*, so it lives under the file rather than under the policy.
        .route("/media/files/{id}/hold", media_file_hold)
        .route("/media/transformation-presets", media_preset_create)
        .route("/media/transformation-presets/{id}", media_preset)
        .route(
            "/media/{id}/versions/{version}/download",
            media_version_download,
        )
        .route("/public/pages/{slug}", public_pages)
        .route("/public/media/{id}", public_media)
        // The share token route: unauthenticated by nature, because the token is the
        // credential. It is a *static* `shared` segment, so it never collides with the
        // `{id}` parameter above it.
        .route("/public/media/shared/{token}", public_media_shared)
        .route("/workflows", workflows)
        .route("/workflows/{id}", workflow)
        .route("/workflows/{id}/run", workflow_run)
        .route("/workflows/{id}/executions", workflow_executions)
        .route("/workflow-executions/{id}", workflow_execution)
        .route(
            "/workflow-executions/{id}/cancel",
            workflow_execution_cancel,
        )
        .route("/onboarding", get(onboarding::status))
        .route("/onboarding/owner", onboarding_owner)
        .route("/onboarding/organization", onboarding_organization)
        .route("/onboarding/site", onboarding_site)
        .route("/onboarding/theme", onboarding_theme)
        .route("/onboarding/ai-provider", onboarding_ai)
        .route("/onboarding/complete", onboarding_complete)
        .route("/ai/providers", ai_providers)
        .route("/ai/providers/{id}", ai_provider)
        .route("/ai/providers/{id}/models", ai_provider_models)
        .route("/ai/providers/{id}/discover-models", ai_provider_discover)
        .route("/ai/models", ai_models)
        .route("/ai/models/{id}", ai_model)
        .route("/ai/chat", ai_chat)
        .route("/cdn/rules", cdn_rules)
        .route("/cdn/rules/reorder", cdn_rule_reorder)
        .route("/cdn/rules/{id}", cdn_rule)
        .route("/cdn/rules/{id}/toggle", cdn_rule_toggle)
        // The developer platform (REQ-033, slice 1). Mounted in the order the literal segments
        // require: `/api-keys/{id}/rotate` is registered before nothing in particular here, but
        // `/request-logs` and `/request-logs/{id}` are two shapes of the same verb, and axum
        // matches the longer literal first only because the parameterised one is a different
        // arity — registering both is safe, registering two *methods* on one is not.
        .route("/api-keys", api_keys)
        .route("/api-keys/{id}", api_key_one)
        .route("/api-keys/{id}/rotate", api_key_rotate)
        .route("/request-logs", request_logs)
        .route("/request-logs/{id}", request_log_one)
        .route("/cdn/status", cdn_status)
        .route("/cdn/purges", cdn_purges)
        .route("/cdn/purges/{id}", cdn_purge_one)
        .route("/cdn/purges/{id}/retry", cdn_purge_retry)
        .route("/cdn/settings", cdn_settings)
        .route("/cdn/settings/test", cdn_settings_test)
        .route("/cdn/adapters", cdn_adapters)
        .route("/deployment/version", deployment_version)
        .route("/deployment/environments", deployment_environments)
        .route(
            "/deployment/environments/{environment}",
            deployment_environment_one,
        )
        // Two POSTs, two segments. Mounted beside the environment route rather than merged onto
        // it: axum panics at startup on a path carrying two of the same method, and the panel has
        // called these sub-paths since slice 2.
        .route(
            "/deployment/environments/{environment}/preflight",
            post(deployment_run::preflight)
                .layer(guards::require(&state, "deployment.manage")),
        )
        .route(
            "/deployment/environments/{environment}/deploy",
            deployment_environment_deploy,
        )
        .route("/deployment/releases", deployment_releases)
        .route("/deployment/releases/{version}", deployment_release_one)
        .route("/deployment/history", deployment_history)
        .route("/deployment/checks", deployment_checks)
        .route("/deployment/checks/run", deployment_checks_run)
        .route("/deployment/jobs/{id}", deployment_job)
        .route("/deployment/jobs/{id}/log", deployment_job_log)
        .route(
            "/deployment/environments/{environment}/rollback",
            deployment_rollback,
        )
        .route("/deployment/maintenance", deployment_maintenance)
        // The per-environment window is a `PUT` on its own path rather than a body field on the
        // list route: the shell banner polls the list in every session, and a poll that could
        // change a window would be a `GET` with a side effect.
        .route(
            "/deployment/maintenance/{environment}",
            deployment_maintenance_one,
        )
        // `/deployment/cluster` answers `404` on a single instance rather than an empty cluster,
        // so there is no disabled card to tease with. The samples and the restart hang off it
        // rather than off `/cluster/{environment}`: the environment is a path parameter the
        // routes read from the snapshot, and splitting it across two shapes would let a caller
        // read one environment's series while restarting another's.
        .route("/deployment/cluster", deployment_cluster)
        .route(
            "/deployment/cluster/{environment}/samples/{workload}",
            deployment_cluster_samples,
        )
        .route(
            "/deployment/cluster/{environment}/restart",
            deployment_cluster_restart,
        )
        .route(
            "/deployment/cluster/{environment}/sample",
            deployment_cluster_sample,
        )
        .route("/environments", environments)
        .route("/environments/{id}", environment_one)
        .route("/environments/{id}/clone", environment_one_clone)
        .route("/environments/{id}/clone-jobs", environment_jobs)
        .route("/environments/{id}/changes", environment_changes)
        .route("/environments/{id}/promotions", environment_promotions)
        .route("/promotions/{id}", promotion_one)
        .route("/promotions/{id}/approve", promotion_approve)
        .route("/promotions/{id}/cancel", promotion_cancel)
        .route(
            "/environments/{id}/clone-jobs/{job_id}/cancel",
            environment_job_cancel,
        )
        .route("/webhooks", webhooks)
        .route("/webhooks/{id}", webhook)
        .route("/webhooks/{id}/deliveries", webhook_deliveries)
        .route(
            "/webhooks/{id}/deliveries/redeliver",
            webhook_redeliver_batch,
        )
        .route(
            "/webhooks/{id}/deliveries/{delivery_id}/redeliver",
            webhook_redeliver_one,
        )
        .route("/webhooks/{id}/stats", webhook_stats)
        .route("/webhooks/{id}/secret/rotate", webhook_secret_rotate)
        .route("/webhooks/{id}/test", webhook_test)
        .route("/events", events)
        .route("/events/catalogue", event_catalogue)
        .route("/events/retention", event_retention)
        .route("/events/retention/sweep", event_retention_sweep)
        .route("/automations", automations)
        .route("/automations/catalogue", automation_catalogue)
        .route("/automations/{id}", automation_entry)
        .route(
            "/pages/{id}/revisions/{revision_id}/comments",
            page_revision_comments,
        );

    // The header policy is applied to the WHOLE tree, `/healthz` included: a security header
    // that is missing on the one endpoint a scanner probes is missing where it is read.
    //
    // The baseline installs here and `security_headers::put` replaces it in place after a save,
    // so a saved policy applies to the next response rather than at the next restart. The saved
    // document is read at boot by `main.rs` and handed in; a platform whose database is
    // unreachable at boot still serves headers rather than serving none.
    let header_layer = crate::headers_middleware::install(omnion_security::HeaderPolicy::default());

    // The rate limiter (REQ-012, slice 3) holds its document in the same process-wide cell the
    // headers do, and for the same reason: read once, not per request, so a request's cost never
    // depends on the database; `security_limiter::put_rate_limits` then replaces the numbers in
    // place, so a save applies to the next request rather than after the next restart.
    //
    // `main.rs` reads the stored document before building the router and installs it; a router
    // built without one (the in-process test harnesses) falls back to the shipped defaults rather
    // than to no limiter at all, which is the failure mode this whole layer exists to remove.
    let limiter_layer = crate::rate_limit_middleware::ensure_installed(&state);
    // The IP access list is installed the same way and for the same reason (REQ-012 slice 4):
    // read once, swapped in place by a save, so a rule added on the panel refuses the *next*
    // request rather than the one after the next restart.
    let ip_access_layer = crate::security_ip::ensure_installed(&state);

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(readyz::readyz))
        // The per-organization module switch (REQ-005, slice 4). One layer on the whole API
        // rather than a guard per module: the layer asks the same path table the panel's
        // navigation is built from which module owns the matched route, so a screen cannot be
        // added to a module without the switch governing it, and the core (tenancy, identity,
        // content, search) is never a module and can never be switched off.
        //
        // The switch is a layer on `v1`, so it runs INSIDE the nest, which is what keeps it
        // below the CSRF layer below: a request must authenticate before either of them answers.
        .nest(
            "/api/v1",
            v1.layer(crate::module_guard::RequireModules::new(state.clone())),
        )
        // The limiter is the OUTERMOST layer, ahead of CSRF and ahead of every permission guard,
        // and the order is the design rather than an accident of where the line falls in the chain:
        //
        // * A limiter behind the guards would cap only callers who already hold a permission, which
        //   leaves an anonymous spray against `POST /auth/login` uncapped — the one path worth
        //   capping, and the only one an attacker can reach without an account.
        // * Ahead of CSRF, because a cookie-less mutation is still a request somebody is sending and
        //   it must spend budget whether or not it would have been refused anyway.
        //
        // `/healthz` and `/readyz` are inside it too, which is deliberate and cheap: they are two
        // `GET`s a probe makes every few seconds, counted against a budget of 600 a minute, and a
        // probe that trips the limiter is a probe that reports the platform down.
        .layer(crate::rate_limit_middleware::rate_limit(
            limiter_layer.clone(),
        ))
        // The IP access list runs ahead of the limiter and ahead of every guard, for the same
        // reason the limiter does: an address rule exists to stop a caller who has no account,
        // so anything behind `guards::require` would never see one. Ahead of the limiter because
        // a denied address should cost nothing — not even a Redis round trip.
        .layer(crate::security_ip::ip_access(ip_access_layer))
        // CSRF sits OUTSIDE the permission guards on purpose: a guard answers 401 for a request
        // with no session and 403 for one whose account lacks the key. The CSRF layer's answer is
        // about the *request*, and it has to be reached only by a request that actually
        // authenticated - which is what the guards having run first guarantees.
        .layer(crate::headers_middleware::require_csrf(&state))
        .layer(header_layer.clone())
        .with_state(state)
}
