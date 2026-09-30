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
    // CRM (docs/requests/REQ-051). A business separates the four powers the relationship layer
    // actually has: seeing the people and the companies, changing them, removing them for good
    // (archiving is a different act from editing), and the two narrower ones — merging two
    // records is irreversible from the user's side, and the flagged fields (a contract note, a
    // margin) are a role's decision rather than the record's.
    PermissionDef {
        key: "crm.contacts.read",
        category: "crm",
        description: "Read CRM contacts, companies and their timelines",
    },
    PermissionDef {
        key: "crm.contacts.create",
        category: "crm",
        description: "Create CRM contacts and companies",
    },
    PermissionDef {
        key: "crm.contacts.update",
        category: "crm",
        description: "Edit CRM contacts and companies",
    },
    PermissionDef {
        key: "crm.contacts.delete",
        category: "crm",
        description: "Archive CRM contacts and companies",
    },
    PermissionDef {
        key: "crm.contacts.merge",
        category: "crm",
        description: "Merge two CRM records into one",
    },
    PermissionDef {
        key: "crm.fields.sensitive.read",
        category: "crm",
        description: "Read the CRM fields a role otherwise cannot see",
    },
    // Slice 2's two keys. Both exist because both acts are a way to **copy** the records rather
    // than work with them: a saved view hands the whole filtered set to whoever opens it next, and
    // an import writes a whole file of contacts in one request. Separating them from `.read` and
    // `.create` is what lets a role look at the CRM without being able to exfiltrate or bulk-load
    // it — a distinction an audit can answer.
    PermissionDef {
        key: "crm.views.manage",
        category: "crm",
        description: "Create and delete saved CRM views (a view is shared with its organization)",
    },
    PermissionDef {
        key: "crm.contacts.import",
        category: "crm",
        description: "Import contacts and companies from a CSV",
    },
    // Slice 3's three keys. Deals are separated from contacts for the same reason the others
    // are: a pipeline exposes an organization's commercial position — its total open value and
    // its win rate are the two numbers a competitor most wants — so reading the board is its
    // own decision rather than a side effect of being able to read a contact. Moving a card
    // between columns is separate from editing it, and reshaping the pipeline itself (the
    // stage editor) is the one change that retroactively rewrites everyone's history, so it
    // gets a key of its own.
    PermissionDef {
        key: "crm.deals.read",
        category: "crm",
        description: "Read CRM deals, the pipeline board and the forecast",
    },
    PermissionDef {
        key: "crm.deals.create",
        category: "crm",
        description: "Create CRM deals",
    },
    PermissionDef {
        key: "crm.deals.update",
        category: "crm",
        description: "Edit CRM deals and move them between stages",
    },
    PermissionDef {
        key: "crm.deals.delete",
        category: "crm",
        description: "Archive CRM deals",
    },
    PermissionDef {
        key: "crm.pipelines.manage",
        category: "crm",
        description: "Create and reshape pipelines and their stages",
    },
    // Slice 4's keys. An activity is a note a person wrote about a person, so reading the feed
    // and reading one record's timeline are the *same* exposure and share a key — a separate
    // "timeline" key would let someone read a contact's history through the deal screen while
    // being refused on the contact. Writing one is its own decision because the activity feed is
    // the only place a CRM record's history can be *added* to.
    PermissionDef {
        key: "crm.activities.read",
        category: "crm",
        description: "Read the CRM activity feed and a record's merged timeline",
    },
    PermissionDef {
        key: "crm.activities.create",
        category: "crm",
        description: "Log CRM activities (calls, meetings, notes and tasks)",
    },
    // The copilot reads a record and proposes a sentence; it never writes one. The key exists
    // because a model that can read the whole CRM is a data-exfiltration surface even when it
    // only ever returns text, and the audit of the call is the record of what it saw.
    PermissionDef {
        key: "crm.copilot.use",
        category: "crm",
        description: "Ask the CRM copilot to summarize a deal or draft a follow-up",
    },
    // The form → lead ingress. Reading the log of submissions that arrived and deciding what a
    // submission becomes are separate decisions with separate consequences, so they are separate
    // keys: a role that watches the pipeline fill up should not be able to silence it, and the
    // person who configures routing is rarely the person who reads it.
    PermissionDef {
        key: "crm.leads.read",
        category: "crm",
        description: "Read the CRM lead inbox: form submissions and what became of them",
    },
    PermissionDef {
        key: "crm.leads.manage",
        category: "crm",
        description: "Configure form → lead routing and run the ingress drain",
    },
    // Sales (docs/requests/REQ-052). The selling side splits the way the relationship layer
    // does — read, create, edit, archive — and adds the two powers that are genuinely different
    // acts rather than a stricter version of editing:
    //
    // * **`sales.quotes.send`** exists because sending is the moment a document stops being the
    //   organization's and becomes the customer's. From that point the lines are frozen, the
    //   number is immutable and a public link exists that anyone holding it can open. A role
    //   that may write a quote but not send it can prepare work a manager reviews, which is the
    //   normal shape of a sales desk.
    // * **`sales.orders.confirm`** is the stock decision. Confirming reserves inventory, so it
    //   moves physical goods; cancelling releases them. It is separated from `.update` because
    //   the damage of a wrong confirmation is not a wrong label on a row.
    PermissionDef {
        key: "sales.products.read",
        category: "sales",
        description: "Read the sales product catalog and its price lists",
    },
    PermissionDef {
        key: "sales.products.manage",
        category: "sales",
        description: "Create and edit products and their price lists",
    },
    PermissionDef {
        key: "sales.pricelists.read",
        category: "sales",
        description: "Read price lists and the prices they assign",
    },
    PermissionDef {
        key: "sales.pricelists.manage",
        category: "sales",
        description: "Create price lists and replace their price rows",
    },
    PermissionDef {
        key: "sales.quotes.read",
        category: "sales",
        description: "Read quotes, their versions and the public link state",
    },
    PermissionDef {
        key: "sales.quotes.create",
        category: "sales",
        description: "Create quotes and duplicate an existing one",
    },
    PermissionDef {
        key: "sales.quotes.update",
        category: "sales",
        description: "Edit a draft quote and cancel it",
    },
    PermissionDef {
        key: "sales.quotes.send",
        category: "sales",
        description: "Send a quote to the customer, mint its public link and request approval",
    },
    PermissionDef {
        key: "sales.orders.read",
        category: "sales",
        description: "Read sales orders and their stock reservation state",
    },
    PermissionDef {
        key: "sales.orders.create",
        category: "sales",
        description: "Create sales orders from an accepted quote or by hand",
    },
    PermissionDef {
        key: "sales.orders.confirm",
        category: "sales",
        description: "Confirm an order (reserving stock), cancel it and create its invoice draft",
    },
    PermissionDef {
        key: "sales.reports.read",
        category: "sales",
        description: "Read the sales reports and export them as CSV",
    },

    // Inventory (docs/requests/REQ-053). The family is split by **what a mistake costs**, not by
    // screen, and three of the five exist for a reason a reader would otherwise have to infer:
    //
    // * `inventory.movements.record` is not implied by `inventory.items.manage`. Fixing a
    //   threshold and altering a balance are different acts, and the second one is the one the
    //   ledger is there to record.
    // * `inventory.negative.manage` **guards no route at all.** The schema cannot ask who is
    //   calling, so the rule is a service check inside the write: it is the only permission on
    //   this platform whose entire meaning is "the refusal this module would otherwise make is
    //   permitted for you", and giving it a route would be giving it a second meaning.
    // * `inventory.locations.manage` is separate from the movement key because closing a location
    //   makes stock unreachable, which is a structural change rather than a day's work.
    // * `inventory.adjustment.approve` is separate from `inventory.movements.record` because
    //   approving somebody else's recount is a **second pair of eyes**, not a bigger pen: a
    //   manager who may approve is not thereby authorised to move stock, and an operator who may
    //   move stock is not thereby authorised to wave their own through. The module refuses a
    //   self-approval in the service, so the key alone is not the whole rule — it is the first
    //   half.
    PermissionDef {
        key: "inventory.items.read",
        category: "inventory",
        description: "Read items, stock levels, the movement ledger and the warehouse tree",
    },
    PermissionDef {
        key: "inventory.items.manage",
        category: "inventory",
        description: "Create, edit and archive inventory items",
    },
    PermissionDef {
        key: "inventory.movements.record",
        category: "inventory",
        description: "Record stock movements (receipts, issues, adjustments, transfers)",
    },
    PermissionDef {
        key: "inventory.locations.manage",
        category: "inventory",
        description: "Create, rename and deactivate warehouses, locations and the thresholds",
    },
    PermissionDef {
        key: "inventory.negative.manage",
        category: "inventory",
        description: "Allow a correction to take stock below zero (needed with reason `correction`)",
    },
    PermissionDef {
        key: "inventory.adjustment.approve",
        category: "inventory",
        description: "Decide another operator's over-threshold stock adjustment (approve or reject)",
    },
    // Slice 3 split the movement key in two rather than adding a fifth. The reason is the same
    // one that separated `inventory.adjustment.approve` from `inventory.movements.record`, and
    // it is about **what a mistake costs**:
    //
    // * `inventory.movements.record` moves a number at one place. A mistake is a correction.
    // * `inventory.transfers.manage` moves a number at **two** places and asserts the goods
    //   physically moved between them, which is a claim about the world rather than about a
    //   balance. A dispatch books goods as in-transit on a truck that may never arrive, and the
    //   role that may do that is not the role that may note a shelf went down by two.
    //
    // There is deliberately **no `inventory.alerts.*` key**: raising and clearing an alert is the
    // sweep's judgement about a balance, and a permission to *see* the inbox is
    // `inventory.items.read`. A key that guards nothing is a comment, and a key that guards
    // reading is a second name for a key that already exists.
    PermissionDef {
        key: "inventory.transfers.manage",
        category: "inventory",
        description: "Create, dispatch, receive and cancel stock transfers between locations",
    },
    // Slice 4 is a third split rather than an entry that reuses an existing key, and the same
    // question decides it: **what does a mistake here cost, and who else would pay it?**
    //
    // * `inventory.movements.record` moves a number at one place, and a mistake there is a
    //   correction somebody can see on the ledger.
    // * `inventory.stocktake.manage` overwrites the balance of **every** location in a scope
    //   from a count, in one act, on the word of one person. A count that is wrong is not wrong
    //   in one row: it is wrong in all of them at once, and the rows it writes look like real
    //   movements unless somebody finds the document behind them.
    //
    // So it does not inherit the movement key, and opening a sheet is not enough: the count is
    // the part anybody may do, and the close is the only part that writes.
    PermissionDef {
        key: "inventory.stocktake.manage",
        category: "inventory",
        description: "Count a location and post the variances a count finds",
    },
    // Accounting (docs/requests/REQ-054, slice 1). The family splits the way the other three do —
    // read, write, release — with one key whose reason is worth reading twice, because it is the
    // one that could plausibly have been folded into a neighbour and should not have been:
    //
    // * `accounting.accounts.read` / `.manage` cover the chart and the rates together. They are
    //   the same decision: an account code is what a journal line picks, and a rate is what a
    //   line defaults to. Splitting them would let a role maintain the tree it is judged against
    //   without being able to change what a document defaults to — or the reverse, which is
    //   worse, because the rate is the one that changes numbers somebody already issued.
    // * **`accounting.journal.manage` is a permission of its own because posting is a claim, not
    //   an edit.** A bookkeeper who may prepare a journal is a normal role; the person who may
    //   post is the one whose name is on the entry, and the balance invariant means every posted
    //   entry is a promise that the books add up. That is the same argument that separated
    //   `inventory.movements.record` from `inventory.stocktake.manage` one wave earlier.
    PermissionDef {
        key: "accounting.accounts.read",
        category: "accounting",
        description: "Read the chart of accounts and the organization's tax rates",
    },
    PermissionDef {
        key: "accounting.accounts.manage",
        category: "accounting",
        description: "Add and edit accounts and tax rates; deactivate an account",
    },
    PermissionDef {
        key: "accounting.journal.read",
        category: "accounting",
        description: "Read journal entries, their lines and both totals",
    },
    PermissionDef {
        key: "accounting.journal.manage",
        category: "accounting",
        description: "Post a manual journal entry (a balanced, attributed, permanent record)",
    },
    // Invoices (docs/requests/REQ-054, slice 2). Three keys, and the middle one is the
    // interesting split:
    //
    // * `accounting.invoices.read` / `.create` / `.send`. A draft is a document nobody has seen, so
    //   creating one is cheap. **`send` is not**: it gives a number to a customer, fires
    //   `accounting.invoice.issued` to every webhook subscriber, and makes the document immutable
    //   from that moment — the flow becomes void-and-duplicate. A role that may draft but not
    //   issue is an ordinary role (a sales assistant preparing invoices for review), so the
    //   boundary is a real one and not a formality.
    // * Void shares `send`'s layer rather than getting a key of its own, and the reason is that
    //   both are statements **about money owed** rather than edits of a private document. Splitting
    //   them would produce a role that can tell a customer an invoice is cancelled while not being
    //   able to issue it, which is a strictly worse configuration than either.
    PermissionDef {
        key: "accounting.invoices.read",
        category: "accounting",
        description: "Read invoices, their lines and their outstanding balances",
    },
    PermissionDef {
        key: "accounting.invoices.create",
        category: "accounting",
        description: "Write a draft invoice, manual or converted from a sales order",
    },
    PermissionDef {
        key: "accounting.invoices.send",
        category: "accounting",
        description:
            "Issue, void and overdue-sweep an invoice (a permanent statement about money owed)",
    },
    // Payments (docs/requests/REQ-054, slice 3). Three keys, and the third is the one worth
    // arguing for:
    //
    // * `accounting.payments.read` / `.record` is the ordinary split — seeing what came in is
    //   not the same act as writing it down.
    // * `.record` is split from reversal rather than sharing it. Recording money that arrived is
    //   the routine act of a bookkeeper; **undoing** a recorded payment rewrites an invoice's
    //   status and posts a counter entry against the ledger, so it is the statement that the
    //   books were wrong, and the same argument that separated `accounting.journal.manage` from
    //   reading the journal applies a second time here.
    // * **`accounting.payments.overpay` is a permission of its own and the narrowest in the
    //   family.** Every other key in this module gates an action that is legitimate. This one
    //   gates the action that is *arithmetically wrong*: allocating more to an invoice than it
    //   has outstanding. It is refused with a 422 by default, and holding the key is how a
    //   person says "I know, it is a goodwill write-off". Folding it into `.record` would hand
    //   every bookkeeper the ability to overstate what was collected — which no permission
    //   review anywhere in the platform would call intended.
    PermissionDef {
        key: "accounting.payments.read",
        category: "accounting",
        description: "Read payments, their allocations and what each one settled",
    },
    PermissionDef {
        key: "accounting.payments.record",
        category: "accounting",
        description:
            "Record a payment and apply it to invoices (each allocation within its outstanding)",
    },
    PermissionDef {
        key: "accounting.payments.reverse",
        category: "accounting",
        description: "Reverse a payment: release its allocations and post the counter entry",
    },
    PermissionDef {
        key: "accounting.payments.overpay",
        category: "accounting",
        description:
            "Allocate more to an invoice than it has outstanding (refused with 422 without this)",
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
    fn the_crm_family_is_catalogued() {
        // REQ-051: the relationship layer separates reading, creating, editing, archiving and
        // merging, and the flagged fields are a separate power — a role that may read a contact
        // is not automatically a role that may read its contract note.
        for key in [
            "crm.contacts.read",
            "crm.contacts.create",
            "crm.contacts.update",
            "crm.contacts.delete",
            "crm.contacts.merge",
            "crm.fields.sensitive.read",
            "crm.views.manage",
            "crm.contacts.import",
            "crm.deals.read",
            "crm.deals.create",
            "crm.deals.update",
            "crm.deals.delete",
            "crm.pipelines.manage",
            "crm.activities.read",
            "crm.activities.create",
            "crm.copilot.use",
            "crm.leads.read",
            "crm.leads.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("crm"),
                "{key} belongs to the crm category"
            );
        }
    }

    #[test]
    fn the_inventory_family_is_catalogued_and_one_key_guards_no_route() {
        // REQ-053. Five keys guard routes and one does not — `inventory.negative.manage`
        // is a service rule the schema cannot check, so it exists only to unlock one refusal.
        // A catalogue entry for it is what lets a role **hold** it; nothing in `routes/inventory.rs`
        // may `require()` it, and this test cannot see that, so the comment in the route file is
        // the second half of the guarantee and the assertion here is the first.
        for key in [
            "inventory.items.read",
            "inventory.items.manage",
            "inventory.movements.record",
            "inventory.locations.manage",
            "inventory.negative.manage",
            "inventory.adjustment.approve",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("inventory"),
                "{key} belongs to the inventory category"
            );
        }
    }

    #[test]
    fn the_sales_family_is_catalogued() {
        // REQ-052: the catalog, the price lists and the documents are read, written and
        // *released* separately, because sending a quote and confirming an order are the two
        // acts a role must be able to withhold from a seller who may otherwise prepare anything.
        for key in [
            "sales.products.read",
            "sales.products.manage",
            "sales.pricelists.read",
            "sales.pricelists.manage",
            "sales.quotes.read",
            "sales.quotes.create",
            "sales.quotes.update",
            "sales.quotes.send",
            "sales.orders.read",
            "sales.orders.create",
            "sales.orders.confirm",
            "sales.reports.read",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("sales"),
                "{key} belongs to the sales category"
            );
        }
    }

    #[test]
    fn the_accounting_family_is_catalogued_and_posting_is_its_own_key() {
        // REQ-054, slice 1. Four keys for the chart, the rates and the journal: the chart and the
        // rates read/manage as one pair (they are the same decision — what a line picks and what
        // it defaults to), and the journal split read/post because posting is a claim with the
        // poster's name on it, not an edit.
        for key in [
            "accounting.accounts.read",
            "accounting.accounts.manage",
            "accounting.journal.read",
            "accounting.journal.manage",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("accounting"),
                "{key} belongs to the accounting category"
            );
        }
    }

    #[test]
    fn the_invoice_family_is_catalogued_and_issuing_is_its_own_key() {
        // REQ-054, slice 2. Slice 1's test asserted these three keys were ABSENT — the routes did
        // not exist yet and a key with no route is a promise the permission screen makes that the
        // product does not keep. This test is the same assertion run in the other direction: the
        // keys exist because the routes do, and `send` is separate from `create` because issuing
        // a number to a customer is not drafting one.
        for key in [
            "accounting.invoices.read",
            "accounting.invoices.create",
            "accounting.invoices.send",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("accounting"),
                "{key} belongs to the accounting category"
            );
        }
        // The keys the REQ's API table names for later slices must STILL not be here. **This
        // guard moved forward in slice 3 and it is aimed at slice 4**, for the same reason slice
        // 2 moved it: a key with no route is a promise the permission screen makes that the
        // product does not keep. Slice 3 turned `accounting.payments.*` from "later" into "real",
        // so those three left this list and entered the assertion above, and the list now names
        // what slice 4 has to deliver.
        for later in ["accounting.expenses.read", "accounting.reports.read"] {
            assert!(
                get(later).is_none(),
                "{later} belongs to slice 4 and must not appear before its route does"
            );
        }
    }

    #[test]
    fn the_payment_family_is_catalogued_and_overpaying_is_its_own_key() {
        // REQ-054, slice 3. Four keys, and the assertion that is worth the test is the last one:
        // `overpay` must be a key **of its own**, not a synonym for `record`. Every other key in
        // this catalogue gates an action that is legitimate; this one gates the action that is
        // arithmetically wrong, and folding it into `record` would hand every bookkeeper the
        // ability to allocate more to an invoice than it has outstanding.
        for key in [
            "accounting.payments.read",
            "accounting.payments.record",
            "accounting.payments.reverse",
            "accounting.payments.overpay",
        ] {
            assert_eq!(
                get(key).map(|entry| entry.category),
                Some("accounting"),
                "{key} belongs to the accounting category"
            );
        }

        // `reverse` is separate from `record`: recording money that arrived is a bookkeeper's
        // routine act, and undoing one rewrites an invoice and posts a counter entry.
        assert_ne!(
            get("accounting.payments.reverse").map(|entry| entry.description),
            get("accounting.payments.record").map(|entry| entry.description),
            "reversing and recording are different acts and must not share a description"
        );
        assert_ne!(
            get("accounting.payments.overpay").map(|entry| entry.description),
            get("accounting.payments.record").map(|entry| entry.description),
            "the overpay override must not be described as recording"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(get("content.pages.explode").is_none());
        assert!(expect_known("nope").is_err());
        assert!(expect_known("content.pages.read").is_ok());
        assert_eq!(keys().len(), CATALOGUE.len());
    }
}
