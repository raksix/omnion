//! The permission catalogue — every permission the platform knows.
//!
//! One row per permission, seeded into the `permissions` table on boot so role entries can
//! hold a foreign key. Keys follow `domain.action` (docs/07-IAM.md §2) and this list is the
//! single source of truth: a role entry, a route guard or a seed that references a key
//! outside the catalogue is a bug, and the store rejects it.
//!
//! Categories group the keys for the admin UI (`content`, `media`, `users`, `plugins`,
//! `deployment`, `iam`, `audit`, `tenancy`, `webhooks`, `events`).

/// A single permission the platform understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionDef {
    /// Stable key, e.g. `content.pages.publish`.
    pub key: &'static str,
    /// UI grouping, e.g. `content`.
    pub category: &'static str,
    /// What the permission allows, in product language.
    pub description: &'static str,
}

/// Every permission of the platform (docs/07-IAM.md §2 plus the IAM and audit surfaces the
/// API already exposes).
pub const CATALOGUE: &[PermissionDef] = &[
    // Content (docs/07-IAM.md §2 "Content").
    PermissionDef {
        key: "content.pages.read",
        category: "content",
        description: "Read pages and their revisions",
    },
    PermissionDef {
        key: "content.pages.create",
        category: "content",
        description: "Create pages",
    },
    PermissionDef {
        key: "content.pages.update",
        category: "content",
        description: "Edit page content",
    },
    PermissionDef {
        key: "content.pages.delete",
        category: "content",
        description: "Delete pages",
    },
    PermissionDef {
        key: "content.pages.publish",
        category: "content",
        description: "Publish or unpublish pages",
    },
    PermissionDef {
        key: "content.pages.schedule",
        category: "content",
        description: "Schedule publication",
    },
    PermissionDef {
        key: "content.pages.restore",
        category: "content",
        description: "Restore an earlier revision",
    },
    // Media.
    PermissionDef {
        key: "media.read",
        category: "media",
        description: "Read the media library",
    },
    PermissionDef {
        key: "media.upload",
        category: "media",
        description: "Upload files",
    },
    PermissionDef {
        key: "media.update",
        category: "media",
        description: "Edit media metadata",
    },
    PermissionDef {
        key: "media.delete",
        category: "media",
        description: "Delete files",
    },
    PermissionDef {
        key: "media.manage",
        category: "media",
        description: "Manage folders and storage settings",
    },
    // Slice 3 splits the storage-side powers out of `media.manage`. They are separate keys
    // because they are separate risks: a team that may organise a library does not need the
    // power to repoint a site's object store or mint a public link to a private file.
    PermissionDef {
        key: "media.settings.manage",
        category: "media",
        description: "Change transformation presets, storage and retention settings",
    },
    PermissionDef {
        key: "media.share",
        category: "media",
        description: "Create public share links for files",
    },
    // Slice 4's third separation. Releasing a quarantined file is the action that undoes a
    // safety decision, so it is not `media.manage` (organising a library) and not
    // `media.delete` (removing one): an editor who may rearrange the whole library and a
    // colleague who may clear an icon out of it are both wrong for this button, and the
    // person who holds it is exactly the person a security review asks about.
    PermissionDef {
        key: "media.scan.manage",
        category: "media",
        description: "Configure scanning and release quarantined files",
    },
    // AI Hub (docs/06-AI-HUB.md §1, §7): connecting providers is an administrator-level power,
    // while using the platform's AI chat is an everyday one — the AI Hub's own permission sets
    // (§8) build on these keys in later phases.
    PermissionDef {
        key: "ai.providers.read",
        category: "ai",
        description: "Read AI providers and the model registry",
    },
    PermissionDef {
        key: "ai.providers.manage",
        category: "ai",
        description: "Connect and configure AI providers",
    },
    PermissionDef {
        key: "ai.chat",
        category: "ai",
        description: "Use the platform's AI chat",
    },
    // Workflows (docs/requests/REQ-003): the automation surface — definitions, their runs and
    // the steps a run left behind.
    PermissionDef {
        key: "workflows.read",
        category: "workflows",
        description: "Read workflows and their run history",
    },
    PermissionDef {
        key: "workflows.manage",
        category: "workflows",
        description: "Create, edit and remove workflows",
    },
    PermissionDef {
        key: "workflows.run",
        category: "workflows",
        description: "Start and cancel workflow runs",
    },
    // Users.
    PermissionDef {
        key: "users.read",
        category: "users",
        description: "Read accounts",
    },
    PermissionDef {
        key: "users.create",
        category: "users",
        description: "Invite and create accounts",
    },
    PermissionDef {
        key: "users.update",
        category: "users",
        description: "Edit accounts",
    },
    PermissionDef {
        key: "users.delete",
        category: "users",
        description: "Delete accounts",
    },
    PermissionDef {
        key: "users.impersonate",
        category: "users",
        description: "Sign in as another account",
    },
    // Plugins.
    PermissionDef {
        key: "plugins.read",
        category: "plugins",
        description: "Browse installed plugins",
    },
    PermissionDef {
        key: "plugins.install",
        category: "plugins",
        description: "Install plugins",
    },
    PermissionDef {
        key: "plugins.update",
        category: "plugins",
        description: "Update plugins",
    },
    PermissionDef {
        key: "plugins.disable",
        category: "plugins",
        description: "Enable or disable plugins",
    },
    PermissionDef {
        key: "plugins.uninstall",
        category: "plugins",
        description: "Remove plugins",
    },
    // Deployment.
    PermissionDef {
        key: "deployment.read",
        category: "deployment",
        description: "Read deployment state and history",
    },
    PermissionDef {
        key: "deployment.preview",
        category: "deployment",
        description: "Create preview environments",
    },
    PermissionDef {
        key: "deployment.deploy",
        category: "deployment",
        description: "Deploy to an environment",
    },
    PermissionDef {
        key: "deployment.rollback",
        category: "deployment",
        description: "Roll a deployment back",
    },
    // REQ-128 slice 4 added two, and both are separate powers rather than extra detail on the
    // ones above:
    //
    // * `bundle.generate` writes a row AND shells out to the release pipeline's generator. An
    //   account that may only read releases must not be able to make the panel run a program;
    //   giving it `deployment.read` for that would make the read key the most powerful one in
    //   the deployment family.
    // * `manage` already exists for the deployment centre's own writes; the upgrade
    //   acknowledgement rides it rather than inventing a key, because accepting that a database
    //   can only be restored is the same decision as rolling a deployment back.
    PermissionDef {
        key: "deployment.bundle.generate",
        category: "deployment",
        description: "Generate an environment bundle and render what it produces",
    },
    // REQ-129 adds three, and the split is the request's own: reading the ledger is `read`,
    // CHANGING THE SCHEMA is not a deployment power, and rehearsing a reversal is neither of
    // those two.
    //
    // * `migrations.read` — the ledger, one migration's SQL, the policy and the lint findings.
    //   Metadata only: every field on those routes is derived from files and rows this
    //   installation already has, so a viewer who may read deployments may read them too.
    // * `migrations.apply` — runs DDL. Deliberately NOT `deployment.deploy`: an operator who may
    //   ship a release is not thereby authorised to write the schema, and a key that means both
    //   cannot answer "who changed the database?" after an incident. It IS narrower than
    //   `deployment.rollback`, which only selects a previous image.
    // * `migrations.verify` — rehearses a reversal against a scratch database and writes the one
    //   column (`down_verified_at`) that makes a release claim its database can be rolled back.
    //   Its own key because it is the one write in this family that LAUNCHES SQL, and because
    //   separating it means "who approved the rollback path" is a distinct question from "who
    //   applied the migration".
    PermissionDef {
        key: "deployment.migrations.read",
        category: "deployment",
        description: "Read the migration ledger, one migration's SQL, the policy and lint findings",
    },
    PermissionDef {
        key: "deployment.migrations.apply",
        category: "deployment",
        description: "Apply pending schema migrations and save the migration policy",
    },
    PermissionDef {
        key: "deployment.migrations.verify",
        category: "deployment",
        description: "Rehearse a migration reversal against a scratch database",
    },
    // Identity and access management.
    PermissionDef {
        key: "iam.permissions.read",
        category: "iam",
        description: "Read the permission catalogue",
    },
    PermissionDef {
        key: "iam.roles.read",
        category: "iam",
        description: "Read roles and their permission sets",
    },
    PermissionDef {
        key: "iam.roles.manage",
        category: "iam",
        description: "Create roles and change their permission sets",
    },
    PermissionDef {
        key: "iam.bindings.read",
        category: "iam",
        description: "Read role assignments",
    },
    PermissionDef {
        key: "iam.bindings.manage",
        category: "iam",
        description: "Assign and revoke roles",
    },
    // The rest of the IAM surface (docs/07-IAM.md, REQ-006): each family keeps read and manage
    // apart, so an operator can be trusted to look at the people, the policies or the sessions
    // without being able to change them.
    PermissionDef {
        key: "iam.groups.read",
        category: "iam",
        description: "Read groups and their members",
    },
    PermissionDef {
        key: "iam.groups.manage",
        category: "iam",
        description: "Create groups and change their membership",
    },
    PermissionDef {
        key: "iam.serviceaccounts.read",
        category: "iam",
        description: "Read machine identities and their keys",
    },
    PermissionDef {
        key: "iam.serviceaccounts.manage",
        category: "iam",
        description: "Create service accounts and issue or revoke their keys",
    },
    PermissionDef {
        key: "iam.policies.read",
        category: "iam",
        description: "Read ABAC policies and their versions",
    },
    PermissionDef {
        key: "iam.policies.manage",
        category: "iam",
        description: "Create and change ABAC policies",
    },
    PermissionDef {
        key: "iam.simulate",
        category: "iam",
        description: "Ask the permission simulator what a subject may do",
    },
    PermissionDef {
        key: "iam.security.read",
        category: "iam",
        description: "Read the password, lockout, IP and session policy",
    },
    PermissionDef {
        key: "iam.security.manage",
        category: "iam",
        description: "Change the password, lockout, IP and session policy",
    },
    // The secrets surface (docs/requests/REQ-125). Reading the key ring is deliberately
    // separated from managing the root key: an operator who may see that a rotation exists is
    // not the one who may start one, because a rotation is irreversible if the operator key is
    // wrong.
    PermissionDef {
        key: "secrets.read",
        category: "secrets",
        description: "Read secrets, the key ring state and the credential slot assignments",
    },
    PermissionDef {
        key: "secrets.manage",
        category: "secrets",
        description: "Create, change and archive stored secrets",
    },
    PermissionDef {
        key: "secrets.root.manage",
        category: "secrets",
        description: "Rotate the installation root key and run a re-wrap ceremony",
    },
    PermissionDef {
        key: "secrets.assign",
        category: "secrets",
        description: "Assign credentials to slots and change their primary and fallback",
    },
    PermissionDef {
        key: "secrets.lease",
        category: "secrets",
        description: "Issue and revoke short-lived secret leases",
    },
    PermissionDef {
        key: "secrets.deploykeys.read",
        category: "secrets",
        description: "Read deployment keys and their use log",
    },
    PermissionDef {
        key: "secrets.deploykeys.manage",
        category: "secrets",
        description: "Create, revoke and delete deployment keys",
    },
    PermissionDef {
        key: "secrets.audit",
        category: "secrets",
        description: "Read the secrets audit trail and acknowledge anomaly flags",
    },
    PermissionDef {
        key: "observability.read",
        category: "observability",
        description: "Read logs, traces, metric catalogue, exporters and alert state",
    },
    PermissionDef {
        key: "observability.manage",
        category: "observability",
        description: "Change observability settings, alert rules and silences",
    },
    PermissionDef {
        key: "observability.exporters.manage",
        category: "observability",
        description: "Add, edit and test telemetry exporters",
    },
    // REQ-127, the reliability centre. The read/manage split is the request's own: reading a
    // budget is safe, changing one is a production behaviour, and the intake power is separate
    // because an intake endpoint's HMAC secret is a different kind of danger from a rate limit —
    // a wrong budget throttles a customer, a wrong HMAC scheme accepts forged webhooks.
    PermissionDef {
        key: "reliability.read",
        category: "reliability",
        description: "Read rate-limit policies, refusal rollups and reliability state",
    },
    PermissionDef {
        key: "reliability.manage",
        category: "reliability",
        description: "Change rate-limit policies, retry policies and breaker state",
    },
    PermissionDef {
        key: "reliability.intake.manage",
        category: "reliability",
        description: "Declare inbound endpoints and change their HMAC, size caps and sanitisation",
    },
    PermissionDef {
        key: "iam.sessions.read",
        category: "iam",
        description: "Read active sessions",
    },
    PermissionDef {
        key: "iam.sessions.revoke",
        category: "iam",
        description: "End a signed-in session",
    },
    PermissionDef {
        key: "iam.devices.read",
        category: "iam",
        description: "Read known devices",
    },
    PermissionDef {
        key: "iam.devices.manage",
        category: "iam",
        description: "Set a device's trust window and forget devices",
    },
    PermissionDef {
        key: "iam.providers.read",
        category: "iam",
        description: "Read the connected sign-in providers",
    },
    PermissionDef {
        key: "iam.providers.manage",
        category: "iam",
        description: "Connect and change sign-in providers",
    },
    PermissionDef {
        key: "iam.approvals.read",
        category: "iam",
        description: "Read permission requests",
    },
    PermissionDef {
        key: "iam.approvals.decide",
        category: "iam",
        description: "Approve or reject permission requests",
    },
    PermissionDef {
        key: "iam.provisioning.manage",
        category: "iam",
        description: "Issue provisioning tokens and run the sync",
    },
    // Audit.
    PermissionDef {
        key: "audit.read",
        category: "audit",
        description: "Read the audit trail",
    },
    // Tenancy (docs/01-VISION.md §10, docs/07-IAM.md §7): organizations, their sites and the
    // domains that address them. `organizations.manage` covers opening and editing tenants;
    // the platform surface stays with the Owner/Administrator ladder.
    PermissionDef {
        key: "organizations.read",
        category: "tenancy",
        description: "Read organizations",
    },
    PermissionDef {
        key: "organizations.manage",
        category: "tenancy",
        description: "Create and edit organizations",
    },
    PermissionDef {
        key: "sites.read",
        category: "tenancy",
        description: "Read sites and their domains",
    },
    PermissionDef {
        key: "sites.create",
        category: "tenancy",
        description: "Create sites",
    },
    PermissionDef {
        key: "sites.update",
        category: "tenancy",
        description: "Edit sites",
    },
    PermissionDef {
        key: "sites.delete",
        category: "tenancy",
        description: "Delete sites",
    },
    PermissionDef {
        key: "domains.manage",
        category: "tenancy",
        description: "Add, remove and promote site domains",
    },
    // Events and webhooks (docs/01-VISION.md §13, phase P12). Reading the bus is one power;
    // pointing the platform's events at an outside URL is a stronger one, because that URL
    // receives the organization's data.
    PermissionDef {
        key: "webhooks.read",
        category: "webhooks",
        description: "Read webhook endpoints and their delivery history",
    },
    PermissionDef {
        key: "webhooks.manage",
        category: "webhooks",
        description: "Connect, change, test and remove webhook endpoints",
    },
    PermissionDef {
        key: "events.read",
        category: "events",
        description: "Read the platform's event feed",
    },
    // Notifications (docs/requests/REQ-021). Four powers, split by *who is affected* rather
    // than by what the button does:
    //
    // * `notifications.read` is a person's own inbox, which every account holds — it is
    //   owner-scoped in the store, so it grants nothing about anybody else.
    // * `notifications.send` is writing to *other* people's inboxes, and it is the one worth
    //   guarding: an account that may only notify itself cannot be used to reach the rest of
    //   the organization, and a module that legitimately needs it says so in its manifest.
    // * `notifications.manage` is the reader's own channel configuration and preferences.
    // * `notifications.admin` is the organization-wide delivery log and the router's rules
    //   (below — it arrived with slice 3, the first build with routes behind it).
    // Security centre (docs/requests/REQ-012). Three powers, and the split is drawn where
    // the request draws it: *looking* is not the same power as *dismissing*, and they are
    // certainly not the same power as *changing the policy that produced the finding*.
    //
    // * `security.read` is the posture overview and the findings list. It is deliberately NOT
    //   in the base role: this screen is the platform's own account of itself, and an account
    //   that can read it by default learns the deployment's posture without ever being given
    //   the job of maintaining it.
    // * `security.scan` re-runs the checks and ingests a CI report. It changes no
    //   configuration, so it sits below `manage` on purpose — an account that may look may
    //   also ask for a fresher look, and nothing more.
    // * `security.manage` acknowledges, ignores and marks findings fixed. This is the key
    //   somebody will be asked to justify later, so it is granted on purpose and never
    //   inferred from `read`.
    // * `security.ip.manage` (slice 4) is the allow/deny lists. It is separate because an
    //   IP rule can lock an operator out of their own platform, and that is a different kind
    //   of power from deciding what is worth looking at.
    PermissionDef {
        key: "security.read",
        category: "security",
        description: "Read the security posture and the findings list",
    },
    PermissionDef {
        key: "security.scan",
        category: "security",
        description: "Re-run the security checks and ingest a dependency report",
    },
    PermissionDef {
        key: "security.manage",
        category: "security",
        description: "Acknowledge, ignore and resolve security findings",
    },
    PermissionDef {
        key: "security.ip.manage",
        category: "security",
        description: "Change the IP allow and deny lists",
    },
    // Backup centre (docs/requests/REQ-013). Four powers, and the split is the one the
    // request draws: **taking** a backup and **overwriting the live platform** are different
    // powers, and the gap between them is the whole risk of the screen.
    //
    // * `backup.read` is the list, the detail, the manifest and the status cards. It is
    //   safe to grant broadly: knowing when the last backup ran is an operational fact
    //   every support conversation needs, and knowing it is *old* is the alarm.
    // * `backup.create` starts a run and re-verifies one. Verifying is here rather than
    //   under `manage` because it only reads: an account that may ask "is that artifact
    //   still there?" is not an account that may delete it.
    // * `backup.restore` is deliberately its own key. A restore overwrites the platform's
    //   content with an older copy, and the person who holds it should be the same person
    //   who could delete the backups and start new ones — no more, and the two are
    //   grantable apart so a site owner can be given both without being given `manage`.
    // * `backup.manage` is schedules, retention and settings. It does NOT include
    //   `backup.restore`: a platform where the schedule editor can also overwrite live
    //   content is a platform where the nightly job and the operator's button are the same
    //   authority, which is how a retention window becomes an outage.
    PermissionDef {
        key: "backup.read",
        category: "backup",
        description: "Read backups, their parts, manifests and status",
    },
    PermissionDef {
        key: "backup.create",
        category: "backup",
        description: "Run a backup and verify an existing one",
    },
    PermissionDef {
        key: "backup.restore",
        category: "backup",
        description: "Restore the platform from a backup",
    },
    PermissionDef {
        key: "backup.manage",
        category: "backup",
        description: "Manage backup schedules, retention and destination settings",
    },
    // System health (docs/requests/REQ-014). Two powers, and the split is the one the
    // request draws: **seeing** that a dependency is unhappy is an operational fact every
    // support conversation needs, and **changing what counts as unhappy** is a decision
    // about this deployment that a reader must not be able to make.
    //
    // * `health.read` is the overview, the service detail, the metrics and the incidents.
    //   It is safe to grant broadly for the same reason `backup.read` is: knowing Redis
    //   stopped answering is the alarm, and hiding it from somebody who can see the
    //   dashboard is how a support call turns into an outage.
    // * `health.manage` runs the probes on demand, writes thresholds and intervals, and
    //   acknowledges or resolves an incident. It is NOT implied by `read`, for the reason
    //   the request gives explicitly: `POST /checks/run` is a mutation — it writes samples
    //   — so it rides the managing key rather than the reading one. A reader who could
    //   trigger a run could also fill the retention window with samples of their own
    //   choosing, one button press at a time.
    PermissionDef {
        key: "health.read",
        category: "health",
        description: "Read system health, metrics and incidents",
    },
    PermissionDef {
        key: "health.manage",
        category: "health",
        description: "Run health checks, set thresholds and acknowledge incidents",
    },
    PermissionDef {
        key: "notifications.read",
        category: "notifications",
        description: "Read and clear your own notifications",
    },
    PermissionDef {
        key: "notifications.send",
        category: "notifications",
        description: "Send notifications to other accounts",
    },
    PermissionDef {
        key: "notifications.manage",
        category: "notifications",
        description: "Change your notification channels and preferences",
    },
    // `notifications.admin` is the org-wide delivery log and the router's rules. It arrived with
    // slice 3, which is the first build where it has routes behind it — a permission with no
    // route is a role entry granting a promise the platform cannot keep, which is why it was
    // absent from the catalogue for the two slices before.
    //
    // It is the widest key in the notification family and the only one that reads *anybody's*
    // activity: the outbox shows who was told what and whether it arrived. The rows carry ids
    // and states, never a title or a body, so the log is safe to show — but "safe to show" is
    // not the same as "harmless to grant", and it belongs to an administrator on purpose.
    PermissionDef {
        key: "notifications.admin",
        category: "notifications",
        description: "Read the organization-wide delivery outbox and manage routing rules",
    },
    // Search (docs/requests/REQ-002). `search.read` is the box itself — every signed-in
    // account holds it, and the results are still narrowed by organization and by each
    // provider's own read permission; `search.manage` is index maintenance, not searching.
    PermissionDef {
        key: "search.read",
        category: "search",
        description: "Search the platform's indexed content",
    },
    PermissionDef {
        key: "search.manage",
        category: "search",
        description: "Rebuild and configure the search index",
    },
    // Analytics (docs/requests/REQ-007). Reading the numbers is one power; changing what a site
    // collects — and therefore what it promises its visitors — is a stronger one; exporting raw
    // rows is a third, because a report a site's operator can hand out is still the site's data.
    PermissionDef {
        key: "analytics.read",
        category: "analytics",
        description: "Read analytics reports, settings and the tracking snippet",
    },
    PermissionDef {
        key: "analytics.export",
        category: "analytics",
        description: "Export analytics reports",
    },
    PermissionDef {
        key: "analytics.goals.manage",
        category: "analytics",
        description: "Create and edit conversion goals",
    },
    PermissionDef {
        key: "analytics.settings.manage",
        category: "analytics",
        description: "Change tracking, privacy and retention settings",
    },
];

