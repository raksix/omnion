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
pub mod ai_agents;
pub mod ai_agent_workspace;
pub mod ai_decisions;
pub mod ai_identities;
pub mod ai_routing;
pub mod ai_skills;
pub mod ai_tools;
pub mod analytics;
pub mod auth;
pub mod automation;
pub mod backups;
pub mod restore_jobs;
pub mod commands;
pub mod content;
pub mod health;
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
pub mod onboarding;
pub mod public;
pub mod readyz;
pub mod scim;
pub mod search;
pub mod security;
pub mod security_headers;
pub mod security_limiter;
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

    // Discovery reads the endpoint and answers a diff; applying it is a second, separate request
    // with its own confirmation, so an endpoint that answers with a surprise cannot rewrite the
    // registry just because somebody pressed Discover (REQ-097 slice 2).
    let ai_provider_discover =
        post(ai::discover_provider_models).layer(guards::require(&state, "ai.providers.manage"));
    let ai_provider_apply_discovery =
        post(ai::apply_provider_discovery).layer(guards::require(&state, "ai.providers.manage"));

    let ai_models = get(ai::list_models).layer(guards::require(&state, "ai.providers.read"));

    // The provider form's own vocabulary: which protocols exist and what ranges it validates
    // against, read with the provider list it drives (REQ-097 slice 1).
    let ai_protocols = get(ai::list_protocols).layer(guards::require(&state, "ai.providers.read"));

    // The connection test dials a provider on the operator's behalf, so it is a `manage` power.
    let ai_provider_test =
        post(ai::test_provider_connection).layer(guards::require(&state, "ai.providers.manage"));

    let ai_model = patch(ai::update_model).layer(guards::require(&state, "ai.providers.manage"));

    // Health, usage and the failover chain (REQ-097 slice 3). Reading a provider's health is the
    // same power as reading the provider list, so a team that can see the connection can see
    // whether it works. "Probe now" dials the endpoint and reorders the chain, so both are
    // `manage` — a reader must not be able to spend the installation's quota or reroute traffic.
    let ai_provider_health =
        get(ai::provider_health).layer(guards::require(&state, "ai.providers.read"));
    let ai_provider_usage =
        get(ai::provider_usage).layer(guards::require(&state, "ai.providers.read"));
    let ai_provider_probe =
        post(ai::probe_provider).layer(guards::require(&state, "ai.providers.manage"));
    let ai_failover = get(ai::failover_chain)
        .layer(guards::require(&state, "ai.providers.read"))
        .merge(
            put(ai::set_failover_order).layer(guards::require(&state, "ai.providers.manage")),
        );

    let ai_chat = post(ai::chat).layer(guards::require(&state, "ai.chat"));

    // Task routing and feature overrides (REQ-098 slice 2). Reading a route map is the same
    // knowledge as the provider list — which models exist and what they can do — so it is
    // `ai.providers.read`. *Rewriting* it is `ai.settings.manage`, a separate power on purpose:
    // a reader can see how traffic is routed and still not be able to redirect it.
    //
    // The dry run is a `read` and does nothing but read: it resolves a hypothetical request
    // against the stored maps, so an operator can ask "what would this do today" without
    // spending quota or changing a row.
    let ai_routing = get(ai_routing::get_routing)
        .layer(guards::require(&state, "ai.providers.read"))
        .merge(put(ai_routing::put_routing).layer(guards::require(&state, "ai.settings.manage")));
    let ai_routing_overrides = get(ai_routing::get_overrides)
        .layer(guards::require(&state, "ai.providers.read"))
        .merge(
            put(ai_routing::put_override).layer(guards::require(&state, "ai.settings.manage")),
        );
    let ai_routing_preview =
        post(ai_routing::preview_routing).layer(guards::require(&state, "ai.providers.read"));

    // The decision log (REQ-098 slice 3). `ai.usage.read`, deliberately not `ai.providers.read`:
    // the log is the accounting trail of what the platform asked of its providers, so a reader
    // of "which models are connected" has no business reading an organization's per-request
    // history. One key, shared with the cost manager (REQ-104), rather than two spellings.
    let ai_decisions = get(ai_decisions::list_decisions)
        .layer(guards::require(&state, "ai.usage.read"));
    let ai_decision = get(ai_decisions::get_decision)
        .layer(guards::require(&state, "ai.usage.read"));
    let ai_decisions_csv = get(ai_decisions::export_decisions)
        .layer(guards::require(&state, "ai.usage.read"));
    // "What cannot resolve" is a routing question, not a usage one: it answers from the log but
    // belongs to the routing screen's warning banner, which a provider reader must be able to
    // see — otherwise the only people who can tell that a task is broken are the ones who
    // already cannot fix it.
    let ai_unresolved = get(ai_decisions::get_unresolved)
        .layer(guards::require(&state, "ai.providers.read"));
    let ai_last_resolved = get(ai_decisions::last_resolved)
        .layer(guards::require(&state, "ai.providers.read"));

    // The agent runtime (REQ-099 slice 1). Three powers, because the three are genuinely
    // different: *seeing* an agent is knowing how the installation's AI is configured, *changing*
    // one is a write, and *running* one spends the installation's money and acts on its behalf.
    // Collapsing run into read would let anybody who can see a prompt also press Run; collapsing
    // manage into read would let a reader re-point an agent at a different model.
    let ai_agents = get(ai_agents::list_agents_route)
        .layer(guards::require(&state, "ai.agents.read"))
        .merge(
            post(ai_agents::create_agent_route).layer(guards::require(&state, "ai.agents.manage")),
        );
    let ai_agent = get(ai_agents::get_agent_route)
        .layer(guards::require(&state, "ai.agents.read"))
        .merge(
            patch(ai_agents::patch_agent_route).layer(guards::require(&state, "ai.agents.manage")),
        )
        .merge(
            delete(ai_agents::delete_agent_route).layer(guards::require(&state, "ai.agents.manage")),
        );
    let ai_agent_runs =
        post(ai_agents::start_run).layer(guards::require(&state, "ai.agents.run"));
    // The telemetry reads (REQ-099 slice 4). Both are `ai.agents.read` rather than a new key:
    // they read the runs and steps an `ai.agents.read` caller can already list, so a separate
    // permission would be a key an operator has to remember for a sum of columns they can add
    // up themselves.
    let ai_agent_telemetry = get(ai_agents::agent_telemetry_route)
        .layer(guards::require(&state, "ai.agents.read"));
    let ai_tool_usage =
        get(ai_agents::tool_usage_route).layer(guards::require(&state, "ai.agents.read"));
    // The skills registry (REQ-099, slice 3). Read and write are separate keys for the same
    // reason agents have: writing a skill means writing text that lands in every prompt an
    // attached agent sends, which is a different act from reading a list of them.
    let ai_skills = get(ai_skills::list_skills_route)
        .layer(guards::require(&state, "ai.skills.read"))
        .merge(post(ai_skills::create_skill_route).layer(guards::require(&state, "ai.skills.manage")));
    // `POST /ai/skills/{key}/validate`, **not** a second POST on `/ai/skills`. The handler has
    // always been written for the keyed path — the spec's own table says so — and the router was
    // the only place that disagreed. axum does not accept two `POST` routes on one path: it
    // panics at *router construction*, so the whole API refused to start and the symptom was
    // "the API did not answer", with `Overlapping method route` naming a line in a file whose
    // other 1600 lines are fine. This is slice 3's own defect, found by the first pass that
    // ever got far enough to boot the binary: the skill walks had run against a *test* binary
    // built with the routes module, and the unit suite never builds a router at all.
    let ai_skill_validate = post(ai_skills::validate_skill_route)
        .layer(guards::require(&state, "ai.skills.manage"));
    let ai_skill = get(ai_skills::get_skill_route)
        .layer(guards::require(&state, "ai.skills.read"))
        .merge(
            axum::routing::patch(ai_skills::update_skill_route)
                .layer(guards::require(&state, "ai.skills.manage")),
        )
        .merge(
            axum::routing::delete(ai_skills::delete_skill_route)
                .layer(guards::require(&state, "ai.skills.manage")),
        )
        .merge(
            post(ai_skills::validate_skill_route)
                .layer(guards::require(&state, "ai.skills.manage")),
        );
    // Attach/order/detach. Read is enough to *see* an agent's skills, but attaching is a
    // change to what its next prompt contains, so it takes the manage key.
    let ai_agent_skills = get(ai_skills::list_agent_skills_route)
        .layer(guards::require(&state, "ai.skills.read"))
        .merge(
            post(ai_skills::attach_skill_route)
                .layer(guards::require(&state, "ai.skills.manage")),
        )
        .merge(
            axum::routing::put(ai_skills::set_agent_skills_route)
                .layer(guards::require(&state, "ai.skills.manage")),
        );
    let ai_agent_skill =
        axum::routing::delete(ai_skills::detach_skill_route)
            .layer(guards::require(&state, "ai.skills.manage"));
    // The tool registry (REQ-100 slice 1). Read and write are separate keys because they are
    // separate acts: *seeing* the registry is knowing which actions the installation's AI can
    // take, and *changing* it is changing what a model will be allowed to do. Collapsing them
    // would give every reader of a schema the power to un-gate `deployment.deploy`.
    let ai_tools =
        get(ai_tools::list_tools_route).layer(guards::require(&state, "ai.tools.read"));
    // `/ai/tools/classes` is a STATIC table, not a parameterised path, and axum panics at router
    // construction when two routes on one prefix disagree about arity — the failure surfaces as
    // "the API did not answer" with `Overlapping method route` naming a line in a 1700-line
    // file. A distinct prefix is the shape that cannot collide, and it is also honest: the class
    // list is not a tool.
    let ai_tool_classes =
        get(ai_tools::tool_classes_route).layer(guards::require(&state, "ai.tools.read"));
    let ai_tool = get(ai_tools::get_tool_route)
        .layer(guards::require(&state, "ai.tools.read"))
        .merge(
            axum::routing::patch(ai_tools::patch_tool_route)
                .layer(guards::require(&state, "ai.tools.manage")),
        );
    // The usage chart is a read of the same call log the detail screen lists, so it stays
    // `ai.tools.read` — a separate key would be a permission an operator has to remember for a
    // sum of columns they could add up themselves.
    //
    // Named `ai_tool_registry_usage`, not `ai_tool_usage`: the binding above already belongs to
    // REQ-099's `/ai/agents/{id}/tool-usage`, and two `let`s of one name in one function is
    // E0283 — "type annotations needed for MethodRouter" — which names neither of the two
    // routes that caused it.
    let ai_tool_registry_usage =
        get(ai_tools::tool_usage_route).layer(guards::require(&state, "ai.tools.read"));
    // The grants of ONE tool, on the tool's own path. The read is `ai.tools.read` for the same
    // reason the usage chart is: "who has an opinion about this tool" is a fact about the tool,
    // and the matrix already answers the same question from the identity side. The write is
    // `ai.tools.manage` rather than `ai.identities.manage` because the caller is editing the
    // tool's row in this screen — and a permission split that follows the screen is one an
    // operator can predict from where they clicked.
    //
    // Registered under its own literal path so it cannot be read as a tool key, the same
    // collision the classes route and the matrix route each document.
    let ai_tool_grants = get(ai_tools::get_tool_grants_route)
        .layer(guards::require(&state, "ai.tools.read"))
        .merge(
            axum::routing::put(ai_tools::put_tool_grants_route)
                .layer(guards::require(&state, "ai.tools.manage")),
        );

    // AI identities and the permission matrix (REQ-100 slice 2). The read/manage split is the
    // point: seeing which tools an installation's AI may take is a *fact about the platform*,
    // while changing what a run may do is a decision somebody has to be accountable for. The
    // matrix is `ai.tools.read` rather than `ai.identities.read` on purpose — it renders the
    // registry (rows) and the grants (columns) in one view, and a viewer who can read the tools
    // can see the whole picture; a viewer who cannot still gets the rows' descriptions from
    // `/ai/tools` alone.
    let ai_identities = get(ai_identities::list_identities_route)
        .layer(guards::require(&state, "ai.identities.read"))
        .merge(
            post(ai_identities::create_identity_route)
                .layer(guards::require(&state, "ai.identities.manage")),
        );
    let ai_identity = get(ai_identities::get_identity_route)
        .layer(guards::require(&state, "ai.identities.read"))
        .merge(
            axum::routing::patch(ai_identities::patch_identity_route)
                .layer(guards::require(&state, "ai.identities.manage"))
                .merge(
                    axum::routing::delete(ai_identities::delete_identity_route)
                        .layer(guards::require(&state, "ai.identities.manage")),
                ),
        );
    // The grant map is a *replace*, so it is guarded as a manage and not as a read: a PUT that
    // swaps twenty cells is exactly as consequential as editing twenty fields of a form.
    let ai_identity_tools = get(ai_identities::get_identity_tools_route)
        .layer(guards::require(&state, "ai.identities.read"))
        .merge(
            axum::routing::put(ai_identities::put_identity_tools_route)
                .layer(guards::require(&state, "ai.identities.manage")),
        );
    // An agent's own allow-list. Reading it is reading the agent (`ai.agents.read`); replacing it
    // is managing the agent, which is where the risk lives — an allow-list is the only thing
    // between an agent and `deployment.deploy`.
    let ai_agent_tool_set = get(ai_identities::get_agent_tools_route)
        .layer(guards::require(&state, "ai.agents.read"))
        .merge(
            axum::routing::put(ai_identities::put_agent_tools_route)
                .layer(guards::require(&state, "ai.agents.manage")),
        );
    let ai_permissions_matrix =
        get(ai_identities::permission_matrix_route).layer(guards::require(&state, "ai.tools.read"));

    let ai_runs = get(ai_agents::list_runs_route).layer(guards::require(&state, "ai.agents.read"));
    let ai_run = get(ai_agents::get_run_route).layer(guards::require(&state, "ai.agents.read"));
    let ai_run_steps = get(ai_agents::get_run_steps).layer(guards::require(&state, "ai.agents.read"));
    let ai_run_events = get(ai_agents::run_events).layer(guards::require(&state, "ai.agents.read"));
    let ai_run_agent = get(ai_agents::run_agent_link).layer(guards::require(&state, "ai.agents.read"));
    let ai_run_cancel =
        post(ai_agents::cancel_run).layer(guards::require(&state, "ai.agents.run"));
    let ai_run_resume =
        post(ai_agents::resume_run).layer(guards::require(&state, "ai.agents.run"));

    // The per-agent workspace (REQ-099 slice 2). Reading a workspace is reading the agent;
    // adding or removing a file is managing it, and it is `ai.agents.manage` rather than
    // `ai.agents.run` on purpose — a run is what *spends*, and a workspace file is an input to
    // a run somebody still has to press Run for. The download carries the same read power as
    // the listing for the obvious reason: a file the tab shows is a file the tab can fetch.
    let ai_agent_files = get(ai_agent_workspace::list_agent_files)
        .layer(guards::require(&state, "ai.agents.read"))
        .merge(
            post(ai_agent_workspace::upload_agent_file)
                .layer(guards::require(&state, "ai.agents.manage")),
        );
    let ai_agent_file = get(ai_agent_workspace::download_agent_file)
        .layer(guards::require(&state, "ai.agents.read"))
        .merge(
            delete(ai_agent_workspace::delete_agent_file)
                .layer(guards::require(&state, "ai.agents.manage")),
        );

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
        // Slice 3's four sub-routers, merged rather than spelled out route by route. Each is a
        // `Router` with its own `route_layer`, so the guard travels with the group and a future
        // fifth endpoint joins the right one by being added inside its block.
        .merge(notifications_push)
        .route("/notifications/channels", notifications_channels)
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
        .route(
            "/ai/providers/{id}/apply-discovery",
            ai_provider_apply_discovery,
        )
        .route("/ai/protocols", ai_protocols)
        .route("/ai/providers/{id}/test", ai_provider_test)
        .route("/ai/providers/{id}/health", ai_provider_health)
        .route("/ai/providers/{id}/usage", ai_provider_usage)
        .route("/ai/providers/{id}/probe", ai_provider_probe)
        .route("/ai/failover", ai_failover)
        .route("/ai/models", ai_models)
        .route("/ai/models/{id}", ai_model)
        .route("/ai/chat", ai_chat)
        .route("/ai/routing", ai_routing)
        .route("/ai/routing/overrides", ai_routing_overrides)
        .route("/ai/routing/preview", ai_routing_preview)
        .route("/ai/logs/decisions", ai_decisions)
        .route("/ai/logs/decisions.csv", ai_decisions_csv)
        .route("/ai/logs/decisions/{id}", ai_decision)
        .route("/ai/routing/unresolved", ai_unresolved)
        .route("/ai/routing/last-resolved", ai_last_resolved)
        // The agent runtime (REQ-099). `/ai/agents/{id}/runs` is a POST that answers as an event
        // stream, and `/ai/runs/{id}/events` re-attaches to a run that is still going — the two
        // are the only GET/POST pair here that share a path prefix, so they are registered in
        // order rather than merged: `axum` matches a literal segment before a capture, and
        // `/ai/runs/{id}` would otherwise swallow `/ai/runs/{id}/events`.
        // Telemetry (REQ-099 slice 4). Two reads, both `ai.agents.read`: the roll-up for one
        // agent and the tenant-wide tool usage. The per-agent route is registered *before*
        // `/ai/agents/{id}`'s siblings are consulted because axum matches a literal segment
        // before a capture, so `/ai/agents/{id}/telemetry` cannot be reached by any other
        // shape.
        .route("/ai/agents/{id}/telemetry", ai_agent_telemetry)
        .route("/ai/telemetry/tools", ai_tool_usage)
        .route("/ai/agents", ai_agents)
        .route("/ai/agents/{id}", ai_agent)
        .route("/ai/agents/{id}/runs", ai_agent_runs)
        // The workspace file path is a wildcard, so `{*path}` rather than `{path}`: a workspace
        // holds `data/2026/q3.csv` as readily as `notes.md`, and a single-segment capture would
        // answer 404 for every file in a subdirectory.
        //
        // The braces are load-bearing and the version is why. axum 0.8 removed the bare `*name`
        // syntax outright: a segment that starts with `*` now panics **at router construction**,
        // so the whole API refused to start rather than this one route 404ing. The panic names
        // the fix, and the cost of the mistake is a stack of identical restarts in pm2 rather
        // than a visible error.
        .route("/ai/agents/{id}/files", ai_agent_files)
        .route("/ai/agents/{id}/files/{*path}", ai_agent_file)
        .route("/ai/skills", ai_skills)
        .route("/ai/skills/{key}", ai_skill)
        // Registered before `/ai/agents/{id}/skills/{key}` for the same reason the other AI
        // routes are: a literal segment outranks a capture, so this keeps its own path.
        .route("/ai/skills/{key}/validate", ai_skill_validate)
        // The tool registry (REQ-100 slice 1). `/ai/tools/classes` is registered BEFORE
        // `/ai/tools/{key}` for the same reason the skills routes are: axum prefers a literal
        // segment over a capture, so the static path keeps its own handler instead of being
        // read as a tool whose key is "classes".
        .route("/ai/tools/classes", ai_tool_classes)
        .route("/ai/tools/{key}/usage", ai_tool_registry_usage)
        .route("/ai/tools/{key}/grants", ai_tool_grants)
        .route("/ai/tools", ai_tools)
        .route("/ai/tools/{key}", ai_tool)
        // The identities and the matrix (REQ-100 slice 2). `/ai/permissions/matrix` is
        // registered under its own literal prefix rather than as `/ai/permissions/{key}`: axum
        // prefers a literal segment over a capture, and a capture here would read "matrix" as a
        // permission name — the same collision the `/ai/tools/classes` comment above describes,
        // reproduced because the shape is easy to reach for a second time.
        .route("/ai/permissions/matrix", ai_permissions_matrix)
        .route("/ai/identities", ai_identities)
        .route("/ai/identities/{id}", ai_identity)
        .route("/ai/identities/{id}/tools", ai_identity_tools)
        .route("/ai/agents/{id}/tools", ai_agent_tool_set)
        .route("/ai/agents/{id}/skills", ai_agent_skills)
        .route("/ai/agents/{id}/skills/{key}", ai_agent_skill)
        .route("/ai/runs", ai_runs)
        .route("/ai/runs/{id}", ai_run)
        .route("/ai/runs/{id}/steps", ai_run_steps)
        .route("/ai/runs/{id}/events", ai_run_events)
        .route("/ai/runs/{id}/agent", ai_run_agent)
        .route("/ai/runs/{id}/cancel", ai_run_cancel)
        .route("/ai/runs/{id}/resume", ai_run_resume)
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

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(readyz::readyz))
        .nest("/api/v1", v1)
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
        // CSRF sits OUTSIDE the permission guards on purpose: a guard answers 401 for a request
        // with no session and 403 for one whose account lacks the key. The CSRF layer's answer is
        // about the *request*, and it has to be reached only by a request that actually
        // authenticated - which is what the guards having run first guarantees.
        .layer(crate::headers_middleware::require_csrf(&state))
        .layer(header_layer.clone())
        .with_state(state)
}
