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
//! The reliability surface (`/reliability/rate-limits`, docs/requests/REQ-127) is the
//! PLATFORM-WIDE budget — user, organization, ip and route scopes an operator edits, with a
//! dry-run that resolves the same policy the middleware resolves. It is deliberately separate
//! from the security centre's per-route limiter, which is the gateway's own budget: two layers,
//! two documents, and every refusal names which one answered.
//!
//! The tenancy surface (`/organizations`, `/sites`) is guarded
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
pub mod commands;
pub mod content;
pub mod restore_jobs;
// Deployment tooling (REQ-128, slice 4). The release cache, the artifact list, the bundle
// generator and the upgrade plan.
pub mod backfills;
pub mod deployment;
// Anonymised support exports (REQ-129, slice 4). Its own module rather than more of
// `backfills.rs` because the two halves of that file are `migration_backfills` state and
// `seed_datasets`, and an export is neither — it is the only artifact on this surface that leaves
// the platform, and burying it at the end of a file about jobs would make that invisible.
pub mod exports;
pub mod graphql;
pub mod graphql_documents;
pub mod graphql_manager;
pub mod graphql_settings;
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
pub mod migrations;
pub mod notifications;
pub mod notifications_admin;
pub mod notifications_test;
pub mod observability;
pub mod observability_alerts;
pub mod observability_overview;
pub mod observability_traces;
pub mod onboarding;
pub mod public;
pub mod readyz;
pub mod reliability_idempotency;
pub mod reliability_intake;
pub mod reliability_limits;
pub mod reliability_retries;
pub mod scim;
pub mod search;
pub mod secrets;
pub mod secrets_audit;
pub mod secrets_credentials;
pub mod secrets_leases;
pub mod security;
pub mod security_events;
pub mod security_headers;
pub mod security_ip;
pub mod security_limiter;
pub mod security_secrets;
pub mod sso;
pub mod tenancy;
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
    let pages = get(content::list_pages)
        .layer(guards::require(&state, "content.pages.read"))
        .merge(post(content::create_page).layer(guards::require(&state, "content.pages.create")));

    let page = get(content::get_page)
        .layer(guards::require(&state, "content.pages.read"))
        .merge(patch(content::update_page).layer(guards::require(&state, "content.pages.update")))
        .merge(delete(content::delete_page).layer(guards::require(&state, "content.pages.delete")));

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
    // The uploader filter's candidates, on the same key as the listing they filter. It is a
    // separate route because it is a separate question, and because a `media.read` account must
    // not need `users.read` to see who uploaded the files it is already allowed to read.
    let media_uploaders =
        get(media_files::list_uploaders).layer(guards::require(&state, "media.read"));
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
    //
    // The replace route carries the same body limit as the upload route, and without it it does
    // not: axum's `DefaultBodyLimit` is 2 MB, so a replacement larger than that is refused
    // `413 payload_too_large` **before** `create_version` is ever entered. The library's own
    // limit is `MAX_UPLOAD_BYTES` (25 MB) and the settings screen lets an operator raise it, so
    // without this line the panel could store a 20 MB file and then be unable to replace it — and
    // the handler's own size check, which names the real limit and its own number, was unreachable
    // for every file above 2 MB. Two limits for one operation, and the smaller one is neither
    // documented nor intentional.
    let media_version_create = Router::new()
        // The path is the **full** one, because this router is merged into `v1` and not nested
        // under `/media/{id}/versions`. `media_upload` above is mounted the same way for the same
        // reason; a bare `/` here registers the handler at the v1 root and the replace answers
        // `405` for its own path, which reads as "the route is gone" rather than "the route is
        // mounted one level too high".
        .route(
            "/media/{id}/versions",
            post(media_versions::create_version).layer(guards::require(&state, "media.upload")),
        )
        .layer(DefaultBodyLimit::max(
            omnion_media::MAX_UPLOAD_BYTES as usize + media::UPLOAD_BODY_SLACK,
        ));
    let media_versions =
        get(media_versions::list_versions).layer(guards::require(&state, "media.read"));
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
    let public_pages = get(public::get_published_page);

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
            post(automation::create_automation)
                // The keyed layer goes FIRST, so it is the INNERMOST of the two: `.layer` wraps
                // what is already built, so the LAST call is the OUTERMOST. This ordering is the
                // acceptance criterion, not a style choice —
                // "a keyed request that is refused by a permission check never consumes an
                // idempotency key" is true here because the guard answers before the code that
                // inserts a key row is ever reached. There is no rollback branch to be wrong, and
                // a rollback would be a second bug: releasing the key on a refusal would let a
                // refused request delete the WINNER's `in_progress` row, which is a different
                // request running concurrently.
                //
                // The other half of the same ordering is what the keyed layer reads: it takes
                // the subject from the `CurrentSession` the guard writes into the request's
                // extensions. Outermost, that map is empty, the layer declines to claim, and the
                // key silently does nothing — which is exactly what the first run of this walk
                // showed, and is the reason the walk asserts the header rather than the 201.
                .layer(crate::idempotency_middleware::require(&state))
                // The guard is OUTSIDE the keyed layer, so it is the LAST `.layer` call and the
                // first thing a request meets. `POST /automations` is also the FIRST endpoint to
                // opt into the contract, chosen deliberately: it is a **job submission** — the
                // request names the case itself — and the write a client is most likely to retry,
                // because a dropped connection after a `201` and a `201` for a rule that already
                // exists are the same end state and two different experiences.
                .layer(guards::require(&state, "workflows.manage")),
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
        // The deployment centre's release surface (REQ-128, slice 4).
        //
        // Three permissions, and the split is the one the request draws: reading what this
        // install is running and what releases exist is `deployment.read`; GENERATING a bundle
        // writes a row and shells out to the pipeline's generator, so it is its own key and it
        // is rate limited; acknowledging a destructive-migration warning is `deployment.deploy`,
        // the same power that rolls the workloads, because accepting that the database can only
        // be restored is part of deciding to deploy.
        //
        // `deployment.deploy` and NOT a `deployment.manage`: the family has `read`, `preview`,
        // `deploy` and `rollback`, and a guard naming a key outside the catalogue refuses EVERY
        // account in the installation — the acknowledgement route would have answered 403 to the
        // instance owner, and every unit test in `omnion-deployment` would still have been green
        // because none of them builds a router. The integration walk caught it on its first run.
        //
        // `/deployment/upgrade-plan/acknowledge` is registered BEFORE `/deployment/artifacts/{version}`
        // would ever shadow it — axum's router prefers a literal segment over a capture, so the
        // order here is documentation rather than a requirement, and the comment says so rather
        // than implying the position matters.
        .route(
            "/deployment/artifacts",
            get(deployment::list_artifacts).layer(guards::require(&state, "deployment.read")),
        )
        .route(
            "/deployment/artifacts/{version}",
            get(deployment::read_release).layer(guards::require(&state, "deployment.read")),
        )
        .route(
            "/deployment/bundles",
            get(deployment::list_bundles)
                .layer(guards::require(&state, "deployment.read"))
                .merge(
                    post(deployment::create_bundle)
                        .layer(guards::require(&state, "deployment.bundle.generate")),
                ),
        )
        .route(
            "/deployment/bundles/{id}",
            get(deployment::read_bundle).layer(guards::require(&state, "deployment.read")),
        )
        .route(
            "/deployment/bundles/{id}/files/{name}",
            get(deployment::download_bundle_file).layer(guards::require(&state, "deployment.read")),
        )
        .route(
            "/deployment/bundles/{id}/render",
            post(deployment::render_bundle)
                .layer(guards::require(&state, "deployment.bundle.generate")),
        )
        .route(
            "/deployment/upgrade-plan",
            get(deployment::read_upgrade_plan).layer(guards::require(&state, "deployment.read")),
        )
        .route(
            "/deployment/upgrade-plan/acknowledge",
            post(deployment::acknowledge_upgrade_plan)
                .layer(guards::require(&state, "deployment.deploy")),
        )
        // The migration ledger (REQ-129, slice 1). Three powers, and the split is the one the
        // request draws:
        //
        // * `migrations.read` — the ledger, one migration's SQL, the lint findings, the policy and
        //   the lock state. All of it is derived from files and rows this installation already has,
        //   so it carries nothing a release reviewer should not see.
        // * `migrations.apply` — DDL, and NOT `deployment.deploy`. A key that both ships an image
        //   and writes the schema cannot answer "who changed the database?" after an incident,
        //   and every unit test in `omnion-permissions` would still be green with the guard on
        //   either key because none of them builds a router. The integration walk proves the split
        //   over the real one.
        // * `migrations.verify` — rehearsing a reversal against a scratch database. Its own key
        //   because it is the only write here that EXECUTES SQL, and because it is the write that
        //   turns a release's rollback path from `unknown` into `reversible`.
        //
        // `/deployment/migrations/{version}` is registered AFTER `/deployment/migrations/lock`,
        // `/policy` and `/violations` would ever shadow it — axum prefers a literal segment over a
        // capture, so the order is documentation rather than a requirement, and this says so
        // instead of implying the position matters.
        .route(
            "/deployment/migrations",
            get(migrations::list_migrations)
                .layer(guards::require(&state, "deployment.migrations.read"))
                .merge(
                    post(migrations::apply_migrations)
                        .layer(guards::require(&state, "deployment.migrations.apply")),
                ),
        )
        .route(
            "/deployment/migrations/plan",
            post(migrations::plan_migrations)
                .layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/migrations/lock",
            get(migrations::read_lock).layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/migrations/violations",
            get(migrations::read_violations)
                .layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/migrations/violations/{id}/waive",
            post(migrations::waive_violation)
                .layer(guards::require(&state, "deployment.migrations.apply")),
        )
        .route(
            "/deployment/migrations/policy",
            get(migrations::read_policy)
                .layer(guards::require(&state, "deployment.migrations.read"))
                .merge(
                    put(migrations::save_policy)
                        .layer(guards::require(&state, "deployment.migrations.apply")),
                ),
        )
        .route(
            "/deployment/migrations/{version}",
            get(migrations::read_migration)
                .layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/migrations/{version}/verify-down",
            post(migrations::rehearse_reversal)
                .layer(guards::require(&state, "deployment.migrations.verify")),
        )
        // Backfill jobs (REQ-129, slice 3). `read` is `migrations.read` — a backfill is a
        // migration that has not finished, and an operator reading the ledger needs to see the
        // jobs that release registered. `backfills.manage` is a SEPARATE key from
        // `migrations.apply`, and the reason is the whole point of the split: applying a migration
        // changes the schema for rows written from now on, while a backfill REWRITES EVERY
        // EXISTING ROW of a table. An operator trusted to break the schema has proved nothing
        // about the data in it.
        //
        // `/deployment/backfills/{id}` is registered AFTER `/deployment/backfills` and before the
        // `{id}/…` action routes only for the usual axum reason — a literal segment and a capture
        // segment cannot be siblings without the capture eating the literal.
        .route(
            "/deployment/backfills",
            get(backfills::list_backfills)
                .layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/backfills/{id}",
            get(backfills::read_backfill)
                .layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/backfills/{id}/run",
            post(backfills::run_batch)
                .layer(guards::require(&state, "deployment.backfills.manage")),
        )
        .route(
            "/deployment/backfills/{id}/pause",
            post(backfills::pause_backfill)
                .layer(guards::require(&state, "deployment.backfills.manage")),
        )
        .route(
            "/deployment/backfills/{id}/resume",
            post(backfills::resume_backfill)
                .layer(guards::require(&state, "deployment.backfills.manage")),
        )
        // Seed datasets. The READ is `migrations.read` — the datasets and this installation's
        // load history are release metadata — and the LOAD is `seeds.load`, its own key, because
        // it is the only write on this surface that puts fixture rows into an operator's database.
        // The environment refusal is separate from the key and both apply: the key says who, and
        // the environment says where — the same split `migrations.apply` has.
        .route(
            "/deployment/seeds",
            get(backfills::list_seeds).layer(guards::require(&state, "deployment.migrations.read")),
        )
        .route(
            "/deployment/seeds/{name}/load",
            post(backfills::load_seed).layer(guards::require(&state, "deployment.seeds.load")),
        )
        // Anonymised exports (REQ-129, slice 4). Three keys and NOT two, and the split is the
        // point: `migrations.read` sees the ledger of what was asked for (an operator reading the
        // migration surface needs to know a support dump left the platform), `exports.create` is
        // the decision to make one, and `exports.download` is the moment the bytes leave. A single
        // "exports.manage" key would answer none of those questions — and the third of them is the
        // one an incident is actually about.
        //
        // `/deployment/exports/classifications` is registered BEFORE `/deployment/exports/{id}`
        // for the usual axum reason: a literal segment cannot be a sibling of a capture segment,
        // or `{id}` would swallow the word.
        .route(
            "/deployment/exports/classifications",
            get(exports::read_classifications)
                .layer(guards::require(&state, "deployment.migrations.read"))
                // Writing the map is a SEPARATE power from reading it, and it is the stronger one:
                // a reader can see what is classified, only a reviewer can change it. Classifying a
                // column `safe` is how a credential gets exported raw, so this carries
                // `deployment.migrations.apply` — the key that owns DDL — rather than the read
                // key sitting directly above it. Sharing one key would let anyone who can see the
                // map also un-see a column from it.
                .merge(
                    axum::routing::put(exports::classify_column)
                        .layer(guards::require(&state, "deployment.migrations.apply")),
                ),
        )
        .route(
            "/deployment/exports",
            get(exports::list_exports)
                .layer(guards::require(&state, "deployment.migrations.read"))
                .merge(
                    post(exports::create_export)
                        .layer(guards::require(&state, "deployment.exports.create")),
                ),
        )
        .route(
            "/deployment/exports/{id}",
            get(exports::read_export)
                .layer(guards::require(&state, "deployment.migrations.read"))
                .merge(
                    axum::routing::delete(exports::revoke_export)
                        .layer(guards::require(&state, "deployment.exports.create")),
                ),
        )
        .route(
            "/deployment/exports/{id}/run",
            post(exports::run_export).layer(guards::require(&state, "deployment.exports.create")),
        )
        .route(
            "/deployment/exports/{id}/download",
            get(exports::download_export)
                .layer(guards::require(&state, "deployment.exports.download")),
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
        // The reliability centre's limits (REQ-127 slice 1). Deliberately BESIDE the security
        // centre's limiter above rather than merged into it: the gateway owns a per-route budget
        // and this owns the platform-wide user/organization/ip budgets, and an operator who
        // cannot tell which document a `429` came from cannot fix it. Every refusal below names
        // its limiter in `details.limiter`, which is what makes the two joinable.
        //
        // The dry-run sits behind `reliability.manage` rather than `reliability.read`, which
        // reads backwards: it changes nothing. It reads live counters, and the panel shows the
        // winning policy's identity, the remaining budget and the Redis key — that is a map of
        // the limiter's internals, and `security.read` was explicitly not granted it by REQ-012's
        // own comment. So the same reasoning, one power stricter.
        .route(
            "/reliability/rate-limits",
            get(reliability_limits::list_policies)
                .layer(guards::require(&state, "reliability.read"))
                .merge(
                    post(reliability_limits::create_policy)
                        .layer(guards::require(&state, "reliability.manage")),
                ),
        )
        .route(
            "/reliability/rate-limits/evaluate",
            post(reliability_limits::evaluate).layer(guards::require(&state, "reliability.manage")),
        )
        .route(
            "/reliability/rate-limits/refusals",
            get(reliability_limits::list_refusals)
                .layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/rate-limits/{id}",
            patch(reliability_limits::update_policy)
                .layer(guards::require(&state, "reliability.manage"))
                .merge(
                    delete(reliability_limits::delete_policy)
                        .layer(guards::require(&state, "reliability.manage")),
                ),
        )
        .route(
            "/reliability/idempotency",
            get(reliability_idempotency::list_keys)
                .layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/idempotency/{key}",
            get(reliability_idempotency::get_key)
                .layer(guards::require(&state, "reliability.read"))
                .merge(
                    delete(reliability_idempotency::release_key)
                        .layer(guards::require(&state, "reliability.manage")),
                ),
        )
        // Retries and breakers (REQ-127 slice 3). The two screens a worker is read through at
        // 03:00: what is the policy, what has been tried, what is dead-lettered, and which
        // providers the platform is refusing to call right now.
        .route(
            "/reliability/retry-policies",
            get(reliability_retries::list_policies)
                .layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/retry-policies/{subsystem}",
            put(reliability_retries::save_policy)
                .layer(guards::require(&state, "reliability.manage")),
        )
        .route(
            "/reliability/retry-attempts",
            get(reliability_retries::list_attempts)
                .layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/retry-attempts/{id}/retry-now",
            post(reliability_retries::retry_now)
                .layer(guards::require(&state, "reliability.manage")),
        )
        .route(
            "/reliability/breakers",
            get(reliability_retries::list_breakers)
                .layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/breakers/{key}",
            patch(reliability_retries::update_breaker)
                .layer(guards::require(&state, "reliability.manage")),
        )
        .route(
            "/reliability/breakers/{key}/reset",
            post(reliability_retries::reset_breaker)
                .layer(guards::require(&state, "reliability.manage")),
        )
        .route(
            "/reliability/breakers/{key}/force-open",
            post(reliability_retries::force_open_breaker)
                .layer(guards::require(&state, "reliability.manage")),
        )
        // The gate every outbound subsystem's client calls. `reliability.read` rather than
        // `manage` because observing a breaker is what a caller DOES, not what an operator
        // edits — and gating it behind `manage` would mean the AI hub's runtime token cannot
        // record that a provider is down.
        .route(
            "/reliability/breakers/{key}/observe",
            post(reliability_retries::observe_breaker)
                .layer(guards::require(&state, "reliability.read")),
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

    // The intake guard (REQ-127 slice 4). TWO routers, deliberately not one:
    //
    // * `intake_panel` sits inside the versioned tree with its permission guards. Declaring a
    //   path is `reliability.intake.manage`, which the catalogue keeps SEPARATE from
    //   `reliability.manage` because a budget is a number and a declaration is a door.
    // * `intake_ingress` is the guarded request itself, on a path that does not exist until an
    //   operator declares it. It carries NO session extractor and NO guard: a provider posting a
    //   signed webhook has no account, and the signature IS the credential. Its body limit is
    //   the platform maximum, not an endpoint's cap — the per-endpoint cap is enforced by
    //   `intake::evaluate` on the bytes that actually arrived, which is the only measurement
    //   that cannot be lied about with a `content-length` header. An outer limit set BELOW the
    //   declared cap would refuse a legitimate large export with a bare `413` and no
    //   `payload_too_large` code, which is exactly the answer the acceptance criteria require
    //   this guard to be able to give itself.
    let intake_ingress = Router::new()
        .route(
            "/public/intake/{id}",
            post(reliability_intake::guarded_request),
        )
        .layer(DefaultBodyLimit::max(
            omnion_reliability::intake::MAX_PAYLOAD_BYTES as usize,
        ));

    let intake_panel = Router::new()
        .route(
            "/reliability/intake",
            get(reliability_intake::list)
                .layer(guards::require(&state, "reliability.read"))
                .merge(
                    post(reliability_intake::create)
                        .layer(guards::require(&state, "reliability.intake.manage")),
                ),
        )
        .route(
            "/reliability/intake/rejections",
            get(reliability_intake::rejections).layer(guards::require(&state, "reliability.read")),
        )
        .route(
            "/reliability/intake/{id}",
            patch(reliability_intake::update)
                .layer(guards::require(&state, "reliability.intake.manage"))
                .merge(
                    delete(reliability_intake::remove)
                        .layer(guards::require(&state, "reliability.intake.manage")),
                ),
        )
        .route(
            "/reliability/intake/{id}/verify-sample",
            post(reliability_intake::verify_sample)
                .layer(guards::require(&state, "reliability.intake.manage")),
        );

    // The key ring and the rotation ceremony (docs/requests/REQ-125, slice 1). Reading the
    // ring is `secrets.read`; starting a rotation, pausing and resuming its walk is
    // `secrets.root.manage`, because a rotation is the one irreversible operation on the ring.
    let secrets_root_key = Router::new()
        .route("/secrets/root-key", get(secrets::read_root_key))
        .route(
            "/secrets/root-key/rewrap-jobs/{id}",
            get(secrets::read_rewrap_job),
        )
        .route_layer(guards::require(&state, "secrets.read"));

    let secrets_rotation = Router::new()
        .route("/secrets/root-key/rotate", post(secrets::rotate_root_key))
        .route(
            "/secrets/root-key/rewrap-jobs/{id}/pause",
            post(secrets::pause_rewrap_job),
        )
        .route(
            "/secrets/root-key/rewrap-jobs/{id}/resume",
            post(secrets::resume_rewrap_job),
        )
        .route_layer(guards::require(&state, "secrets.root.manage"));

    // Typed credential profiles and the slots consumers resolve through (docs/requests/REQ-125,
    // slice 2). Reading them is `secrets.read`; typing a secret and running a validator is
    // `secrets.manage`; changing an assignment is `secrets.assign`, kept separate from both
    // because a slot swap silently changes what a running production workload is using.
    let secrets_credentials_read = Router::new()
        .route(
            "/secrets/credentials",
            get(secrets_credentials::read_credentials),
        )
        .route(
            "/secrets/credentials/{id}",
            get(secrets_credentials::read_credential),
        )
        .route("/credential-slots", get(secrets_credentials::read_slots))
        .route(
            "/credential-slots/{scope}/{slot}/resolve/{scope_id}",
            get(secrets_credentials::resolve_slot_route),
        )
        .route_layer(guards::require(&state, "secrets.read"));

    let secrets_credentials_write = Router::new()
        .route(
            "/secrets/{id}/credential",
            post(secrets_credentials::attach_credential_profile),
        )
        .route(
            "/secrets/{id}/validate",
            post(secrets_credentials::validate_credential),
        )
        .route_layer(guards::require(&state, "secrets.manage"));

    // The assignment write carries its own permission: `secrets.assign`, deliberately not the
    // read permission, because a slot swap silently changes what a running workload is using.
    let secrets_slot_assign = Router::new()
        .route(
            "/credential-slots/{scope}/{slot}",
            put(secrets_credentials::put_slot),
        )
        .route_layer(guards::require(&state, "secrets.assign"));

    // slice 3. Reading leases and keys is a read; issuing, revoking and deleting is its own
    // permission, because each of those hands out or takes back the power to read a value.
    let secrets_leases_read = Router::new()
        .route("/secret-leases", get(secrets_leases::read_leases))
        .route(
            "/deployment-keys",
            get(secrets_leases::read_deployment_keys),
        )
        .route(
            "/deployment-keys/{id}/uses",
            get(secrets_leases::read_deployment_key_uses),
        )
        .route_layer(guards::require(&state, "secrets.read"));

    // The lease writes are `secrets.lease`; the deployment-key writes are
    // `secrets.deploykeys.manage`. A deployment key is a machine credential, so managing one
    // is a strictly bigger deal than managing a lease and gets its own name.
    let secrets_lease_write = Router::new()
        .route("/secrets/{id}/lease", post(secrets_leases::issue_lease))
        .route(
            "/secret-leases/{id}/revoke",
            post(secrets_leases::revoke_lease),
        )
        .route_layer(guards::require(&state, "secrets.lease"));

    let secrets_deploy_key_write = Router::new()
        .route(
            "/deployment-keys",
            post(secrets_leases::create_deployment_key),
        )
        .route(
            "/deployment-keys/{id}/revoke",
            post(secrets_leases::revoke_deployment_key),
        )
        .route(
            "/deployment-keys/{id}",
            delete(secrets_leases::delete_deployment_key),
        )
        .route_layer(guards::require(&state, "secrets.deploykeys.manage"));

    // slice 4. The audit surface is its own permission (`secrets.audit`) and deliberately not
    // `secrets.read`: reading which secrets an account has touched, from which address and
    // under which request id is a bigger question than reading the list of secrets, and one
    // operator should be able to grant the first without the second.
    let secrets_audit = Router::new()
        .route("/secrets/audit", get(secrets_audit::read_audit))
        .route("/secrets/audit/export", get(secrets_audit::export_audit))
        .route(
            "/secrets/audit/anomalies",
            get(secrets_audit::read_anomalies),
        )
        .route(
            "/secrets/audit/anomalies/{id}/acknowledge",
            patch(secrets_audit::acknowledge_anomaly),
        )
        .route_layer(guards::require(&state, "secrets.audit"));

    // The persisted-document manager (REQ-130 slice 2).
    //
    // Split into THREE routers rather than one route per verb with a `.layer()` each, because a
    // `.layer()` on a `get().post()` pair applies ONE permission to BOTH verbs — the first draft
    // guarded `GET /documents` with the manage key, which would have made the read list refuse the
    // very people the manager exists to inform. Two separate routers is the shape the rest of this
    // file uses (see `secrets_audit` above and the observability writers below), and it is the
    // only shape in which the read guard and the write guard can differ.
    //
    // `content.pages.read` for reads and `deployment.migrations.manage` for writes — both REAL
    // catalogue keys. The request's table names `developer.read` and `developer.graphql.manage`,
    // and this repository ships **no `developer.*` key at all**; an uncatalogued key resolves to no
    // permission, so a route guarded on one answers 403 for every caller including the instance
    // owner while looking perfectly healthy. See `graphql_manager.rs` for each substitution's
    // reasoning and a test that reads the catalogue to hold them.
    let graphql_documents_read = Router::new()
        .route("/graphql/documents", get(graphql_manager::list))
        .route("/graphql/documents/prunable", get(graphql_manager::prunable))
        .route("/graphql/documents/{id}", get(graphql_manager::detail))
        .route_layer(guards::require(&state, graphql_manager::READ_PERMISSION));

    let graphql_documents_write = Router::new()
        .route("/graphql/documents", post(graphql_manager::register))
        .route(
            "/graphql/documents/{id}",
            put(graphql_manager::set_status),
        )
        .route_layer(guards::require(&state, graphql_manager::MANAGE_PERMISSION));

    // The endpoint's own settings. Read with the read guard and written with the write guard, for
    // the same reason: an operator who may read the policy may see it, and only an operator who
    // may manage the release shape may change it.
    let graphql_settings_routes = Router::new()
        .route("/graphql/settings", get(graphql_manager::read_settings))
        .route_layer(guards::require(&state, graphql_manager::READ_PERMISSION));
    let graphql_settings_writes = Router::new()
        .route("/graphql/settings", put(graphql_manager::save_settings))
        .route_layer(guards::require(&state, graphql_manager::MANAGE_PERMISSION));

    // Redemption is the ONE handler with no session guard. It is authenticated by the
    // deployment key in the header instead, so it lives on its own router and is never
    // reachable by a cookie: a browser cannot redeem a lease, which is the property the whole
    // request rests on.
    // The observability surface (`/observability/*`, docs/requests/REQ-126). Reading telemetry is
    // `observability.read`; changing what the platform records, and for how long, is
    // `observability.manage` — a different power on purpose, because "see what happened" and
    // "decide what gets recorded" are not the same authority. Slice 1 ships the log explorer and
    // its settings; the exporters, alert rules and trace search arrive in slices 3 and 4.
    let observability_read = Router::new()
        .route("/observability/logs", get(observability::read_logs))
        .route(
            "/observability/logs/requests/{request_id}",
            get(observability::read_request_lines),
        )
        .route(
            "/observability/logs/settings",
            get(observability::read_settings),
        )
        // The metric catalogue and the chart behind it (REQ-126, slice 2). Both are reads of
        // telemetry, so both are `observability.read`; a caller who may see what happened may see
        // what the instance counts.
        // The landing screen (REQ-126). It is a read of the same sources the tiles below read,
        // so it is `observability.read` — a caller who may see what happened may see the summary
        // of it. It writes nothing, so it records no audit entry: a screen that leaves a row
        // behind on every page view is an audit trail of navigation.
        .route(
            "/observability/overview",
            get(observability_overview::read_overview),
        )
        .route(
            "/observability/metrics/catalog",
            get(observability::read_catalog),
        )
        .route(
            "/observability/metrics/query",
            get(observability::read_metric_query),
        )
        // The trace search and its detail (REQ-126, slice 3). Both are reads of telemetry, so
        // both are `observability.read` — a caller who may see what happened may see how long it
        // took and which span failed.
        .route(
            "/observability/traces",
            get(observability_traces::read_traces),
        )
        .route(
            "/observability/traces/{trace_id}",
            get(observability_traces::read_trace),
        )
        .route(
            "/observability/exporters",
            get(observability_traces::read_exporters),
        )
        // The alert surface and the settings screen (REQ-126, slice 4). Reading a rule is
        // `observability.read` for the same reason reading an exporter is: what the instance is
        // configured to watch is as readable as what it recorded.
        .route(
            "/observability/alert-rules",
            get(observability_alerts::read_alert_rules),
        )
        .route(
            "/observability/alerts",
            get(observability_alerts::read_alerts),
        )
        .route(
            "/observability/settings",
            get(observability_alerts::read_observability_settings),
        )
        // The bundle manifest and the lifecycle contract. Both are reads of static, build-time
        // facts — which bundle this instance ships, and what its probes do during a drain — and
        // REQ-128 (deployment tooling) and the deployment centre need them as data rather than
        // each keeping a hard-coded copy of a contract the process has to honour.
        .route(
            "/observability/bundle",
            get(observability_alerts::read_bundle),
        )
        .route(
            "/observability/lifecycle",
            get(observability_alerts::read_lifecycle),
        )
        .route_layer(guards::require(&state, "observability.read"));

    let observability_write = Router::new()
        .route(
            "/observability/logs/settings",
            put(observability::save_settings),
        )
        // Re-seeding the catalogue is a write, not a read: it changes what the panel documents and
        // it writes an audit row, so it takes `observability.manage` like the other mutations.
        .route(
            "/observability/metrics/sync",
            post(observability::sync_catalog),
        )
        // Exporter management (REQ-126, slice 3). An exporter row is a standing instruction to
        // send this instance's telemetry somewhere, so it takes `observability.manage` — a
        // different power from `observability.read` on purpose, and every mutation writes an
        // audit row.
        .route(
            "/observability/exporters",
            post(observability_traces::create_exporter),
        )
        // PATCH, not PUT: the request's API table documents `PATCH /exporters/{id}` and the panel
        // sends exactly that, so registering only `put` left the exporters screen's Edit button
        // answering 405 on a method the router does not have. `apps/api/tests/observability_permissions.rs`
        // now drives every mutating route with the method the CLIENT sends, which is the only
        // check that can see this class of defect.
        .route(
            "/observability/exporters/{id}",
            axum::routing::patch(observability_traces::update_exporter)
                .delete(observability_traces::delete_exporter),
        )
        .route(
            "/observability/exporters/{id}/test",
            post(observability_traces::test_exporter),
        )
        // Alert rules, silences and the settings row (REQ-126, slice 4). All three change what
        // the instance tells an operator and when it interrupts them, so all three take
        // `observability.manage` and all three write an audit row. The preview is a POST
        // because it evaluates a caller-supplied expression — a GET would be replayed by every
        // cache in the path.
        .route(
            "/observability/alert-rules",
            post(observability_alerts::create_alert_rule),
        )
        .route(
            "/observability/alert-rules/preview",
            post(observability_alerts::preview_alert_rule),
        )
        .route(
            "/observability/alert-rules/{id}",
            axum::routing::patch(observability_alerts::update_alert_rule)
                .delete(observability_alerts::delete_alert_rule),
        )
        .route(
            "/observability/silences",
            post(observability_alerts::create_silence),
        )
        .route(
            "/observability/silences/{id}",
            axum::routing::delete(observability_alerts::delete_silence),
        )
        .route(
            "/observability/settings",
            put(observability_alerts::save_observability_settings),
        );

    let secrets_lease_redeem = Router::new().route(
        "/secret-leases/{id}/redeem",
        post(secrets_leases::redeem_lease),
    );

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
        .merge(secrets_root_key)
        .merge(secrets_rotation)
        .merge(secrets_credentials_read)
        .merge(secrets_credentials_write)
        .merge(secrets_slot_assign)
        .merge(secrets_leases_read)
        .merge(secrets_audit)
        .merge(graphql_documents_read)
        .merge(graphql_documents_write)
        .merge(graphql_settings_routes)
        .merge(graphql_settings_writes)
        .merge(secrets_lease_write)
        .merge(secrets_deploy_key_write)
        .merge(secrets_lease_redeem)
        .merge(observability_read)
        .merge(observability_write.layer(guards::require(&state, "observability.manage")))
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
        // The intake guard's two routers. The panel half joins the guarded tree beside the
        // breakers above; the ingress half is the *request* path and carries no session.
        .merge(intake_panel)
        .merge(intake_ingress)
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
        .route("/organizations", organizations)
        .route("/organizations/{id}", organization)
        .route("/sites", sites)
        .route("/sites/{id}", site)
        .route("/sites/{id}/domains", domains)
        .route("/sites/{id}/domains/{domain_id}", domain)
        .route("/sites/{id}/domains/{domain_id}/primary", domain_primary)
        // The GraphQL surface (REQ-130, slice 1).
        //
        // **The guard is `content.pages.read`, and that is deliberate.** The endpoint reads the
        // same surface it exposes, so a caller who may read content may run queries — and the key
        // is REAL. The first draft guarded it on `developer.graphql.execute`, which this
        // repository does not ship, and an uncatalogued key resolves to no permission: the route
        // answers `403` for every caller, the instance owner included, while looking perfectly
        // healthy. That is the fourth time this defect has cost this repository a tick.
        //
        // `require_or_machine`, because a service account integrating with the platform has no
        // session and the request says so ("session or API key, sandbox keys included"). The two
        // verbs share one path; the GET leg answers `PERSISTED_QUERY_NOT_FOUND` until slice 2
        // registers documents.
        .route(
            "/graphql",
            post(graphql::execute_graphql)
                .get(graphql::execute_persisted)
                .layer(guards::require_or_machine(&state, "content.pages.read")),
        )
        // The persisted-document manager and the endpoint settings (REQ-130 slice 2).
        //
        // `content.pages.read` guards the reads and `deployment.migrations.manage` the writes, and
        // both are REAL catalogue keys. The request's table names `developer.read` and
        // `developer.graphql.manage`, and **this repository ships no `developer.*` key at all** — an
        // uncatalogued key resolves to no permission, so the route answers 403 for every caller
        // including the owner while looking healthy. `graphql_manager.rs` carries the reasoning for
        // each substitution and a test that reads the catalogue to hold them.
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
        .route("/media/uploaders", media_uploaders)
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
        .merge(media_version_create)
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
        .route("/media/folders/{id}/grants", media_folder_grant_write)
        .route("/media/{id}/grants", media_file_grants)
        .route("/media/{id}/grants", media_file_grant_write)
        .route("/media/grants/{grant_id}", media_grant_delete)
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
    // The platform-wide budget (REQ-127 slice 1), installed beside the gateway limiter rather
    // than merged into it. The in-process harnesses that build a router without a `main.rs` get
    // an EMPTY policy set rather than invented defaults: this layer is a second document, and a
    // second set of defaults is a second thing an operator has to discover and tune. "No policy
    // matches" is the documented state of a fresh instance, and the gateway limiter above is
    // still enforcing its own document throughout.
    let platform_layer = crate::reliability_middleware::ensure_installed(&state);
    // The IP access list is installed the same way and for the same reason (REQ-012 slice 4):
    // read once, swapped in place by a save, so a rule added on the panel refuses the *next*
    // request rather than the one after the next restart.
    let ip_access_layer = crate::security_ip::ensure_installed(&state);

    Router::new()
        .route("/healthz", get(health::healthz))
        // `/livez` is the same handler under the name some platforms expect (REQ-126, slice 4).
        // An alias implemented separately from the endpoint it aliases is a probe that checks
        // something the deployment does not, so this is one handler at two paths.
        .route("/livez", get(health::healthz))
        .route("/readyz", get(readyz::readyz))
        // The Prometheus exposition (REQ-126, slice 2). Unversioned and unauthenticated, beside
        // the probes and for the same reason: a scraper has no session, and a versioned
        // telemetry path is a path that has to be kept compatible with itself. The exposure is a
        // deliberate, documented decision — the bodies it can emit are exactly the families in
        // `omnion_telemetry::metrics::FAMILIES`, and nothing else has a route into them.
        .route("/metrics", get(observability::metrics_exposition))
        .nest("/api/v1", v1)
        // Four layers wrap the versioned tree, and the order is load-bearing. `.layer()` wraps
        // what is already built, so the LAST call is the OUTERMOST and the FIRST is the innermost.
        // Innermost to outermost: the permission guards (installed per-route inside `v1`), the
        // header policy, CSRF, the rate limiter, and finally the request log.
        //
        // The limiter sits ABOVE CSRF and above every permission guard, and that is the design
        // rather than an accident of where the line falls in the chain:
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
        //
        // The request log stays outside the limiter (see the bottom of this chain) so a request
        // BURNED the budget is still a line an operator can find — a rate-limited request with no
        // log line is the one rejection the log cannot answer questions about.
        // The platform budgets sit INSIDE the gateway limiter, so a caller over both budgets is
        // refused by the outer one and the operator's first stop is the document they configured
        // first. The reverse order would mean the newer, less-tuned layer always wins, and a
        // platform whose 429s are decided by whichever row was written last is not debuggable.
        //
        // The IP access list runs ahead of the limiter and ahead of every guard, for the same
        // reason the limiter does: an address rule exists to stop a caller who has no account,
        // so anything behind `guards::require` would never see one. Ahead of the limiter because
        // a denied address should cost nothing — not even a Redis round trip.
        //
        // Read the chain OUTERMOST to INNERMOST, i.e. bottom-up, because `.layer()` wraps what is
        // already built: the LAST call in the block is the OUTERMOST layer. The order below is
        // therefore `platform_limit` → `rate_limit` → `ip_access`, meaning a refused address
        // costs nothing, a caller over the gateway budget is told by the document written first,
        // and a request that BURNED a budget is still a line the request log can find.
        .layer(crate::reliability_middleware::platform_limit(
            platform_layer.clone(),
        ))
        .layer(crate::rate_limit_middleware::rate_limit(
            limiter_layer.clone(),
        ))
        .layer(crate::security_ip::ip_access(ip_access_layer))
        // CSRF sits OUTSIDE the permission guards on purpose: a guard answers 401 for a request
        // with no session and 403 for one whose account lacks the key. The CSRF layer's answer is
        // about the *request*, and it has to be reached only by a request that actually
        // authenticated - which is what the guards having run first guarantees.
        .layer(crate::headers_middleware::require_csrf(&state))
        // The header policy covers the whole tree, the probes included: a security header that is
        // missing on the one endpoint a scanner probes is missing where it is read.
        .layer(header_layer.clone())
        // The request id, the trace and the one line per request (REQ-126 slice 1). Installed on
        // the OUTER router, not on `v1`, so the probes are described too - a probe that fails is
        // the first thing an operator looks for, and a line with no request id is one they cannot
        // join to anything.
        //
        // Outermost of the three, so a request REJECTED by the two layers above it still arrives
        // with a request id and still lands in the log. A CSRF refusal with no request id is the
        // one rejection an operator cannot correlate with the page that caused it.
        //
        // `from_fn_with_state` inline rather than behind a helper that returns an `impl Layer`:
        // `Router::layer` needs the layer's concrete service to be `Clone + Service<Request<Body>>`,
        // and an `impl Layer<Route>` erases exactly the bounds it needs to check.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::request_log::request_context,
        ))
        .with_state(state)
}
