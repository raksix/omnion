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

pub mod theme_layouts;
pub mod theme_settings;
pub mod themes;
pub mod ai;
pub mod analytics;
pub mod auth;
pub mod automation;
pub mod backups;
pub mod blocks;
pub mod commands;
pub mod comments;
pub mod content;
pub mod featured_media;
pub mod forms;
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
pub mod menus;
pub mod members;
pub mod newsletter;
pub mod notifications;
pub mod notifications_admin;
pub mod onboarding;
pub mod patterns;
pub mod public;
pub mod readyz;
pub mod scim;
pub mod search;
pub mod security;
pub mod security_headers;
pub mod security_limiter;
pub mod seo;
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
    // The block registry (REQ-063, slice 1): the registry document and the dry-run validator
    // both change nothing, so they read with `content.blocks.read` — the power an editor needs
    // to author against the registry at all. Writing a block tree is `content.pages.update` on
    // the page it belongs to, so the two halves of the editor carry the two keys that mean
    // something: "may I see what I can build" and "may I change this page".
    let blocks_registry =
        get(blocks::list_blocks).layer(guards::require(&state, "content.blocks.read"));
    let blocks_validate =
        post(blocks::validate_blocks).layer(guards::require(&state, "content.blocks.read"));

    // Patterns and page templates (REQ-063, slice 3). Both libraries read with the block read
    // key — what an author may build is not a privilege — but writing is a separate key from
    // editing a page, because a pattern outlives the page it was cut from and is reused across
    // every site of the organization. Building from a template is the other direction: it needs
    // `content.pages.create` (it creates a page) and no curation power at all.
    let patterns_list =
        get(patterns::list_patterns).layer(guards::require(&state, "content.blocks.read"));
    let patterns_save =
        post(patterns::save_pattern).layer(guards::require(&state, "content.patterns.manage"));
    let templates_list =
        get(patterns::list_templates).layer(guards::require(&state, "content.blocks.read"));
    let templates_save =
        post(patterns::save_template).layer(guards::require(&state, "content.templates.manage"));
    let page_from_template = post(patterns::create_page_from_template)
        .layer(guards::require(&state, "content.pages.create"));

    let pattern = get(patterns::get_pattern)
        .layer(guards::require(&state, "content.blocks.read"))
        .merge(
            put(patterns::update_pattern).layer(guards::require(&state, "content.patterns.manage")),
        )
        .merge(
            delete(patterns::delete_pattern)
                .layer(guards::require(&state, "content.patterns.manage")),
        );

    // The insert path reads a pattern's blocks with ids already fresh for the page they are
    // going into, so it carries the read key — inserting is authoring, not curation.
    let pattern_blocks =
        get(patterns::get_pattern_blocks).layer(guards::require(&state, "content.blocks.read"));

    let template = delete(patterns::delete_template)
        .layer(guards::require(&state, "content.templates.manage"));

    let pages = get(content::list_pages)
        .layer(guards::require(&state, "content.pages.read"))
        .merge(post(content::create_page).layer(guards::require(&state, "content.pages.create")));

    let page = get(content::get_page)
        .layer(guards::require(&state, "content.pages.read"))
        .merge(patch(content::update_page).layer(guards::require(&state, "content.pages.update")))
        .merge(delete(content::delete_page).layer(guards::require(&state, "content.pages.delete")));

    let page_publish =
        post(content::publish_page).layer(guards::require(&state, "content.pages.publish"));

    // REQ-063 slice 2: the preview frame reads the *draft* of a page and hands the panel a tree
    // the server has already filtered for the chosen viewport. It changes nothing, so it carries
    // the same read key as opening the page — a frame that needed a second permission would only
    // teach authors to skip the one screen that shows the phone render.
    let page_preview =
        get(content::preview_page).layer(guards::require(&state, "content.pages.read"));

    let page_restore =
        post(content::restore_revision).layer(guards::require(&state, "content.pages.restore"));

    let page_revisions =
        get(content::list_revisions).layer(guards::require(&state, "content.pages.read"));

    let page_revision =
        get(content::get_revision).layer(guards::require(&state, "content.pages.read"));

    // REQ-063: the block-level compare two revisions, on the same read key as reading either
    // of them — looking at a history is `content.pages.read`, and needing a second permission to
    // ask what changed inside it would only teach authors to restore instead of compare.
    let page_revision_diff =
        get(content::diff_revision).layer(guards::require(&state, "content.pages.read"));

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
    let backup_schedules_read: MethodRouter<AppState, Infallible> =
        get(backups::list_schedules).layer(guards::require(&state, "backup.read"));
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

    // Menus (REQ-064, slice 1). Reading a menu is `menus.read` — an editor needs to see the
    // navigation before deciding anything about it — and every write is `menus.manage`, which
    // is a genuinely separate power: an account that may publish a page should not thereby
    // rewrite the site's header. The public payload carries no guard at all, because a theme
    // that needs a session to draw its navigation cannot be rendered by anything.
    let menus_list = get(menus::list_menus).layer(guards::require(&state, "menus.read"));
    let menus_create = post(menus::create_menu).layer(guards::require(&state, "menus.manage"));
    let menu_read = get(menus::get_menu).layer(guards::require(&state, "menus.read"));
    let menu_write = put(menus::update_menu)
        .layer(guards::require(&state, "menus.manage"))
        .merge(delete(menus::delete_menu).layer(guards::require(&state, "menus.manage")));
    let menu_items_write =
        put(menus::save_menu_items).layer(guards::require(&state, "menus.manage"));
    let menu_items_from_pages =
        post(menus::add_pages_to_menu).layer(guards::require(&state, "menus.manage"));
    // The rendered menu a theme draws. Unauthenticated by nature, and audience-filtered by
    // query — see `menus::public_menu`.
    let public_menu = get(menus::public_menu);

    // Scheduled publishing (REQ-064, slice 1). One key for the whole queue: reading it, moving
    // an entry and cancelling one are the same power a publisher already has
    // (`content.pages.schedule`), and a second key would only answer "who may look at the
    // queue" separately from "who may change it" without making either safer.
    let publishing_queue =
        get(menus::list_queue).layer(guards::require(&state, "content.pages.schedule"));
    let publishing_entry =
        put(menus::reschedule_entry).layer(guards::require(&state, "content.pages.schedule"));
    let publishing_entry_cancel =
        post(menus::cancel_entry).layer(guards::require(&state, "content.pages.schedule"));
    let publishing_entry_now =
        post(menus::publish_now).layer(guards::require(&state, "content.pages.schedule"));
    let publishing_entry_retry =
        post(menus::retry_entry).layer(guards::require(&state, "content.pages.schedule"));
    let page_schedule =
        post(menus::schedule_page).layer(guards::require(&state, "content.pages.schedule"));

    // Forms (REQ-064, slice 2). Three powers, not two: `forms.read` draws the builder and the
    // list, `forms.manage` writes the definition, and `forms.submissions.read` reads what
    // visitors sent. The third key is the one that matters — a person who may design the form
    // has no business reading the answers, and every owner of a contact form has been that
    // person at some point. The public submit route carries no guard at all: it is the endpoint
    // a stranger's browser posts to.
    let forms_list = get(forms::list_forms).layer(guards::require(&state, "forms.read"));
    let forms_create = post(forms::create_form).layer(guards::require(&state, "forms.manage"));
    let form_read = get(forms::get_form).layer(guards::require(&state, "forms.read"));
    let form_write = put(forms::update_form)
        .layer(guards::require(&state, "forms.manage"))
        .merge(delete(forms::delete_form).layer(guards::require(&state, "forms.manage")));
    let form_fields = put(forms::save_form_fields).layer(guards::require(&state, "forms.manage"));
    let form_publish = post(forms::set_form_status).layer(guards::require(&state, "forms.manage"));
    // The inbox: reading it AND changing a row's state take the same key, because an inbox you
    // may read but not act on is a screen with buttons that answer 403.
    let submissions_list =
        get(forms::list_submissions).layer(guards::require(&state, "forms.submissions.read"));
    let submissions_bulk = patch(forms::bulk_submission_status)
        .layer(guards::require(&state, "forms.submissions.read"));
    let submissions_export =
        get(forms::export_submissions).layer(guards::require(&state, "forms.submissions.read"));
    let submission_read =
        get(forms::get_submission).layer(guards::require(&state, "forms.submissions.read"));
    let submission_write = patch(forms::set_submission_status)
        .layer(guards::require(&state, "forms.submissions.read"))
        .merge(
            delete(forms::delete_submission)
                .layer(guards::require(&state, "forms.submissions.read")),
        );
    // Unauthenticated, like the rendered menu: a form on a live page is posted to by browsers
    // that have no account on this installation.
    let public_form_submit = post(forms::public_submit);

    // SEO toolkit (REQ-064, slice 3). TWO powers, and the split is the same one the CMS has
    // drawn everywhere: `seo.read` looks at a site's search setup, `seo.manage` changes it.
    // The public routes below carry no guard at all — a sitemap, a robots.txt and a redirect are
    // what a crawler asks for before it has any account anywhere.
    let seo_overview = get(seo::get_seo_overview).layer(guards::require(&state, "seo.read"));
    let page_seo_read = get(seo::get_page_seo).layer(guards::require(&state, "seo.read"));
    let page_seo_write = put(seo::put_page_seo).layer(guards::require(&state, "seo.manage"));

    // A page's featured image (REQ-064, slice 4d). **Three guards across two routes, and the
    // first version had two and got it wrong.** The mistake was guarding the page's own read with
    // `media.read` "because the body mentions a file" — which locked a page editor who can edit
    // the page's title, body and crop out of reading the alt they are required to write. The three
    // questions are genuinely different and each gets its own key:
    //
    //   * reading THIS page's image      → `content.pages.read`   — it is the page's own field
    //   * writing it                     → `content.pages.update` — the same power as its body
    //   * listing what it could point at  → `media.read`           — that is a question about the
    //     LIBRARY, and it is the one that can enumerate files rather than name one
    //
    // The picker is a separate route for the third reason alone: one endpoint serving both would
    // need one guard for both, and the weaker one wins — either an editor with an empty picker or
    // a page editor holding the whole library.
    let featured_read =
        get(featured_media::get_featured_media).layer(guards::require(&state, "content.pages.read"));
    let featured_write = put(featured_media::put_featured_media)
        .layer(guards::require(&state, "content.pages.update"));
    let featured_candidates =
        get(featured_media::list_candidates).layer(guards::require(&state, "media.read"));
    // The theme gallery (REQ-062 slice 1). Two read permissions and one write, and the
    // split follows the question each one answers rather than the verb: the gallery is a
    // listing (`themes.read`), the single theme is the renderer's own lookup, and activation
    // is the only thing on this surface that changes what a signed-out visitor sees — so it
    // carries its own key and cannot be reached by a `themes.read` account.
    let themes_gallery =
        get(themes::list_gallery).layer(guards::require(&state, "themes.read"));
    let theme_read = get(themes::get_theme).layer(guards::require(&state, "themes.read"));
    let theme_activate =
        post(themes::activate_theme).layer(guards::require(&state, "themes.activate"));
    let theme_rollback =
        post(themes::rollback_theme).layer(guards::require(&state, "themes.activate"));
    // Theme settings (REQ-062 slice 2). Read is `themes.read` because the history screen is a
    // reading; every write is `themes.customize`, which is deliberately NOT implied by
    // `themes.activate`. The save and the publish are different powers too — a designer who
    // may stage a palette but not put it in front of visitors is a real setup, so the split
    // is by *what the write does*, not by which table it touches.
    let theme_settings_read =
        get(theme_settings::read_settings).layer(guards::require(&state, "themes.read"));
    let theme_settings_save = put(theme_settings::save_settings)
        .layer(guards::require(&state, "themes.customize"));
    let theme_settings_publish = post(theme_settings::publish_settings)
        .layer(guards::require(&state, "themes.customize"));
    let theme_settings_revisions = get(theme_settings::list_revisions)
        .layer(guards::require(&state, "themes.read"));
    let theme_settings_revision = get(theme_settings::read_revision)
        .layer(guards::require(&state, "themes.read"));
    let theme_settings_restore = post(theme_settings::restore_revision)
        .layer(guards::require(&state, "themes.customize"));
    // Theme layouts and packages (REQ-062 slice 3). The builder reads with `themes.read` and
    // writes with `themes.customize`, exactly as the settings surface does — a slot and a
    // colour token are the same power over the same site.
    //
    // The package routes are `themes.install`/`themes.export` and NOT `themes.customize`,
    // because they are not about one site: a package crosses sites, and an account that may
    // restyle the site it is on has no business writing a row every site in the installation
    // can see. The validate route carries `themes.install` too — a dry run that an editor
    // cannot perform is a dry run they guess at instead.
    let theme_layouts_read =
        get(theme_layouts::read_layouts).layer(guards::require(&state, "themes.read"));
    let theme_layout_slot_read =
        get(theme_layouts::read_slot).layer(guards::require(&state, "themes.read"));
    let theme_layout_slot_save = put(theme_layouts::save_slot)
        .layer(guards::require(&state, "themes.customize"));
    let theme_layout_slot_reset = post(theme_layouts::reset_slot)
        .layer(guards::require(&state, "themes.customize"));
    let theme_package_export =
        get(theme_layouts::export_package).layer(guards::require(&state, "themes.export"));
    let theme_package_validate =
        post(theme_layouts::validate_package).layer(guards::require(&state, "themes.install"));
    let theme_package_install =
        post(theme_layouts::install_package).layer(guards::require(&state, "themes.install"));
    let theme_remove = delete(theme_layouts::remove_theme)
        .layer(guards::require(&state, "themes.install"));
    let seo_redirect_create =
        post(seo::create_redirect).layer(guards::require(&state, "seo.manage"));
    let seo_redirect_write = put(seo::update_redirect)
        .layer(guards::require(&state, "seo.manage"))
        .merge(delete(seo::delete_redirect).layer(guards::require(&state, "seo.manage")));
    let seo_redirect_test = post(seo::test_redirect).layer(guards::require(&state, "seo.read"));
    // The import writes rules, so it carries `seo.manage` like every other write on this surface
    // — a *read* guard would have let any account that may look at the rules paste a file into
    // the table. The export is the read it is: a caller who may see the rules may take them away.
    let seo_redirect_import =
        post(seo::import_redirects).layer(guards::require(&state, "seo.manage"));
    let seo_redirect_export = get(seo::export_redirects).layer(guards::require(&state, "seo.read"));
    let seo_settings_write = put(seo::put_settings).layer(guards::require(&state, "seo.manage"));
    let seo_sitemap_regenerate =
        post(seo::regenerate_sitemap).layer(guards::require(&state, "seo.manage"));
    let seo_broken_read = get(seo::list_broken_links).layer(guards::require(&state, "seo.read"));
    let seo_broken_scan = post(seo::scan_broken_links).layer(guards::require(&state, "seo.manage"));
    let seo_broken_write =
        patch(seo::set_broken_link_ignored).layer(guards::require(&state, "seo.manage"));
    // Unauthenticated by nature — see the note above.
    let public_sitemap = get(seo::public_sitemap);
    let public_robots = get(seo::public_robots);
    let public_redirect = get(seo::public_redirect);

    // Newsletter (REQ-064, slice 4b). Two powers, and the public surface underneath them: the
    // signup, the confirmation click and the unsubscribe click are the only unauthenticated
    // routes in this module, and each of them writes to somebody else's inbox — so the confirm
    // and unsubscribe tokens are the whole security surface, hashed and single-use.
    let newsletter_lists_read =
        get(newsletter::list_lists).layer(guards::require(&state, "newsletter.read"));
    let newsletter_lists_write = post(newsletter::create_list)
        .layer(guards::require(&state, "newsletter.manage"));
    let newsletter_list_read =
        get(newsletter::get_list).layer(guards::require(&state, "newsletter.read"));
    let newsletter_list_write = put(newsletter::patch_list)
        .layer(guards::require(&state, "newsletter.manage"))
        .merge(delete(newsletter::delete_list).layer(guards::require(&state, "newsletter.manage")));
    let newsletter_subscribers_read =
        get(newsletter::list_subscribers).layer(guards::require(&state, "newsletter.read"));
    // TWO separate POST method routers on purpose: they serve DIFFERENT paths
    // (`/newsletter/lists/{id}/subscribers` and `/newsletter/lists/{id}/import`), and merging
    // two POST routers panics at router construction with "Overlapping method route" — which
    // takes down the whole application, not just this screen. A method router is a per-PATH
    // thing; the paths are what decide.
    let newsletter_list_subscribers_write =
        post(newsletter::add_subscriber).layer(guards::require(&state, "newsletter.manage"));
    let newsletter_list_import = post(newsletter::import_subscribers)
        .layer(guards::require(&state, "newsletter.manage"));
    let newsletter_subscriber_read =
        get(newsletter::get_subscriber).layer(guards::require(&state, "newsletter.read"));
    let newsletter_subscriber_write = patch(newsletter::set_subscriber_status)
        .layer(guards::require(&state, "newsletter.manage"))
        .merge(
            delete(newsletter::delete_subscriber)
                .layer(guards::require(&state, "newsletter.manage")),
        );
    let newsletter_export =
        get(newsletter::export_subscribers).layer(guards::require(&state, "newsletter.read"));
    let newsletter_issues_read =
        get(newsletter::list_issues).layer(guards::require(&state, "newsletter.read"));
    let newsletter_issues_write =
        post(newsletter::send_issue).layer(guards::require(&state, "newsletter.manage"));

    let public_newsletter_subscribe = post(newsletter::public_subscribe);
    let public_newsletter_confirm = get(newsletter::public_confirm);
    let public_newsletter_unsubscribe = get(newsletter::public_unsubscribe);
    let public_newsletter_issue = get(newsletter::public_issue);
    let public_newsletter_lists = get(newsletter::public_lists);

    // Memberships (REQ-064, slice 4c). Two powers, and the split is the point: a member table
    // shows every address on the site, so "somebody may look at it" must not also mean
    // "somebody may block the one member they dislike, mint them a password reset, or delete
    // them". `memberships.manage` additionally owns the SITE's policy, because gating decides
    // who can read which published page.
    //
    // The public half carries no guard and no member session of the platform's own kind: these
    // are the routes a browser with no account on this installation posts to. The literal
    // segments are declared before the parameterised ones so axum ranks `/members/verify`
    // ahead of a hypothetical `/members/{id}`.
    let members_read = get(members::list_members).layer(guards::require(&state, "memberships.read"));
    let members_create =
        post(members::create_member).layer(guards::require(&state, "memberships.manage"));
    let member_read = get(members::get_member).layer(guards::require(&state, "memberships.read"));
    let member_write = patch(members::patch_member)
        .layer(guards::require(&state, "memberships.manage"))
        .merge(
            delete(members::delete_member).layer(guards::require(&state, "memberships.manage")),
        );
    let member_block =
        post(members::block_member).layer(guards::require(&state, "memberships.manage"));
    let member_verify =
        post(members::verify_member).layer(guards::require(&state, "memberships.manage"));
    let member_send_verification = post(members::send_verification)
        .layer(guards::require(&state, "memberships.manage"));
    let member_send_reset =
        post(members::send_reset).layer(guards::require(&state, "memberships.manage"));
    let member_signout_everywhere = post(members::signout_everywhere)
        .layer(guards::require(&state, "memberships.manage"));
    let member_settings_read =
        get(members::get_settings).layer(guards::require(&state, "memberships.read"));
    let member_settings_write =
        put(members::put_settings).layer(guards::require(&state, "memberships.manage"));
    let public_member_signup = post(members::public_signup);
    let public_member_signin = post(members::public_signin);
    let public_member_signout = post(members::public_signout);
    let public_member_me = get(members::public_me).merge(put(members::public_update_me));
    let public_member_verify = get(members::public_verify);
    let public_member_reset = post(members::public_password_reset);
    let public_member_gate = get(members::public_gate);

    // Comments (REQ-064, slice 4a). Two powers and one public surface. `comments.read` opens
    // the inbox, `comments.manage` changes a row and edits the policy — and the two are
    // deliberately separate, because the value of the split is exactly the case where a site
    // hands the inbox to somebody who should not be able to approve a defamatory comment and
    // hand the policy to somebody who should not be able to turn moderation off.
    let comments_inbox = get(comments::list_comments).layer(guards::require(&state, "comments.read"));
    let comments_bulk = post(comments::bulk_moderate).layer(guards::require(&state, "comments.manage"));
    let comment_read = get(comments::get_comment).layer(guards::require(&state, "comments.read"));
    let comment_write = patch(comments::moderate_comment)
        .layer(guards::require(&state, "comments.manage"))
        .merge(delete(comments::delete_comment).layer(guards::require(&state, "comments.manage")));
    let comment_reply =
        post(comments::reply).layer(guards::require(&state, "comments.manage"));
    let comment_settings_read =
        get(comments::get_settings).layer(guards::require(&state, "comments.read"));
    let comment_settings_write =
        put(comments::put_settings).layer(guards::require(&state, "comments.manage"));
    let comment_ban_create =
        post(comments::add_ban).layer(guards::require(&state, "comments.manage"));
    let comment_ban_write =
        delete(comments::remove_ban).layer(guards::require(&state, "comments.manage"));
    // The two public routes carry no guard at all: a comment thread and the form that posts it
    // are read and written by browsers that have no account on this installation.
    let public_comment_thread = get(comments::public_thread);
    let public_comment_submit = post(comments::public_submit);

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
    let analytics_reports = Router::new()
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
        )
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
        .route("/blocks", blocks_registry)
        .route("/blocks/validate", blocks_validate)
        .route("/patterns", patterns_list)
        .route("/patterns", patterns_save)
        .route("/patterns/{id}", pattern)
        .route("/patterns/{id}/blocks", pattern_blocks)
        .route("/page-templates", templates_list)
        .route("/page-templates", templates_save)
        .route("/page-templates/{id}", template)
        .route("/pages/from-template", page_from_template)
        .route("/pages", pages)
        .route("/pages/{id}", page)
        .route("/pages/{id}/publish", page_publish)
        // Menus and the publishing queue (REQ-064, slice 1). The static segments are declared
        // before the parameter ones so axum ranks them ahead of `{id}`.
        .route("/menus", menus_list)
        .route("/menus", menus_create)
        .route("/menus/{id}", menu_read.merge(menu_write))
        .route("/menus/{id}/items", menu_items_write)
        .route("/menus/{id}/items/from-pages", menu_items_from_pages)
        .route("/publishing/queue", publishing_queue)
        .route("/publishing/queue/{id}", publishing_entry)
        .route("/publishing/queue/{id}/cancel", publishing_entry_cancel)
        .route("/publishing/queue/{id}/publish-now", publishing_entry_now)
        .route("/publishing/queue/{id}/retry", publishing_entry_retry)
        .route("/pages/{id}/schedule", page_schedule)
        .route("/public/menus/{location}", public_menu)
        // The three public SEO surfaces. The host is a path parameter rather than a header so
        // they can be cached and proxied like any other file, and so a crawler following a
        // canonical URL lands on the right site without a `Host` header being trusted through a
        // CDN that rewrites it.
        .route("/public/{host}/sitemap.xml", public_sitemap)
        .route("/public/{host}/robots.txt", public_robots)
        .route("/public/{host}/redirect", public_redirect)
        // Forms and their inbox (REQ-064, slice 2). The static segments are declared before the
        // parameter ones so axum ranks them ahead of `{id}` — `/forms/{id}/submissions/export`
        // is a literal, and a route registered after `/forms/{id}/submissions/{sid}` would never
        // be reached.
        .route("/forms", forms_list)
        .route("/forms", forms_create)
        .route("/forms/{id}", form_read.merge(form_write))
        .route("/forms/{id}/fields", form_fields)
        .route("/forms/{id}/publish", form_publish)
        .route("/forms/{id}/submissions", submissions_list)
        .route("/forms/{id}/submissions", submissions_bulk)
        .route("/forms/{id}/submissions/export", submissions_export)
        .route(
            "/forms/{id}/submissions/{sid}",
            submission_read.merge(submission_write),
        )
        .route("/public/forms/{key}/submit", public_form_submit)
        .route("/pages/{id}/preview", page_preview)
        .route("/pages/{id}/restore", page_restore)
        .route("/pages/{id}/revisions", page_revisions)
        .route("/pages/{id}/revisions/{revision_id}", page_revision)
        .route(
            "/pages/{id}/revisions/{revision_id}/diff",
            page_revision_diff,
        )
        .route(
            "/pages/{id}/revisions/{revision_id}/translations",
            page_translations,
        )
        .route(
            "/pages/{id}/revisions/{revision_id}/translations/{language}",
            page_translation,
        )
        // SEO toolkit (REQ-064, slice 3). The page tab hangs off `/pages/{id}/seo` beside the
        // other per-page sub-resources, and the site-wide surface lives under `/seo` with the
        // settings under `/sites/{id}/seo` — three prefixes for one feature, because each names
        // a different scope and a single one would have made a page's SEO a site-level route
        // with a page id in the query.
        .route("/pages/{id}/seo", page_seo_read.merge(page_seo_write))
        .route(
            "/pages/{id}/featured-media",
            featured_read.merge(featured_write),
        )
        .route(
            "/sites/{site_id}/featured-media/candidates",
            featured_candidates,
        )
        .route("/seo/settings", seo_overview)
        .route("/seo/redirects", seo_redirect_create)
        // Before `/seo/redirects/{id}`: axum's matchit would otherwise read "import" as a
        // redirect id and hand the CSV to a handler that expects a UUID, which fails as a 400
        // with a message about the path instead of about the file.
        .route("/seo/redirects/import", seo_redirect_import)
        .route("/seo/redirects/export", seo_redirect_export)
        .route("/seo/redirects/{id}", seo_redirect_write)
        .route("/seo/redirects/{id}/test", seo_redirect_test)
        .route("/sites/{site_id}/seo/settings", seo_settings_write)
        .route(
            "/sites/{site_id}/seo/sitemap/regenerate",
            seo_sitemap_regenerate,
        )
        .route("/seo/broken-links", seo_broken_read.merge(seo_broken_scan))
        .route("/seo/broken-links/{id}", seo_broken_write)
        // Newsletter (REQ-064, slice 4b). The panel is three tables and one archive; the
        // `/public` half is the signup, the two link clicks and the archive page, and none of
        // them carry a session. The literal segments are declared before the parameter ones so
        // axum ranks `/newsletter/subscribers/export` ahead of `/newsletter/subscribers/{id}` —
        // a route registered after a parameterised sibling is unreachable, and the export is
        // the one button an owner reaches for under pressure.
        .route("/newsletter/lists", newsletter_lists_read)
        .route("/newsletter/lists", newsletter_lists_write)
        .route("/newsletter/lists/{id}", newsletter_list_read.merge(newsletter_list_write))
        .route(
            "/newsletter/lists/{id}/subscribers",
            newsletter_list_subscribers_write,
        )
        .route("/newsletter/lists/{id}/import", newsletter_list_import)
        .route("/newsletter/subscribers", newsletter_subscribers_read)
        .route("/newsletter/subscribers/export", newsletter_export)
        .route(
            "/newsletter/subscribers/{id}",
            newsletter_subscriber_read.merge(newsletter_subscriber_write),
        )
        .route("/newsletter/issues", newsletter_issues_read)
        .route("/newsletter/issues", newsletter_issues_write)
        .route("/public/newsletter/lists", public_newsletter_lists)
        .route("/public/newsletter/confirm", public_newsletter_confirm)
        .route("/public/newsletter/unsubscribe", public_newsletter_unsubscribe)
        .route("/public/newsletter/issues/{slug}", public_newsletter_issue)
        .route(
            "/public/newsletter/{key}/subscribe",
            public_newsletter_subscribe,
        )
        // Memberships (REQ-064, slice 4c). The panel is the table, the drawer and the policy;
        // the `/public` half is the visitor's own signup, sign-in, profile, the two link clicks
        // and the gate probe a theme asks before it draws a page. The per-site policy hangs off
        // `/sites/{id}`, beside the other per-site surfaces.
        //
        // The literal segments are declared BEFORE `/members/{id}` so axum ranks
        // `/members/settings`-style paths ahead of it — a route registered after a
        // parameterised sibling is unreachable, and the visitor's sign-in is the one link a
        // member area cannot afford to lose.
        .route("/members", members_read)
        .route("/members", members_create)
        .route("/members/{id}", member_read.merge(member_write))
        .route("/members/{id}/block", member_block)
        .route("/members/{id}/verify", member_verify)
        .route("/members/{id}/send-verification", member_send_verification)
        .route("/members/{id}/send-reset", member_send_reset)
        .route("/members/{id}/sign-out-everywhere", member_signout_everywhere)
        .route("/sites/{site_id}/members/settings", member_settings_read)
        .route("/sites/{site_id}/members/settings", member_settings_write)
        .route("/public/members/signup", public_member_signup)
        .route("/public/members/signin", public_member_signin)
        .route("/public/members/signout", public_member_signout)
        .route("/public/members/me", public_member_me)
        .route("/public/members/verify", public_member_verify)
        .route("/public/members/password-reset", public_member_reset)
        .route("/public/members/gate", public_member_gate)
        // Comments (REQ-064, slice 4a). The inbox is `/comments`, the policy hangs off the site
        // it belongs to (`/sites/{id}/comment-settings`, beside the other per-site surfaces) and
        // the visitor's two routes live under `/public` where every unauthenticated surface on
        // this platform already lives.
        .route("/comments", comments_inbox)
        .route("/comments/bulk", comments_bulk)
        .route("/comments/{id}", comment_read.merge(comment_write))
        .route("/comments/{id}/reply", comment_reply)
        .route("/sites/{site_id}/comment-settings", comment_settings_read)
        .route("/sites/{site_id}/comment-settings", comment_settings_write)
        .route("/sites/{site_id}/comment-bans", comment_ban_create)
        .route("/sites/{site_id}/comment-bans/{id}", comment_ban_write)
        .route("/public/comments/{page}", public_comment_thread)
        .route("/public/comments/{page}", public_comment_submit)
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
        .route("/backups/{id}/verify", backups_verify)
        .route("/backup-schedules", backup_schedules_read)
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
        .route("/themes", themes_gallery)
        .route("/themes/{key}", theme_read)
        .route("/sites/{site_id}/theme", theme_activate)
        .route("/sites/{site_id}/theme/rollback", theme_rollback)
        .route("/sites/{site_id}/theme-settings", theme_settings_read)
        .route("/sites/{site_id}/theme-settings", theme_settings_save)
        .route(
            "/sites/{site_id}/theme-settings/publish",
            theme_settings_publish,
        )
        .route(
            "/sites/{site_id}/theme-settings/revisions",
            theme_settings_revisions,
        )
        .route(
            "/sites/{site_id}/theme-settings/revisions/{revision_no}",
            theme_settings_revision,
        )
        .route(
            "/sites/{site_id}/theme-settings/revisions/{revision_no}/restore",
            theme_settings_restore,
        )
        // Theme layouts and packages (REQ-062 slice 3). `/themes/{key}` above is a GET, and a
        // `DELETE` on the same path is a different method — axum merges those, so the two
        // coexist. `validate` and `install` are registered BEFORE `/themes/{key}` for the
        // reason `/backups/sweep` is: a `POST /themes/validate` would otherwise be a perfect
        // `key` and no conflict at all, so this is a readability choice rather than a
        // correctness one — but it keeps the static names next to each other.
        .route("/themes/validate", theme_package_validate)
        .route("/themes/install", theme_package_install)
        .route("/sites/{site_id}/theme-layouts", theme_layouts_read)
        .route(
            "/sites/{site_id}/theme-layouts/{slot}",
            theme_layout_slot_read,
        )
        .route(
            "/sites/{site_id}/theme-layouts/{slot}",
            theme_layout_slot_save,
        )
        .route(
            "/sites/{site_id}/theme-layouts/{slot}/reset",
            theme_layout_slot_reset,
        )
        .route(
            "/sites/{site_id}/theme-package/export",
            theme_package_export,
        )
        .route("/themes/{key}", theme_remove)
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