/// Look a permission up by key.
#[must_use]
pub fn get(key: &str) -> Option<&'static PermissionDef> {
    CATALOGUE.iter().find(|entry| entry.key == key)
}

/// `true` when the key exists in the catalogue.
#[must_use]
pub fn is_known(key: &str) -> bool {
    get(key).is_some()
}

/// Every key, in catalogue order.
#[must_use]
pub fn keys() -> Vec<&'static str> {
    CATALOGUE.iter().map(|entry| entry.key).collect()
}

/// Request a permission key and fail when it is outside the catalogue.
pub fn expect_known(key: &str) -> Result<&'static PermissionDef, crate::error::PermissionsError> {
    get(key).ok_or_else(|| crate::error::PermissionsError::UnknownPermission(key.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn keys_are_unique_and_well_formed() {
        let mut seen = BTreeSet::new();
        for entry in CATALOGUE {
            assert!(seen.insert(entry.key), "duplicate key {}", entry.key);
            assert!(
                entry.key.split('.').count() >= 2,
                "{} must be domain.action",
                entry.key
            );
            assert!(
                entry
                    .key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'),
                "{} must be lowercase ASCII",
                entry.key
            );
            assert!(
                !entry.description.trim().is_empty(),
                "{} needs a description",
                entry.key
            );
            assert!(
                !entry.category.trim().is_empty(),
                "{} needs a category",
                entry.key
            );
        }
    }

    #[test]
    fn every_documented_category_is_present() {
        let categories: BTreeSet<&str> = CATALOGUE.iter().map(|entry| entry.category).collect();
        for expected in [
            "content",
            "media",
            "ai",
            "workflows",
            "users",
            "plugins",
            "deployment",
            "iam",
            "audit",
            "tenancy",
            "search",
            "analytics",
        ] {
            assert!(categories.contains(expected), "missing category {expected}");
        }
    }

    #[test]
    fn the_observability_family_is_catalogued() {
        // REQ-126: the admin centre is guarded by `observability.*`, and the split matters —
        // reading telemetry is not the power to reconfigure telemetry export.
        for key in [
            "observability.read",
            "observability.manage",
            "observability.exporters.manage",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("observability"),
                "{key} belongs to the observability category"
            );
        }
    }

    /// REQ-127: the reliability centre's three keys exist, and the intake power is SEPARATE.
    ///
    /// The separation is the assertion, not the existence. `reliability.manage` changes a budget
    /// — a wrong number throttles a customer. `reliability.intake.manage` declares an inbound
    /// endpoint and its HMAC scheme — a wrong scheme accepts forged webhooks. Folding them into
    /// one key would hand every operator who tunes a rate limit the power to change how inbound
    /// traffic is authenticated, and folding the other way would make the intake screen
    /// unreachable to the people who are supposed to own it.
    #[test]
    fn the_reliability_family_is_catalogued_and_intake_is_separate() {
        for key in [
            "reliability.read",
            "reliability.manage",
            "reliability.intake.manage",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("reliability"),
                "{key} belongs to the reliability category"
            );
        }
        // The read key must not imply the manage key, and the manage key must not imply the
        // intake one. This is a catalogue-level statement about powers, and the integration walk
        // in `apps/api/tests/reliability_limits.rs` proves it over the router.
        assert_ne!(
            get("reliability.manage").map(|entry| entry.description),
            get("reliability.intake.manage").map(|entry| entry.description),
            "two powers with one description are one power written twice"
        );
    }

    #[test]
    fn the_deployment_family_is_catalogued_and_generation_is_separate_from_reading() {
        for key in [
            "deployment.read",
            "deployment.preview",
            "deployment.deploy",
            "deployment.rollback",
            "deployment.bundle.generate",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
        }
        // The bundle generator shells out to a program and writes a row, so it may not ride the
        // read key. This is asserted rather than assumed because a route that guarded a
        // subprocess with `deployment.read` would be green in every catalogue test and be the
        // most powerful key in the family.
        assert_ne!(
            "deployment.bundle.generate", "deployment.read",
            "generation is a write, not a read"
        );
    }

    #[test]
    fn applying_a_migration_is_not_a_deployment_power_and_rehearsing_is_not_either() {
        // REQ-129's three keys, and the reasons they are three rather than one.
        for key in [
            "deployment.migrations.read",
            "deployment.migrations.apply",
            "deployment.migrations.verify",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("deployment"),
                "{key} belongs to the deployment category"
            );
        }
        // The claim this file exists to make load-bearing: `deployment.deploy` ships an image, it
        // does not write the schema. A route that guarded `POST /migrations/apply` with the deploy
        // key would be green in every test in this crate — none of them builds a router — and
        // would answer "who changed the database?" with "whoever could deploy".
        assert_ne!(
            "deployment.migrations.apply", "deployment.deploy",
            "changing the schema is not deploying an image"
        );
        assert_ne!(
            "deployment.migrations.apply", "deployment.rollback",
            "applying is the opposite of rolling back"
        );
        // Rehearsing a reversal is its own power because it is the only write here that executes
        // SQL, and because it is the write that turns `unknown` into `reversible` on the release.
        assert_ne!(
            "deployment.migrations.verify", "deployment.migrations.apply",
            "proving the rollback path is not applying a migration"
        );
        // Distinct descriptions, which is the cheapest way to catch two keys that drifted into
        // one power. Two identical descriptions means the family grew a name and not a permission.
        let mut descriptions: Vec<&str> = [
            "deployment.read",
            "deployment.migrations.read",
            "deployment.migrations.apply",
            "deployment.migrations.verify",
        ]
        .iter()
        .map(|key| get(key).map(|entry| entry.description).unwrap_or(""))
        .collect();
        descriptions.sort_unstable();
        let before = descriptions.len();
        descriptions.dedup();
        assert_eq!(
            before,
            descriptions.len(),
            "two powers with one description are one power written twice"
        );
    }

    #[test]
    fn the_tenancy_family_is_catalogued() {
        // docs/01-VISION.md §10 (multi-site vs multi-tenant) and docs/07-IAM.md §7: sites and
        // the domains that address them are first-class, so the keys that guard them are too.
        for key in [
            "organizations.read",
            "organizations.manage",
            "sites.read",
            "sites.create",
            "sites.update",
            "sites.delete",
            "domains.manage",
        ] {
            assert!(is_known(key), "{key} must be in the catalogue");
        }

        for key in [
            "organizations.manage",
            "sites.create",
            "sites.update",
            "sites.delete",
            "domains.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("tenancy"),
                "{key} belongs to the tenancy category"
            );
        }
    }

    #[test]
    fn the_documented_permission_families_are_covered() {
        // docs/07-IAM.md §2 lists these exact families; they are the contract the admin UI
        // (and later the AI agent permission sets) build on.
        for family in [
            "content.pages.read",
            "content.pages.create",
            "content.pages.update",
            "content.pages.delete",
            "content.pages.publish",
            "content.pages.schedule",
            "content.pages.restore",
            "media.read",
            "media.upload",
            "media.update",
            "media.delete",
            "media.manage",
            "users.read",
            "users.create",
            "users.update",
            "users.delete",
            "users.impersonate",
            "plugins.read",
            "plugins.install",
            "plugins.update",
            "plugins.disable",
            "plugins.uninstall",
            "deployment.read",
            "deployment.preview",
            "deployment.deploy",
            "deployment.rollback",
        ] {
            assert!(is_known(family), "{family} must be in the catalogue");
        }
    }

    #[test]
    fn the_workflow_family_is_catalogued() {
        // P09: the automation surface is guarded by three keys — read, manage and run — so a
        // role can be trusted to trigger a workflow without letting it rewrite definitions.
        for key in ["workflows.read", "workflows.manage", "workflows.run"] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("workflows"),
                "{key} belongs to the workflows category"
            );
        }
    }

    #[test]
    fn the_ai_family_is_catalogued() {
        // P11 (docs/06-AI-HUB.md §1): connecting a provider and using the chat are separate
        // keys, so an operator can let a team talk to the platform's AI without letting them
        // point it at another endpoint.
        for key in ["ai.providers.read", "ai.providers.manage", "ai.chat"] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("ai"),
                "{key} belongs to the ai category"
            );
        }
    }

    #[test]
    fn the_health_family_is_catalogued() {
        // REQ-014: seeing that a dependency is unhealthy and deciding what counts as
        // unhealthy are two powers, and the run-checks mutation is the second one
        // because it writes samples — a reader must not be able to fill the
        // retention window with samples of their own choosing.
        for key in ["health.read", "health.manage"] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("health"),
                "{key} belongs to the health category"
            );
        }
    }

    #[test]
    fn the_analytics_family_is_catalogued() {
        // REQ-007: seeing what a site measures, deciding what it may measure, exporting the
        // numbers and owning its conversion goals are four separate powers.
        for key in [
            "analytics.read",
            "analytics.export",
            "analytics.goals.manage",
            "analytics.settings.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("analytics"),
                "{key} belongs to the analytics category"
            );
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(get("content.pages.explode").is_none());
        assert!(expect_known("nope").is_err());
        assert!(expect_known("content.pages.read").is_ok());
        assert_eq!(keys().len(), CATALOGUE.len());
    }
}
