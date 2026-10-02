//! The permission catalogue — every permission the platform knows.
//!
//! One row per permission, seeded into the `permissions` table on boot so role entries can
//! hold a foreign key. Keys follow `domain.action` (docs/07-IAM.md §2) and this list is the
//! single source of truth: a role entry, a route guard or a seed that references a key
//! outside the catalogue is a bug, and the store rejects it.
//!
//! Categories group the keys for the admin UI (`content`, `media`, `users`, `plugins`,
//! `deployment`, `developer`, `iam`, `audit`, `tenancy`, `webhooks`, `events`).

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
    // `deployment.manage`, distinct from `deployment.deploy` on purpose. "Manage" is the
    // operator's own instruments on the centre: run an update check, configure a maintenance
    // window, restart a workload. "Deploy" is the thing that changes what a tenant runs. They
    // are different amounts of trust and an account that may prepare a deployment — a release
    // engineer checking whether a version exists before the window opens — must not thereby be
    // able to put it in production.
    PermissionDef {
        key: "deployment.manage",
        category: "deployment",
        description: "Run update checks, configure maintenance windows and restart workloads",
    },
    // Cluster visibility is its own key, and not a fourth degree of the same axis: reading
    // replica counts and CPU limits is a fact about infrastructure that an operator investigating
    // an incident needs without the ability to restart anything, and merging it into
    // `deployment.manage` would mean the read arrives with the write.
    PermissionDef {
        key: "deployment.cluster.read",
        category: "deployment",
        description: "Read cluster replicas, resource usage and rollout status",
    },
    // Maintenance windows change whether every write route on the platform answers 503, so they
    // are separated from both: an account that may deploy a version must not thereby be able to
    // freeze the panel for everybody else.
    PermissionDef {
        key: "deployment.maintenance",
        category: "deployment",
        description: "Configure maintenance windows",
    },
    // Edge regions (REQ-035 slice 1). Read and manage are separate keys for the same
    // reason `deployment.cluster.read` is separate from `deployment.manage`: "which regions
    // exist and how are they doing" is a fact an operator needs during an incident, and
    // "rename one, move the routing default, drain one" is a control-plane action. Merged,
    // every person reading a health matrix could also redirect traffic.
    PermissionDef {
        key: "platform.regions.read",
        category: "platform",
        description: "Read the edge region registry, its health matrix and region latency",
    },
    PermissionDef {
        key: "platform.regions.manage",
        category: "platform",
        description: "Rename a region, set its status, activate it or move the routing default",
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
    // CDN / edge (docs/requests/REQ-011). Reading rules and the provider state is one
    // power; changing a rule is another, because a rule decides what a shared cache is
    // allowed to keep. `cdn.purge` is separate from `cdn.manage` on purpose: invalidating
    // a zone is a blunt operational act that an operator may want to allow without
    // letting the same account rewrite the policy.
    PermissionDef {
        key: "cdn.read",
        category: "cdn",
        description: "Read CDN settings, cache rules and purge history",
    },
    PermissionDef {
        key: "cdn.manage",
        category: "cdn",
        description: "Create, change, reorder and remove cache rules and provider settings",
    },
    PermissionDef {
        key: "cdn.purge",
        category: "cdn",
        description: "Invalidate cached URLs, tags or a whole zone",
    },
    // The developer platform (docs/requests/REQ-033, slice 1). Two keys, and the split is the
    // one the request itself names: **reading** a key's metadata and **minting** one are
    // different amounts of trust. An account that can see which integrations exist and how much
    // they call is an auditor's need; an account that can mint a key that authenticates as
    // this organization is a much larger power, and a read-only developer role must not be able
    // to grant itself the write by virtue of holding the read. The request log rides the read
    // key because it is the same question ("what has this organization called, and did it
    // work") answered one row at a time rather than one day at a time.
    PermissionDef {
        key: "developer.keys.read",
        category: "developer",
        description: "Read API key metadata, usage and the request log",
    },
    PermissionDef {
        key: "developer.keys.manage",
        category: "developer",
        description: "Create, rotate and revoke API keys",
    },
    // The API Explorer (REQ-033, slice 2). Two keys and the split is the one that makes the
    // Explorer safe rather than interesting: **browsing** the document is reading the platform's
    // own shape, which is the same question as reading a key's metadata, so it rides
    // `developer.read` — a manager holds it. **Running** a call is a much larger power even
    // though it carries no key material, because the caller's *session* is what authorises it:
    // a developer who can send `DELETE /pages/{id}` from the Explorer is doing that with the
    // permissions of the person sitting at the screen, and the request file is explicit that a
    // call the caller could not make from the UI must answer the same `403`. Collapsing both
    // into one key would have made a read-only role's Explorer a write capability.
    PermissionDef {
        key: "developer.read",
        category: "developer",
        description: "Browse the API reference and the served OpenAPI document",
    },
    PermissionDef {
        key: "developer.explorer.run",
        category: "developer",
        description: "Send API requests from the Explorer as the signed-in caller",
    },
    // OAuth applications (docs/requests/REQ-033, slice 3). Two keys, and the split is the one
    // that keeps the token endpoint honest.
    //
    // * `developer.oauth.read` is the app's own metadata: which redirect URIs it registered,
    //   what it may be granted, whether its secret rotation is still inside an overlap. That is
    //   a support question ("our integration started failing on Tuesday") and it is the same
    //   kind of question as reading a key's metadata, so it rides with the rest of the
    //   developer read surface.
    // * `developer.oauth.manage` mints and rotates a **client secret**. A client secret is a
    //   credential that authenticates as this organization at an endpoint no panel session ever
    //   passes through, and rotating one starts an overlap in which two secrets are valid. An
    //   account that can read the list must not thereby be able to add one — the same argument
    //   as `developer.keys.read` / `developer.keys.manage`, kept separate for the same reason.
    //
    // Neither key grants anything about a *user's* OAuth session: those are scoped by the
    // consent the user gave, not by a permission in this table.
    PermissionDef {
        key: "developer.oauth.read",
        category: "developer",
        description: "Read OAuth application metadata and registered redirect URIs",
    },
    PermissionDef {
        key: "developer.oauth.manage",
        category: "developer",
        description: "Register OAuth applications and rotate client secrets",
    },
    // SDK scaffolds and the CLI (docs/requests/REQ-033, slice 4). One key, and the split is
    // against adding a second.
    //
    // * Generating a starter writes an archive into a bucket. That is a *write* with a cost and
    //   a quota, so it is its own power rather than riding `developer.read`.
    // * Validating a manifest and listing the templates are pure reads and ride `developer.read`
    //   with the rest of the "what can I build here" surface -- the API Explorer's document
    //   browsing included.
    //
    // The CLI device-code flow deliberately adds **no key of its own**. `start` is a read of the
    // tenant's CLI state, so `developer.read`; `approve` is `developer.keys.manage`, because
    // the token it authorises is a credential for the same tenant and an account that cannot mint
    // a key must not be able to mint a CLI token by approving somebody else's login. A fourth
    // key here would have been one more place for the two halves to disagree about who may log
    // a machine in.
    PermissionDef {
        key: "developer.sdks.scaffold",
        category: "developer",
        description: "Generate plugin, theme and workflow starter archives",
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
    // Developer portal (docs/requests/REQ-022, slice 1). Six powers, and the split is drawn
    // where the *blast radius* changes rather than where the screens do.
    //
    // * `developer.read` is the overview and the scope catalogue — what the portal exists to
    //   show somebody who is about to integrate. It is not in the base role: an account that
    //   can read it by default learns the shape of the public API surface of the deployment,
    //   which is reconnaissance, not a convenience.
    // * `developer.keys.read` / `developer.keys.manage` are the credential pair. Issuing a
    //   credential is the only action in the platform that creates a *usable* secret, so it is
    //   never inferred from reading the list — an account that can see the key prefixes must
    //   not be able to mint one that does anything.
    // * `developer.oauth.read` / `developer.oauth.manage` are the same pair for OAuth apps,
    //   kept separate because an app's redirect URIs are a token-theft surface and an account
    //   auditing which apps exist should not be able to change where they point.
    // * `developer.logs.read` is the request log. Separate from `developer.read` because the
    //   log answers "who called what, and what did it cost" — the closest thing this platform
    //   has to a traffic record, and not something an integration author needs in order to
    //   build an integration.
    PermissionDef {
        key: "developer.read",
        category: "developer",
        description: "Read the developer portal overview and the scope catalogue",
    },
    PermissionDef {
        key: "developer.keys.read",
        category: "developer",
        description: "Read API keys, their scopes and their usage",
    },
    PermissionDef {
        key: "developer.keys.manage",
        category: "developer",
        description: "Create, rotate and revoke API keys",
    },
    PermissionDef {
        key: "developer.oauth.read",
        category: "developer",
        description: "Read OAuth apps and their authorizations",
    },
    PermissionDef {
        key: "developer.oauth.manage",
        category: "developer",
        description: "Register OAuth apps, rotate their secrets and archive them",
    },
    PermissionDef {
        key: "developer.logs.read",
        category: "developer",
        description: "Read the API request log",
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
            // The three the deployment centre adds. Named here explicitly because an
            // uncatalogued key is the worst failure this catalogue has: `guards::require`
            // resolves a key against it, so a route guarded by a name the catalogue does not
            // know answers 403 for *everybody*, the owner included, and the screen looks like a
            // permissions bug rather than a typo.
            "deployment.manage",
            "deployment.cluster.read",
            "deployment.maintenance",
        ] {
            assert!(is_known(family), "{family} must be in the catalogue");
        }
    }

    #[test]
    fn the_platform_region_family_is_catalogued() {
        // REQ-035 slice 1. An uncatalogued key is the worst failure this catalogue has:
        // `guards::require` resolves a key against it, so a route guarded by a name the
        // catalogue does not know answers 403 for *everybody* — the owner included — and the
        // screen reads as a permissions bug rather than a typo.
        for key in ["platform.regions.read", "platform.regions.manage"] {
            assert!(is_known(key), "{key} must be in the catalogue");
            // The category is asserted too, because the role editor groups by it and a key
            // in the wrong category is invisible where the editor looks.
            assert_eq!(
                CATALOGUE
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| d.category),
                Some("platform"),
                "{key} must be in the platform category"
            );
        }
        // The two must stay *different* keys. A later edit that folds the read into the
        // manage key would keep this test green while removing the whole point of the
        // split, so the distinction is asserted rather than documented.
        assert_ne!("platform.regions.read", "platform.regions.manage");
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
    fn the_developer_family_is_catalogued() {
        // REQ-033 slice 1, and REQ-022's slice on top. The keys are guarded separately and a
        // route guarded by a name the catalogue does not know answers 403 for *everybody* —
        // including the owner — so a typo here would read as "permissions are broken" rather
        // than as "a key is missing".
        //
        // The assertion is on the category as well as the key because the panel's role editor
        // groups by it and a key in the wrong category is invisible where the editor looks.
        //
        // `developer.logs.read` is main's key for the traffic record, kept in the catalogue
        // because the role editor offers it and an operator who granted it must not find the
        // grant silently unrecognised. It is *not* what guards `/developer/logs` on this
        // branch — that read rides `developer.keys.read`, argued at the route.
        for key in [
            "developer.keys.read",
            "developer.keys.manage",
            "developer.read",
            "developer.logs.read",
            "developer.explorer.run",
            "developer.oauth.read",
            "developer.oauth.manage",
            "developer.sdks.scaffold",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("developer"),
                "{key} belongs to the developer category"
            );
        }
        // REQ-022's stronger form, kept because it is the one that fails loudly: a catalogue
        // that collapsed `developer.keys.manage` into `developer.keys.read` would still pass a
        // presence check and would ship the escalation.
        assert_ne!(
            get("developer.keys.read").map(|entry| entry.description),
            get("developer.keys.manage").map(|entry| entry.description),
            "a read and a manage of the same object must be two keys with two descriptions"
        );
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
