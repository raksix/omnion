//! The event catalogue: one registry of every name the platform records.
//!
//! A bus without a catalogue is a bus nobody can subscribe to. Before this module the only
//! truth about an event name was the string literal at the call site — which meant the
//! endpoint form had nothing to render a picker from, the `/events` screen had nothing to
//! describe, and a typo in one emitter became a name that no receiver could ever subscribe
//! to, silently.
//!
//! So the names move here. One table, one place, three jobs:
//!
//! * **The picker.** [`CATALOGUE`] is what `/api/v1/events/catalogue` returns and what the
//!   endpoint form's grouped multi-select is built from — an operator only ever sees names
//!   that really exist.
//! * **The subscription vocabulary.** [`reconcile`] turns what an operator typed into the
//!   stored list: group wildcards (`page.*`) are expanded to the names that exist now, and the
//!   wildcard itself is kept, so the next catalogue growth re-expands it without the endpoint
//!   being edited (REQ-016 slice 1).
//! * **The drift detector.** A test walks the workspace for `NewEvent::new("…")` and fails
//!   when an emitter uses a name this table does not carry, which is the only way a registry
//!   stays true without anybody remembering to update it.
//!
//! **The `reserved` status is not a placeholder button.** `order.created` is a name the
//! commerce module will record; it is listed, described and subscribable today, and it is
//! marked reserved so the panel can say *which module ships it* instead of implying the
//! platform is broken. Names that exist in neither column are refused by [`reconcile`] only
//! when they are a group whose group is entirely unknown — an unknown concrete name is kept,
//! because a plugin owns names this table has never heard of, and REQ-016's whole point is
//! that other software can listen in.

use std::collections::BTreeSet;

use crate::error::{EventsError, Result};

/// Suffix that turns an event name into a group subscription.
const GROUP_SUFFIX: &str = ".*";

/// What kind of value one payload field carries.
///
/// A receiver that knows the kind does not have to guess before it parses; a receiver that
/// does not care is not blocked by it. `Json` is the honest answer for a field whose shape
/// belongs to the emitter, and `Any` is for names only this table knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// An opaque identifier.
    Uuid,
    /// Free text.
    String,
    /// A whole number.
    Integer,
    /// A true/false.
    Boolean,
    /// An RFC 3339 timestamp.
    Timestamp,
    /// Nested structure, described by the emitter.
    Json,
    /// Known to exist, shape not declared here.
    Any,
}

impl FieldKind {
    /// The lowercase name the API and the panel render.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uuid => "uuid",
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Json => "json",
            Self::Any => "any",
        }
    }

    /// Parse a stored kind name.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "uuid" => Self::Uuid,
            "string" => Self::String,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "timestamp" => Self::Timestamp,
            "json" => Self::Json,
            _ => Self::Any,
        }
    }
}

/// One field of an event's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Field name, as it appears in the payload.
    pub name: &'static str,
    /// What it carries.
    pub kind: FieldKind,
    /// Whether a receiver may rely on it being there.
    ///
    /// `true` is a promise and is paid for: an emitter that omits a required field is a bug,
    /// and [`crate::catalogue`] tests the emitters that this crate can see.
    pub required: bool,
}

/// Whether the platform records this name yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Emitted by the platform today.
    Live,
    /// Named, described and subscribable; the owning module is not shipped yet.
    Reserved,
}

impl Status {
    /// The lowercase name the API and the panel render.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Reserved => "reserved",
        }
    }

    /// Parse a stored status name.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "reserved" => Self::Reserved,
            _ => Self::Live,
        }
    }
}

/// One entry of the catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventDefinition {
    /// Dotted, lower-case name (`page.published`).
    pub name: &'static str,
    /// Which part of the platform it belongs to; the picker's grouping and the feed's filter.
    pub area: &'static str,
    /// One sentence a consumer can read before subscribing.
    pub description: &'static str,
    /// The fields the payload carries.
    pub payload_fields: &'static [Field],
    /// Whether it is recorded today.
    pub status: Status,
}

impl EventDefinition {
    /// The part before the first dot — the group this event can be subscribed to as a whole.
    #[must_use]
    pub fn group(&self) -> &'static str {
        self.name.split('.').next().unwrap_or(self.name)
    }

    /// The fields a receiver can rely on.
    #[must_use]
    pub fn required_fields(&self) -> impl Iterator<Item = &Field> {
        self.payload_fields.iter().filter(|field| field.required)
    }
}

macro_rules! catalogue {
    (
        $(
            $name:literal, $area:literal, $status:ident, $description:literal,
            [ $( ( $field:literal, $kind:ident, $required:ident ) ),* ] ;
        )+
    ) => {
        /// Every event name the platform knows, one row per name.
        ///
        /// The table is deliberately a macro rather than a hand-written array: a row is then
        /// a single line, the shape of a row is checked by the compiler, and adding an event
        /// is one line rather than a struct literal spanning eight.
        ///
        /// The status and the required flag are matched as **identifiers**, not strings, and
        /// the macro turns them into the values themselves. The first version parsed them at
        /// compile time with a `const fn` and stopped: `str` equality is not const-stable on
        /// this toolchain, so the table would not build. Matching the token is both simpler
        /// and stricter — a row that says `Reserved` in the wrong case is a compile error
        /// rather than a row that silently reads as `live`.
        pub const CATALOGUE: &[EventDefinition] = &[$(
            EventDefinition {
                name: $name,
                area: $area,
                description: $description,
                status: Status::$status,
                payload_fields: &[$(
                    Field {
                        name: $field,
                        kind: FieldKind::$kind,
                        required: catalogue!(@required $required),
                    },
                )*],
            },
        )*];
    };

    // `req` is the whole vocabulary: a required field is a promise, an optional one is the
    // absence of it, and there is no third spelling to misread.
    (@required req) => {
        true
    };
    (@required opt) => {
        false
    };
}

// The rows below spell their area as a plain string, not a `const`, on purpose: a macro
// fragment can only match a `$area:literal`, and a bare `const` path is a different token
// kind. The cost of a repeated area string is one row of repetition; the benefit is that the
// table stays a table a person can read top to bottom.
catalogue! {
    // ---- Content -------------------------------------------------------------------------------
    "page.created", "content", Live,
    "A page was created as a working draft.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req), ("status", String, req)];
    "page.updated", "content", Live,
    "A page's draft was edited.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req), ("status", String, opt)];
    "page.published", "content", Live,
    "A page's draft was published and the public site serves it.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req), ("status", String, opt),
     ("revision_id", Uuid, req), ("revision_no", Integer, req), ("title", String, opt)];
    "page.unpublished", "content", Reserved,
    "A published page went back to being a draft.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req), ("status", String, opt)];
    "page.deleted", "content", Live,
    "A page was removed from the site.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req)];
    "page.restored", "content", Live,
    "A deleted page was restored from the trash.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("slug", String, req)];
    "translation.updated", "content", Live,
    "A page's translation was edited.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("locale", String, req)];
    "translation.published", "content", Reserved,
    "A page's translation was published.",
    [("page_id", Uuid, req), ("site_id", Uuid, req), ("locale", String, req)];

    // ---- Media ---------------------------------------------------------------------------------
    // The upload fact is `media.created`, not `media.uploaded`. The registry is written from
    // what the code emits rather than from what the request imagined: renaming the row to
    // `media.uploaded` would make the catalogue agree with the spec and disagree with every
    // receiver that has already subscribed, and the cost of that is a silent break in
    // somebody else's software. A request that wants a different word changes the emitter.
    "media.created", "media", Live,
    "A file finished uploading and became a library item.",
    [("media_id", Uuid, req), ("site_id", Uuid, req), ("filename", String, req), ("content_type", String, opt)];
    "media.updated", "media", Live,
    "A library item's metadata or file was changed.",
    [("media_id", Uuid, req), ("site_id", Uuid, opt), ("filename", String, opt)];
    "media.deleted", "media", Live,
    "A library item was moved to the trash.",
    [("media_id", Uuid, req), ("site_id", Uuid, opt)];
    "media.restored", "media", Live,
    "A trashed library item came back.",
    [("media_id", Uuid, req)];
    "media.purged", "media", Live,
    "A library item was permanently removed.",
    [("media_id", Uuid, req)];
    "media.version_created", "media", Live,
    "A new version of a library item was stored.",
    [("media_id", Uuid, req), ("version", Integer, req)];
    "media.folder_created", "media", Live,
    "A media folder was created.",
    [("folder_id", Uuid, req), ("parent_id", Uuid, opt)];
    "media.folder_moved", "media", Live,
    "A media folder was moved under another folder.",
    [("folder_id", Uuid, req), ("parent_id", Uuid, opt)];
    "media.folder_deleted", "media", Live,
    "A media folder was removed.",
    [("folder_id", Uuid, req)];
    "media.share_created", "media", Live,
    "A media share link was opened.",
    [("share_id", Uuid, req), ("media_id", Uuid, req), ("url", String, opt)];
    "media.share_revoked", "media", Live,
    "A media share link was closed.",
    [("share_id", Uuid, req), ("media_id", Uuid, opt)];
    "media.scan_flagged", "media", Live,
    "A scan found something that needs a person's decision.",
    [("media_id", Uuid, req), ("reason", String, req)];
    "media.retention_applied", "media", Live,
    "A retention rule changed or removed items.",
    [("rule", String, req), ("affected", Integer, opt)];

    // The AI router (REQ-098). `unresolved` is the only one that is recorded today, and it is
    // the one an operator actually needs: it is the signal that a request came in and *nothing*
    // in the pool could answer it, which is the failure that otherwise surfaces as a feature
    // that quietly stopped working rather than as an error anywhere.
    "ai.route.unresolved", "ai", Live,
    "A model request could not be resolved to a usable model by any rule.",
    [("decision_id", Uuid, req), ("task", String, req), ("rule", String, opt),
     ("feature", String, opt), ("requested", String, opt), ("requirements", Json, opt)];
    // The approval gate (REQ-101). `ai.approval.applied` is the one an operator subscribes to,
    // because the approval events themselves only say what a *person decided* and this one is
    // the only signal that the change actually reached the content table -- a missing name here
    // would leave the whole "nothing dangerous happens without a human" claim verifiable only
    // by reading the audit log.
    "ai.approval.applied", "ai", Live,
    "An approved preview was written through its gated pipeline.",
    [("approval_id", Uuid, req), ("resource_type", String, req),
     ("resource_id", String, req)];
    // The change-set editor (REQ-101 slice 3). All three are `Live` because a proposal nobody
    // hears about is a proposal nobody confirms: the editor's whole purpose is to put an
    // operation in front of a human, and a screen that fills up while a person is looking at
    // a different tab has failed quietly. `confirmed` is the one an operator subscribes to --
    // it is the only signal that a model's proposed work became a promise, and the payload
    // carries `irreversible` because a set that deletes is the one worth an e-mail.
    "ai.changeset.proposed", "ai", Live,
    "A conversation proposed a set of operations for a person to review.",
    [("change_set_id", Uuid, req), ("title", String, req),
     ("operations", Integer, req)];
    "ai.changeset.confirmed", "ai", Live,
    "A person confirmed a change set, moving it towards application.",
    [("change_set_id", Uuid, req), ("operations", Integer, req),
     ("irreversible", Boolean, req)];
    "ai.changeset.discarded", "ai", Live,
    "A person discarded a change set, with the reason they gave.",
    [("change_set_id", Uuid, req), ("reason", String, req)];
    // `failed` and `applied` are the two halves of the same promise, and both carry `reason`
    // so a subscriber can render the same line for either. `failed` is the one that matters
    // most: an all-or-nothing apply that rolled back writes nothing anywhere, so without this
    // name a subscriber cannot tell "the set is still waiting" from "the set was tried and
    // undid itself" — and the second is the one a person has to act on.
    "ai.changeset.failed", "ai", Live,
    "A confirmed change set was rolled back: no operation was applied.",
    [("change_set_id", Uuid, req), ("reason", String, req)];
    "ai.changeset.applied", "ai", Live,
    "A confirmed change set applied every operation it carried, in one transaction.",
    [("change_set_id", Uuid, req), ("operations", Integer, req)];
    // The tool registry (REQ-100). `ai.tool.denied` is the one an operator subscribes to: it
    // is the signal that a model tried to reach something it was not granted, which is the
    // visible form of a probing agent. The rest are the panel's own audit trail — a registry
    // whose limits and gates changed is a registry nobody could explain after an incident.
    "ai.tool.registered", "ai", Live,
    "A tool from the compiled catalogue was seeded into the registry.",
    [("tool_key", String, req), ("class", String, req), ("seed_version", Integer, opt)];
    "ai.tool.updated", "ai", Live,
    "A tool's limits, gate or enabled state changed.",
    [("tool_key", String, req), ("changed", Json, opt)];
    "ai.tool.disabled", "ai", Live,
    "A tool was switched off while agents still referenced it.",
    [("tool_key", String, req), ("agents", Json, opt)];
    "ai.tool.grant_changed", "ai", Live,
    "An identity's grant for one tool changed.",
    [("identity_id", Uuid, req), ("tool_key", String, req), ("effect", Boolean, req),
     ("changed_by", Uuid, opt)];
    "ai.tool.denied", "ai", Live,
    "An agent named a tool its identity does not grant. The alert hook for a probing agent.",
    [("run_id", Uuid, opt), ("step_id", Uuid, opt), ("tool_key", String, req),
     ("identity_id", Uuid, opt), ("reason", String, opt)];
    "ai.tool.failed", "ai", Live,
    "A tool call ran and failed.",
    [("run_id", Uuid, opt), ("step_id", Uuid, opt), ("tool_key", String, req),
     ("error_code", String, opt), ("duration_ms", Integer, opt)];
    "ai.tool.limited", "ai", Live,
    "A run hit a tool's per-run call cap.",
    [("run_id", Uuid, opt), ("tool_key", String, req), ("cap", Integer, opt)];
    "ai.identity.created", "ai", Live,
    "An AI identity — a named set of tool grants — was created.",
    [("identity_id", Uuid, req), ("key", String, req)];
    "ai.identity.updated", "ai", Live,
    "An AI identity's details or default flag changed.",
    [("identity_id", Uuid, req), ("key", String, req), ("changed", Json, opt)];
    "ai.identity.removed", "ai", Live,
    "An AI identity was removed along with its grants.",
    [("identity_id", Uuid, req), ("key", String, req)];
    // The air gap (REQ-106 slice 2). Two of these are the switch's PROOF rather than its
    // effects, which is why both sides are named: `enabled`/`disabled` is the audit entry an
    // operator reads to answer "who stopped the installation from calling out, and why", and
    // `call_refused` is the only evidence that the switch is still doing its job. A switch that
    // emits nothing when it fires is indistinguishable from a switch that is off, so the refusal
    // carries the provider AND the host — the count of these is the answer to "is anything still
    // leaving this machine?".
    "ai.airgap.enabled", "ai", Live,
    "The air-gap switch was turned on; every non-local AI call is now refused.",
    [("reason", String, req), ("actor_id", Uuid, opt), ("providers_blocked", Integer, opt)];
    "ai.airgap.disabled", "ai", Live,
    "The air-gap switch was turned off, so non-local calls are permitted again.",
    [("actor_id", Uuid, opt), ("reason", String, opt)];
    "ai.airgap.call_refused", "ai", Live,
    "The air gap refused a call before any request left the installation.",
    [("provider", String, req), ("host", String, opt), ("task", String, opt),
     ("model_key", String, opt)];
    "media.duplicate_merged", "media", Live,
    "A duplicate item was merged into the one that was kept.",
    [("kept_media_id", Uuid, req), ("merged_media_id", Uuid, req), ("affected", Integer, opt)];
    "media.storage_updated", "media", Live,
    "The media library's storage settings changed.",
    [("driver", String, req)];
    "media.preset_created", "media", Live,
    "A transform preset was created.",
    [("preset_id", Uuid, req), ("name", String, req)];
    "media.grant_changed", "media", Live,
    "Access to a media item was granted to somebody.",
    [("media_id", Uuid, req), ("subject", String, req)];
    "media.grant_removed", "media", Live,
    "Access to a media item was taken away.",
    [("media_id", Uuid, req), ("subject", String, req)];

    // The retention route emits this pair from one `NewEvent::new(if hold { … } else { … })`, so
    // the two names never appear as a plain string literal beside the constructor — which is why
    // the drift gate only found them once it learned to read a name that follows the marker on
    // the same line rather than inside it. The rows are `Live` because the route has been
    // recording them since legal hold shipped.
    //
    // `reason` is **required** on both: a hold with no stated reason is a retention decision
    // nobody can defend later, and the pair is how an outside system learns that a file it asked
    // to delete is not going to be deleted.
    "media.hold_placed", "media", Live,
    "A legal hold was placed on a file: retention will not touch it until the hold is released.",
    [("media_id", Uuid, req), ("site_id", Uuid, req), ("filename", String, opt),
     ("reason", String, req)];
    "media.hold_released", "media", Live,
    "A legal hold was released; retention may now apply to the file again.",
    [("media_id", Uuid, req), ("site_id", Uuid, req), ("filename", String, opt),
     ("reason", String, req)];

    // ---- Identity ------------------------------------------------------------------------------
    "user.created", "identity", Live,
    "An account was created.",
    [("user_id", Uuid, req), ("email", String, opt), ("role", String, opt)];
    "user.updated", "identity", Live,
    "An account's details or role changed.",
    [("user_id", Uuid, req), ("email", String, opt), ("role", String, opt)];
    "user.deleted", "identity", Live,
    "An account was removed.",
    [("user_id", Uuid, req)];
    "iam.mfa_enrolled", "identity", Live,
    "A second factor was enrolled on an account.",
    [("user_id", Uuid, req), ("method", String, opt)];
    "iam.mfa_removed", "identity", Live,
    "A second factor was removed from an account.",
    [("user_id", Uuid, req), ("method", String, opt)];
    "iam.session_revoked", "identity", Live,
    "A session was ended before it expired.",
    [("user_id", Uuid, req), ("session_id", Uuid, opt)];
    "iam.policy_changed", "identity", Live,
    "The authorization policy was edited.",
    [("policy", String, opt), ("action", String, req)];
    "iam.binding_created", "identity", Live,
    "A role was granted to a subject.",
    [("subject", String, req), ("role", String, req)];
    "iam.user_provisioned", "identity", Live,
    "An account was created or updated by an identity provider.",
    [("user_id", Uuid, req), ("provider", String, req)];
    "iam.provider_connected", "identity", Live,
    "An identity provider was connected or disconnected.",
    [("provider", String, req), ("connected", Boolean, req)];
    "iam.approval_requested", "identity", Live,
    "Somebody asked for access they do not have.",
    [("subject", String, req), ("permission", String, opt)];
    "iam.approval_decided", "identity", Live,
    "An access request was approved or refused.",
    [("subject", String, req), ("permission", String, opt), ("approved", Boolean, opt)];

    // ---- Security (REQ-012) -------------------------------------------------------------------
    //
    // Four names, and the reason they are all here rather than one is that they answer four
    // different questions an operator subscribes to separately: *did the platform check itself*,
    // *what does it now send on the wire*, *what does it refuse* and *somebody released a lock*.
    //
    // The payloads deliberately carry no policy **values**. `security.headers.updated` names the
    // CSP directive *names* and the mode; it does not carry the origins, because a header
    // policy travels to every webhook receiver and is still configuration somebody considers
    // sensitive. `security.rate_limits.updated` carries the *count* of scopes, not the numbers —
    // the numbers are in the audit entry and in the panel, and a bus event is the wrong place to
    // publish a limit to.
    "security.scan.completed", "security", Live,
    "The posture checks were re-run and a fresh result set was recorded.",
    [("run_id", Uuid, req), ("checks", Integer, req)];
    "security.headers.updated", "security", Live,
    "The response-header policy was saved; the next response already carries it.",
    [("csp_mode", String, req), ("directives", String, opt)];
    "security.rate_limits.updated", "security", Live,
    "The rate-limit document was saved, so the limiter is deciding by the new numbers.",
    [("scopes", Integer, req)];
    // `user_id` is the account that was locked, NOT the actor: the release is the unlock, and an
    // operator subscribing to it wants to know which account was let back in. The actor already
    // rides the envelope, so repeating it here would be a second place for the two to disagree.
    "security.lockout.released", "security", Live,
    "An operator released an account's brute-force lockout before it expired.",
    [("user_id", Uuid, req)];
    // `user_id` is the account that was locked, NOT the actor, for the same reason as the
    // release above, and there is no actor to name here at all: the lock is applied by an
    // anonymous caller guessing a password. `attempts` is the threshold that fired and
    // `lockout_minutes` how long it lasts — both are configuration an operator subscribed here
    // wants, and neither is a credential. What is deliberately absent is the attempted password
    // and the address: an event bus is a fan-out to third-party receivers, and a brute-force
    // attempt is exactly the payload nobody should be copying anywhere.
    "security.lockout.triggered", "security", Live,
    "An account reached its brute-force threshold and is now locked out.",
    [("user_id", Uuid, req), ("attempts", Integer, req), ("lockout_minutes", Integer, opt)];
    // A finding that *opens* is the one security fact an operator wires to a third party, so
    // this is the name REQ-012's Events section promises them. Two payload decisions, and both
    // are about the same thing — **the scan's content must not travel**:
    //
    // * `title` and `description` are absent. A dependency title is a package name and a
    //   description is whatever the CI vendor wrote, which is attacker-influenced free text
    //   being copied to every receiver the operator has. The finding is *identifiable* from
    //   `finding_id` alone, because the panel reads it back with the same guard it protects.
    // * `evidence` is absent for the same reason with more force: it is the raw report entry,
    //   and the ingest path's own heuristic for "this document carries a credential" is a
    //   heuristic. A bus is not the place to test it again.
    //
    // `severity` IS carried, because the receiver's decision is "page me or file a ticket",
    // and that decision is unreadable from a finding id alone.
    "security.finding.opened", "security", Live,
    "A new finding was raised — a check that found something, or a report that was ingested.",
    [("finding_id", Uuid, req), ("severity", String, req), ("source", String, req),
     ("component", String, opt), ("component_version", String, opt), ("fixed_in", String, opt)];
    // `action` is what makes this one event rather than two: a receiver that has to infer
    // whether a network was opened or closed from a diff of `cidr` lists is reimplementing this
    // module. `note` is the operator's own free text and is deliberately absent for the same
    // reason a finding's title is — this one fans out to third-party receivers.
    //
    // This row was MISSING while slice 4(a) shipped, and `every_emitted_name_is_in_the_catalogue`
    // is what said so: an emitter whose name is not in the catalogue records an event no
    // endpoint can subscribe to, so the rule an operator believes they are running applies to
    // nobody. It went unnoticed because nothing in the workspace asserts that gate is green on
    // the branch it was written on.
    "security.ip_rule.changed", "security", Live,
    "An IP access rule was added or removed, and the next request is judged by the new set.",
    [("action", String, req), ("rule_id", Uuid, req), ("kind", String, req), ("cidr", String, req)];

    // ---- Tenancy -------------------------------------------------------------------------------
    "site.created", "tenancy", Live,
    "A site was created.",
    [("site_id", Uuid, req), ("key", String, req), ("name", String, opt)];
    "site.updated", "tenancy", Live,
    "A site's settings or name changed.",
    [("site_id", Uuid, req), ("name", String, opt)];
    "site.archived", "tenancy", Live,
    "A site was archived and is no longer served.",
    [("site_id", Uuid, req)];
    "domain.added", "tenancy", Live,
    "A domain was pointed at a site.",
    [("domain_id", Uuid, req), ("site_id", Uuid, req), ("hostname", String, req)];
    "domain.verified", "tenancy", Reserved,
    "A domain answered the platform's ownership check.",
    [("domain_id", Uuid, req), ("site_id", Uuid, req), ("hostname", String, req)];
    "domain.removed", "tenancy", Live,
    "A domain was detached from its site.",
    [("domain_id", Uuid, req), ("site_id", Uuid, opt), ("hostname", String, opt)];

    // ---- Plugins, themes, workflows --------------------------------------------------------------
    "plugin.installed", "plugins", Reserved,
    "A plugin was installed.",
    [("plugin", String, req), ("version", String, opt)];
    "plugin.activated", "plugins", Reserved,
    "An installed plugin was switched on.",
    [("plugin", String, req)];
    "plugin.deactivated", "plugins", Reserved,
    "An installed plugin was switched off.",
    [("plugin", String, req)];
    "plugin.uninstalled", "plugins", Reserved,
    "A plugin was removed from the platform.",
    [("plugin", String, req)];
    "theme.activated", "themes", Live,
    "A theme was made the active one.",
    [("theme", String, req), ("site_id", Uuid, opt)];
    "workflow.run.started", "workflows", Reserved,
    "A workflow run began.",
    [("workflow_id", Uuid, req), ("run_id", Uuid, req), ("site_id", Uuid, opt)];
    "workflow.run.completed", "workflows", Reserved,
    "A workflow run finished every step.",
    [("workflow_id", Uuid, req), ("run_id", Uuid, req), ("duration_ms", Integer, opt)];
    "workflow.run.failed", "workflows", Reserved,
    "A workflow run stopped on a step that failed.",
    [("workflow_id", Uuid, req), ("run_id", Uuid, req), ("error", String, opt)];

    // ---- Webhooks ------------------------------------------------------------------------------
    "webhook.endpoint.created", "webhooks", Live,
    "A webhook endpoint was connected.",
    [("endpoint_id", Uuid, req), ("endpoint_name", String, req), ("url", String, opt)];
    "webhook.endpoint.updated", "webhooks", Live,
    "A webhook endpoint was changed.",
    [("endpoint_id", Uuid, req), ("endpoint_name", String, req)];
    "webhook.endpoint.removed", "webhooks", Live,
    "A webhook endpoint was removed.",
    [("endpoint_id", Uuid, req), ("endpoint_name", String, req)];
    "webhook.endpoint.tested", "webhooks", Live,
    "An operator sent a test delivery to an endpoint.",
    [("endpoint_id", Uuid, req), ("event_id", Integer, opt)];
    "webhook.secret.rotated", "webhooks", Live,
    "An endpoint's signing secret was replaced.",
    [("endpoint_id", Uuid, req), ("endpoint_name", String, req)];
    "webhook.test", "webhooks", Live,
    "A test delivery, sent by hand, to one endpoint.",
    [("endpoint_id", Uuid, req)];
    "webhook.delivery.failed", "webhooks", Live,
    "A delivery ran out of attempts and the receiver never took it.",
    [("delivery_id", Uuid, req), ("endpoint_id", Uuid, req), ("endpoint_name", String, req),
     ("event_name", String, req), ("attempts", Integer, opt), ("response_status", Integer, opt)];
    // The two retention events (REQ-016 slice 3). `changed` carries the window *before* and
    // *after*, because "the window is 30 days" on its own is not a record of anything — what an
    // audit needs is the transition, and a consumer that keeps history longer than the platform
    // did must be able to see the moment that changed. Neither carries a count of what was
    // deleted: that number is the run log's, and a payload that duplicates it is a second
    // place for the two to disagree.
    "webhook.retention.changed", "webhooks", Live,
    "An organization changed how long it keeps its event history.",
    [("previous_window_days", Integer, opt), ("window_days", Integer, req)];
    "webhook.retention.swept", "webhooks", Live,
    "A retention sweep removed history from the bus.",
    [("window_days", Integer, opt), ("events_deleted", Integer, opt),
     ("deliveries_deleted", Integer, opt)];

    // ---- Analytics, search, notifications -------------------------------------------------------
    "analytics.traffic_spike", "analytics", Live,
    "Traffic on a site left its normal band.",
    [("site_id", Uuid, opt), ("metric", String, opt)];
    "analytics.goal_reached", "analytics", Live,
    "A goal's target was hit.",
    [("goal_id", Uuid, opt), ("site_id", Uuid, opt)];
    "analytics.retention_purged", "analytics", Live,
    "A retention rule removed analytics rows.",
    [("rule", String, req), ("affected", Integer, opt)];
    "analytics.erasure_completed", "analytics", Live,
    "Everything held about one person was erased.",
    [("subject", String, opt)];
    "search.reindexed", "search", Live,
    "The search index was rebuilt.",
    [("scope", String, opt), ("indexed", Integer, opt)];
    "notification.created", "notifications", Live,
    "A notification was raised for somebody.",
    [("notification_id", Uuid, req), ("channel", String, opt)];
    "notification.routed", "notifications", Live,
    "A fact was turned into a notification by a route.",
    [("notification_id", Uuid, opt), ("route", String, opt), ("event_name", String, opt)];
    "notification.preferences.changed", "notifications", Live,
    "Somebody changed how or whether they are notified.",
    [("user_id", Uuid, req), ("field", String, opt)];

    // ---- System health --------------------------------------------------------------------------
    // The five names REQ-014's Events section names, and the reason this area exists at all is
    // the sentence "an operations endpoint subscribes to degraded and recovered" — which was,
    // until the emitters shipped, a sentence with no name behind it: the table had no `health`
    // area, so there was nothing to subscribe to and nothing for a receiver to wait on.
    //
    // `degraded` and `recovered` are a **pair on purpose**, and they are what the request calls
    // the two an operations endpoint listens for. A receiver that gets only `degraded` cannot
    // tell a resolved outage from a deleted endpoint, and one that gets only `recovered` has no
    // idea what came back.
    "health.service.degraded", "health", Live,
    "A dependency stopped answering, or answered too slowly.",
    [("service", String, req), ("from_state", String, req), ("to_state", String, req),
     ("message", String, opt), ("incident_id", Uuid, opt), ("suppressed", Boolean, opt)];
    "health.service.recovered", "health", Live,
        "A dependency that had been unhealthy is answering again.",
        [("service", String, req), ("from_state", String, req), ("to_state", String, req),
         ("duration_seconds", Integer, opt), ("incident_id", Uuid, opt)];
    "health.threshold.breached", "health", Live,
    "A metric went past its configured critical limit. Fires once per metric per window.",
    [("metric", String, req), ("value", Integer, opt), ("crit_limit", Integer, opt),
     ("window_start", Timestamp, opt)];
    "health.incident.acknowledged", "health", Live,
    "An operator claimed an incident.",
    [("incident_id", Uuid, req), ("service", String, req), ("actor", Uuid, req),
     ("note", String, opt)];
    "health.checks.completed", "health", Live,
    "A probe run finished; the worst state it concluded is on the payload.",
    [("state", String, req), ("services", Integer, req), ("worst_service", String, opt),
     ("samples", Integer, opt)];

    // ---- Backups and restoration -----------------------------------------------------------------
    // The two rows the backup centre's own routes emit, added when the drift gate started
    // naming them. Both are **Live and emitted**; what was missing was the row, which is the same
    // failure as any other unlisted name — an operator cannot subscribe to an event the picker
    // has never heard of, and the route has been recording it for every restore all along.
    "backup.restored", "backups", Live,
    "A restore finished. Carries what came back and what did not, so a receiver can tell a clean \
     restore from a partial one.",
    [("backup_id", Uuid, req), ("parts", Integer, opt), ("objects_restored", Integer, opt),
     ("objects_failed", Integer, opt), ("safety_backup_id", Uuid, opt)];

    // `notification.delivery.succeeded` is deliberately *not* here as a second row: the delivery
    // lifecycle already publishes `notification.created` and the per-channel state lives in
    // `notification_deliveries`. A "test delivery succeeded" audit fact is what this name is —
    // one key says whether it was a test — and `test` is required rather than optional precisely
    // so a receiver cannot read "succeeded" as a production delivery and page somebody for a
    // message a person deliberately asked the platform to send.
    "notification.delivery.succeeded", "notifications", Live,
    "A channel accepted a notification the reader asked to be sent, one attempt at a time.",
    [("notification_id", Uuid, req), ("channel", String, req), ("test", Boolean, req),
     ("delivered", Boolean, opt)];

    // ---- Commerce (reserved: the module is not shipped yet) -------------------------------------
    "order.created", "commerce", Reserved,
    "An order was placed. Listed now; the commerce module records it when it ships.",
    [("order_id", Uuid, req), ("site_id", Uuid, opt), ("total", String, opt)];
}
/// Every catalogue entry, in table order.
pub fn all() -> &'static [EventDefinition] {
    CATALOGUE
}

/// One entry by exact name.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static EventDefinition> {
    CATALOGUE.iter().find(|entry| entry.name == name)
}

/// `true` when the name is in the catalogue, live or reserved.
#[must_use]
pub fn is_known(name: &str) -> bool {
    lookup(name).is_some()
}

/// Every distinct area, in the order the table first mentions it.
///
/// The order is the table's, not alphabetical: the panel's picker reads better with `content`
/// before `commerce`, and a stable order means the screen does not reshuffle between releases.
#[must_use]
pub fn areas() -> Vec<&'static str> {
    let mut seen: Vec<&'static str> = Vec::new();
    for entry in CATALOGUE {
        if !seen.contains(&entry.area) {
            seen.push(entry.area);
        }
    }
    seen
}

/// The entries of one area.
#[must_use]
pub fn in_area(area: &str) -> Vec<&'static EventDefinition> {
    CATALOGUE
        .iter()
        .filter(|entry| entry.area == area)
        .collect()
}

/// The names of one area, for a group's expansion.
#[must_use]
pub fn group_members(group: &str) -> Vec<&'static str> {
    CATALOGUE
        .iter()
        .filter(|entry| entry.group() == group)
        .map(|entry| entry.name)
        .collect()
}

/// `true` when a subscription is a group wildcard (`page.*`).
#[must_use]
pub fn is_group(pattern: &str) -> bool {
    pattern
        .strip_suffix(GROUP_SUFFIX)
        .is_some_and(|group| !group.is_empty() && group.chars().all(|c| c.is_ascii_lowercase()))
}

/// The group a wildcard stands for, or `None` when it is not a wildcard.
#[must_use]
pub fn group_of(pattern: &str) -> Option<&str> {
    if is_group(pattern) {
        pattern.strip_suffix(GROUP_SUFFIX)
    } else {
        None
    }
}

/// Turn an operator's selection into the list an endpoint stores.
///
/// Two things happen, and both matter:
///
/// * **Groups are expanded, and the wildcard is kept.** Storing only today's expansion would
///   mean `page.*` silently stops covering an event added next release — the exact surprise
///   a group is meant to remove. Storing only the wildcard would mean an endpoint can be
///   subscribed to a group whose members do not exist, and a receiver could never tell what
///   it is getting. Both are stored, so the expansion is readable and the wildcard keeps
///   growing.
/// * **Deduplicated and sorted.** Two rows in the table that mean the same subscription must
///   compare equal, or the fan-out query would queue the same delivery twice.
///
/// An unknown concrete name is kept rather than refused: plugins own names this table has
/// never heard of, and refusing them would make the bus closed to the ecosystem REQ-016
/// exists for. A wildcard over a group that exists in the catalogue is expanded; one that
/// matches nothing is kept as written, because it is either a group's module that has not
/// shipped or a plugin's own namespace.
///
/// The 32-selection ceiling is checked on **what the operator typed**, not on the expansion.
/// The bound is a limit on a human's reading patience — "you picked 40 things" — and a group
/// they picked as one thing stays one thing however many names it stands for. Checking the
/// expanded list instead would refuse `page.*`, which is precisely the subscription that is
/// least work to write and most likely to be wanted.
pub fn reconcile(subscriptions: &[String]) -> Result<Vec<String>> {
    if subscriptions.is_empty() {
        return Err(EventsError::invalid_endpoint(
            "subscribe the endpoint to at least one event",
        ));
    }
    if subscriptions.len() > crate::validation::MAX_SUBSCRIPTIONS {
        return Err(EventsError::invalid_endpoint(format!(
            "an endpoint subscribes to at most {} events",
            crate::validation::MAX_SUBSCRIPTIONS
        )));
    }

    let mut names: BTreeSet<String> = BTreeSet::new();

    for raw in subscriptions {
        let pattern = raw.trim();
        if pattern.is_empty() {
            return Err(EventsError::invalid_endpoint(
                "an event subscription is empty",
            ));
        }

        // The group part has to be a legal first segment even though the `*` is not, so the
        // check is on the stem.
        if let Some(group) = group_of(pattern) {
            validate_group(group)?;
            names.insert(pattern.to_owned());
            names.extend(group_members(group).into_iter().map(str::to_owned));
            continue;
        }

        let name = crate::validation::validate_event_name(pattern)?;
        names.insert(name);
    }

    Ok(names.into_iter().collect())
}

/// `true` when an endpoint's stored subscription list covers one event.
///
/// The stored list is already expanded, so an exact match answers the common case. The group
/// check is the belt to that braces: a row written before a group was expanded, or a row an
/// operator's own SQL edited, still delivers, because the cost of a false negative here is a
/// receiver that silently stops hearing about a page.
#[must_use]
pub fn subscribed_to(subscriptions: &[String], event: &str) -> bool {
    if subscriptions.iter().any(|entry| entry == event) {
        return true;
    }

    let Some(group) = event.split('.').next() else {
        return false;
    };
    let wildcard = format!("{group}{GROUP_SUFFIX}");

    subscriptions.iter().any(|entry| entry == &wildcard)
}

/// Validate the stem of a group wildcard.
fn validate_group(group: &str) -> Result<()> {
    let stem = format!("{group}.probe");
    crate::validation::validate_event_name(&stem)?;
    Ok(())
}

/// Names a receiver cannot miss: every live name, for the API and the picker's default view.
#[must_use]
pub fn live_names() -> Vec<&'static str> {
    CATALOGUE
        .iter()
        .filter(|entry| entry.status == Status::Live)
        .map(|entry| entry.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_of<'a>(entries: &'a [&'static EventDefinition]) -> Vec<&'a str> {
        entries.iter().map(|entry| entry.name).collect()
    }

    #[test]
    fn the_table_is_well_formed() {
        assert!(!CATALOGUE.is_empty(), "the catalogue is never empty");

        let mut seen: Vec<&str> = Vec::new();
        for entry in CATALOGUE {
            assert!(
                crate::validation::validate_event_name(entry.name).is_ok(),
                "{} must be a legal event name",
                entry.name
            );
            assert!(
                !entry.description.trim().is_empty(),
                "{} must say what it means",
                entry.name
            );
            assert!(
                !entry.area.trim().is_empty(),
                "{} must belong to an area",
                entry.name
            );
            assert!(
                !seen.contains(&entry.name),
                "{} is listed twice",
                entry.name
            );
            seen.push(entry.name);

            for field in entry.payload_fields {
                assert!(!field.name.is_empty());
            }
        }
    }

    #[test]
    fn the_names_the_platform_emits_are_all_listed() {
        // The names this crate's own code records. The modules' emitters are checked by
        // `apps/api/tests/events.rs::every_emitted_name_is_in_the_catalogue`, which can see
        // the source tree; a unit test here cannot, and a test that pretends to is worse than
        // no test.
        for name in ["webhook.test", "page.published", "media.created"] {
            assert!(is_known(name), "{name} is emitted but not listed");
        }
    }

    #[test]
    fn the_registry_covers_every_module_that_emits() {
        // A floor rather than an exact count: the table grows, the test only fails when a
        // release removes coverage nobody meant to remove.
        assert!(
            CATALOGUE.len() >= 60,
            "the catalogue covers {} names; the brief asks for coverage across content, \
             media, identity, tenancy, plugins, themes, workflows and webhooks",
            CATALOGUE.len()
        );
    }

    #[test]
    fn a_reserved_name_names_the_module_that_ships_it() {
        // `Reserved` without a reason is how a row drifts back into looking live. Every
        // reserved name says which module owes it, so a reader of the panel can be told
        // "a module ships this" rather than "the platform is broken", and a developer can
        // find the owner without reading the git history.
        //
        // The list here is the claim, and `every_live_name_has_an_emitter` in
        // `apps/api/tests/events.rs` is what keeps it from becoming a lie: promoting one of
        // these to `Live` without adding the emitter turns that gate red with the name.
        let owed_by: &[(&str, &str)] = &[
            ("page.unpublished", "content"),
            ("translation.published", "content"),
            ("domain.verified", "tenancy"),
            ("plugin.installed", "plugins"),
            ("plugin.activated", "plugins"),
            ("plugin.deactivated", "plugins"),
            ("plugin.uninstalled", "plugins"),
            // Filed under `workflows`, owed by the automation layer: the engine in
            // `crates/workflows` starts the runs, but the bus event belongs to the workflow
            // area, and the picker groups on the area.
            ("workflow.run.started", "workflows"),
            ("workflow.run.completed", "workflows"),
            ("workflow.run.failed", "workflows"),
            ("order.created", "commerce"),
        ];

        for (name, module) in owed_by {
            let entry = lookup(name).unwrap_or_else(|| panic!("{name} must stay listed"));
            assert_eq!(
                entry.status,
                Status::Reserved,
                "{name} has no emitter, so it is not live; add the emission, then flip this \
                 row and the gate in apps/api/tests/events.rs will agree"
            );
            assert_eq!(
                entry.area, *module,
                "{name} is owed by {module}, not by the area it was filed under"
            );
            assert!(
                !entry.description.is_empty(),
                "{name} still has to say what a receiver would get"
            );
        }
    }

    #[test]
    fn a_reserved_name_is_listed_and_marked() {
        let order = lookup("order.created").expect("order.created is listed");
        assert_eq!(order.status, Status::Reserved);
        assert_eq!(order.area, "commerce");
        assert!(!order.description.is_empty());

        // Reserved is not the same as absent: a receiver may subscribe to it today.
        assert!(is_known("order.created"));
    }

    #[test]
    fn groups_are_recognised_and_expanded() {
        assert!(is_group("page.*"));
        assert!(!is_group("page.published"));
        assert!(!is_group(".*"));
        assert!(!is_group("Page.*"));
        assert_eq!(group_of("media.*"), Some("media"));
        assert_eq!(group_of("media.created"), None);

        let members = group_members("page");
        assert!(members.contains(&"page.published"));
        assert!(members.contains(&"page.deleted"));
        assert!(!members.contains(&"media.created"));
    }

    #[test]
    fn a_group_subscription_is_stored_expanded_and_keeps_the_wildcard() {
        let stored = reconcile(&["page.*".to_owned()]).expect("valid");

        assert!(stored.contains(&"page.*".to_owned()), "the wildcard stays");
        assert!(
            stored.contains(&"page.published".to_owned()),
            "today's members are stored too"
        );
        assert!(!stored.contains(&"media.created".to_owned()));
        assert!(
            stored.windows(2).all(|pair| pair[0] < pair[1]),
            "the stored list is sorted and deduplicated"
        );
    }

    #[test]
    fn a_wildcard_over_an_empty_group_is_kept_rather_than_refused() {
        // `payments` is a plausible area that the platform does not record yet. The
        // subscription is legitimate — it is a group whose module has not shipped — so it is
        // kept as written and becomes real when the names arrive.
        let stored = reconcile(&["payments.*".to_owned()]).expect("valid");
        assert_eq!(stored, vec!["payments.*".to_owned()]);

        // A group with only a reserved member still expands, because a reserved name is a
        // name: an endpoint that subscribes to `order.*` today must be ready for the day the
        // commerce module ships, without anybody editing it then.
        let commerce = reconcile(&["order.*".to_owned()]).expect("valid");
        assert_eq!(
            commerce,
            vec!["order.*".to_owned(), "order.created".to_owned()]
        );
    }

    #[test]
    fn an_area_is_not_a_group() {
        // `commerce` is the area; `order` is the group a receiver subscribes to. Conflating
        // them would make `commerce.*` silently match nothing while looking correct, and
        // would put a subscription on a group the emitters never use.
        let order = lookup("order.created").expect("listed");
        assert_eq!(order.area, "commerce");
        assert_eq!(order.group(), "order");

        assert!(
            group_members("commerce").is_empty(),
            "no event is emitted as commerce.*"
        );
        assert!(!group_members("order").is_empty());
        assert!(lookup("commerce.*").is_none());
    }

    #[test]
    fn reconciliation_deduplicates_a_group_against_its_members() {
        let stored = reconcile(&[
            "page.published".to_owned(),
            "page.*".to_owned(),
            "page.published".to_owned(),
        ])
        .expect("valid");

        assert_eq!(
            stored
                .iter()
                .filter(|name| *name == "page.published")
                .count(),
            1,
            "the same subscription twice is one subscription"
        );
        assert!(stored.contains(&"page.*".to_owned()));
    }

    #[test]
    fn a_group_covers_events_the_catalogue_has_not_heard_of_yet() {
        // The whole reason the wildcard is stored next to the expansion.
        let stored = reconcile(&["page.*".to_owned()]).expect("valid");
        assert!(subscribed_to(&stored, "page.something_added_next_release"));
        assert!(!subscribed_to(&stored, "media.created"));
    }

    #[test]
    fn an_exact_subscription_does_not_cover_its_siblings() {
        let stored = reconcile(&["page.published".to_owned()]).expect("valid");
        assert!(subscribed_to(&stored, "page.published"));
        assert!(!subscribed_to(&stored, "page.updated"));
    }

    #[test]
    fn an_empty_or_broken_subscription_is_refused() {
        assert!(reconcile(&[]).is_err());
        assert!(reconcile(&["   ".to_owned()]).is_err());
        assert!(
            reconcile(&["page".to_owned()]).is_err(),
            "a bare name is not a name"
        );
        assert!(
            reconcile(&["*".to_owned()]).is_err(),
            "an empty group is not a group"
        );
        assert!(
            reconcile(&["PAGE.*".to_owned()]).is_err(),
            "a group is lower-case"
        );
    }

    #[test]
    fn the_api_shape_of_a_field_is_declared() {
        let published = lookup("page.published").expect("listed");
        let revision = published
            .payload_fields
            .iter()
            .find(|field| field.name == "revision_no")
            .expect("page.published carries revision_no");
        assert_eq!(revision.kind, FieldKind::Integer);
        assert!(revision.required);
        assert_eq!(revision.kind.as_str(), "integer");
        assert_eq!(FieldKind::parse("nonsense"), FieldKind::Any);
    }

    #[test]
    fn areas_are_listed_once_in_table_order() {
        let areas = areas();
        assert_eq!(
            areas.len(),
            BTreeSet::from_iter(areas.iter().copied()).len()
        );
        assert!(areas.contains(&"content"));
        assert!(areas.contains(&"commerce"));
        assert!(
            in_area("content").len() > 1,
            "an area with one member would not be a group"
        );
    }

    #[test]
    fn the_ceiling_counts_what_the_operator_typed_not_what_it_expanded_to() {
        // Eight groups covering every member of the catalogue: forty-odd names once expanded,
        // eight selections as typed.
        let typed: Vec<String> = areas()
            .into_iter()
            .map(|area| format!("{area}.*"))
            .collect();
        assert!(
            typed.len() <= crate::validation::MAX_SUBSCRIPTIONS,
            "the table has more areas than the ceiling allows, so this test cannot say what it means"
        );

        let stored = reconcile(&typed).expect("eight groups are eight selections");
        assert!(
            stored.len() > typed.len(),
            "the groups expanded: {} entries for {} selections",
            stored.len(),
            typed.len()
        );

        // And the ceiling still bites when a person really does pick too much.
        let too_many: Vec<String> = (0..=crate::validation::MAX_SUBSCRIPTIONS)
            .map(|index| format!("custom.event_{index}"))
            .collect();
        assert!(reconcile(&too_many).is_err());
    }

    #[test]
    fn a_live_name_is_never_also_reserved() {
        let live = live_names();
        assert!(live.contains(&"page.published"));
        assert!(!live.contains(&"order.created"));
    }

    /// REQ-101's change-set names, asserted the way the tool-registry ones are.
    ///
    /// The drift test walks the sources for `NewEvent::new("…")` and fails on a name this table
    /// does not carry, so listing a name here is the *permission* to emit it. `ai.changeset.failed`
    /// is the one that matters: an all-or-nothing apply that rolled back writes nothing anywhere,
    /// so this name is the only signal a subscriber gets that a set was tried and undid itself.
    #[test]
    fn the_change_set_names_are_live_and_typed() {
        for (name, required_field, kind) in [
            ("ai.changeset.proposed", "change_set_id", FieldKind::Uuid),
            ("ai.changeset.confirmed", "change_set_id", FieldKind::Uuid),
            ("ai.changeset.discarded", "reason", FieldKind::String),
            ("ai.changeset.failed", "reason", FieldKind::String),
            ("ai.changeset.applied", "operations", FieldKind::Integer),
        ] {
            let entry = lookup(name).unwrap_or_else(|| panic!("{name} must be in the catalogue"));
            assert_eq!(
                entry.status,
                Status::Live,
                "{name} is emitted today, not reserved"
            );
            assert_eq!(entry.area, "ai", "{name} belongs to the AI area");
            let field = entry
                .payload_fields
                .iter()
                .find(|candidate| candidate.name == required_field)
                .unwrap_or_else(|| panic!("{name} must declare {required_field}"));
            assert!(field.required, "{name}.{required_field} is required");
            assert_eq!(field.kind, kind, "{name}.{required_field} has the wrong kind");
        }

        // `failed` and `applied` are the two halves of one promise, and a subscriber has to be
        // able to tell them apart: the same id, one that rolled back and one that committed.
        // If either were renamed to the other's spelling, this fails.
        let failed = lookup("ai.changeset.failed").expect("listed");
        let applied = lookup("ai.changeset.applied").expect("listed");
        assert!(
            !failed.payload_fields.iter().any(|f| f.name == "operations"),
            "`failed` has no operation count: it applied none"
        );
        assert!(
            applied.payload_fields.iter().any(|f| f.name == "operations"),
            "`applied` says how many operations committed"
        );
    }

    #[test]
    fn the_tool_registry_names_are_live_and_typed() {
        // REQ-100's event table, asserted rather than assumed. The drift test walks the sources
        // for `NewEvent::new("…")` and fails on a name the table does not carry, so a name added
        // here is the *permission* to emit it — this test is what makes that permission and the
        // payload shape agree.
        for (name, required_field, kind) in [
            ("ai.tool.registered", "tool_key", FieldKind::String),
            ("ai.tool.updated", "tool_key", FieldKind::String),
            ("ai.tool.disabled", "tool_key", FieldKind::String),
            ("ai.tool.grant_changed", "tool_key", FieldKind::String),
            ("ai.tool.denied", "tool_key", FieldKind::String),
            ("ai.tool.failed", "tool_key", FieldKind::String),
            ("ai.tool.limited", "tool_key", FieldKind::String),
            ("ai.identity.created", "key", FieldKind::String),
            ("ai.identity.updated", "key", FieldKind::String),
            ("ai.identity.removed", "key", FieldKind::String),
        ] {
            let entry = lookup(name).unwrap_or_else(|| panic!("{name} must be in the catalogue"));
            assert_eq!(
                entry.status,
                Status::Live,
                "{name} is emitted today, not reserved"
            );
            assert_eq!(entry.area, "ai", "{name} belongs to the AI area");
            let field = entry
                .payload_fields
                .iter()
                .find(|candidate| candidate.name == required_field)
                .unwrap_or_else(|| panic!("{name} must declare {required_field}"));
            assert!(field.required, "{name}.{required_field} is required");
            assert_eq!(
                field.kind, kind,
                "{name}.{required_field} has the wrong kind"
            );
        }
    }
    fn the_health_area_carries_the_five_names_the_request_names() {
        // REQ-014's Events section lists exactly these five, and the reason this test exists is
        // that for four of the REQ's slices the sentence "an operations endpoint subscribes to
        // degraded and recovered" had nothing behind it. A name that is emitted but not listed
        // is refused by the drift test; a name that is listed but never emitted is invisible to
        // every green gate in the workspace, so it needs an assertion of its own.
        let in_health = in_area("health");
        let health = names_of(&in_health);
        assert_eq!(
            health.len(),
            5,
            "the health area is {:?}; the request names five",
            health
        );
        for name in [
            "health.service.degraded",
            "health.service.recovered",
            "health.threshold.breached",
            "health.incident.acknowledged",
            "health.checks.completed",
        ] {
            let entry = lookup(name).unwrap_or_else(|| panic!("{name} must be listed"));
            assert_eq!(entry.area, "health");
            assert_eq!(entry.status, Status::Live, "{name} is emitted today");
            // All five share one group, so a single `health.*` subscription reaches all of them.
            // That is the subscription the request describes an operations endpoint making, and
            // it only works if every name sits behind the same first segment.
            assert_eq!(entry.group(), "health", "{name} is not behind health.*");
        }
        // The pair the request calls out, stated as a pair.
        assert!(subscribed_to(
            &["health.*".to_owned()],
            "health.service.degraded"
        ));
        assert!(subscribed_to(
            &["health.*".to_owned()],
            "health.service.recovered"
        ));
    }

    #[test]
    fn the_health_names_carry_the_fields_a_receiver_needs() {
        // What makes an operations subscription usable rather than decorative: a receiver that
        // gets `health.service.degraded` with only a name cannot decide anything, and the
        // required fields are the promise in the picker that it can.
        for (name, field) in [
            ("health.service.degraded", "service"),
            ("health.service.degraded", "to_state"),
            ("health.service.recovered", "service"),
            ("health.service.recovered", "from_state"),
            ("health.threshold.breached", "metric"),
            ("health.incident.acknowledged", "actor"),
            ("health.checks.completed", "state"),
        ] {
            let entry = lookup(name).expect("listed");
            let found = entry
                .payload_fields
                .iter()
                .find(|candidate| candidate.name == field)
                .unwrap_or_else(|| panic!("{name} must declare {field}"));
            assert!(found.required, "{name}.{field} is required in the test");
        }
    }

    #[test]
    fn a_denied_call_names_the_run_and_the_step_it_was_refused_in() {
        // `ai.tool.denied` is the alert hook. A denial with no `run_id` and no `reason` is not
        // actionable, so both must be *optional* (a denial outside a run is real — the execution
        // path also enforces a named tool) rather than required and wrong.
        let entry = lookup("ai.tool.denied").expect("listed");
        let required: Vec<&str> = entry.required_fields().map(|f| f.name).collect();
        assert_eq!(
            required,
            vec!["tool_key"],
            "a denial must always name the tool"
        );
        for optional in ["run_id", "step_id", "identity_id", "reason"] {
            assert!(
                entry
                    .payload_fields
                    .iter()
                    .any(|f| f.name == optional && !f.required),
                "ai.tool.denied must carry {optional} as optional"
            );
        }
    }

    #[test]
    fn a_grant_change_says_which_way_it_moved_and_who_moved_it() {
        let entry = lookup("ai.tool.grant_changed").expect("listed");
        // `effect` is required because the whole event is the direction of the change: a payload
        // that could carry "no change" would be an event nobody could alert on.
        assert!(
            entry
                .required_fields()
                .any(|f| f.name == "effect" && f.kind == FieldKind::Boolean)
        );
        assert!(
            entry
                .payload_fields
                .iter()
                .any(|f| f.name == "changed_by" && !f.required)
        );
    }

    /// REQ-106's air-gap names, asserted the way the change-set ones are.
    ///
    /// `ai.airgap.call_refused` is the one that matters: a switch that refuses calls silently is
    /// indistinguishable from a switch that is off, and the count of these events is the only
    /// answer to "is anything still leaving this machine?". Its `provider` and `host` are required
    /// for the same reason the refusal message names them — an event saying only "blocked" tells
    /// an operator to go looking in the wrong place.
    #[test]
    fn the_airgap_names_are_live_and_typed() {
        for (name, required_field, kind) in [
            ("ai.airgap.enabled", "reason", FieldKind::String),
            ("ai.airgap.call_refused", "provider", FieldKind::String),
        ] {
            let entry = lookup(name).unwrap_or_else(|| panic!("{name} must be listed"));
            assert_eq!(entry.status, Status::Live, "{name} is emitted today");
            let field = entry
                .payload_fields
                .iter()
                .find(|candidate| candidate.name == required_field)
                .unwrap_or_else(|| panic!("{name} must declare {required_field}"));
            assert!(field.required, "{name}.{required_field} is required");
            assert_eq!(field.kind, kind, "{name}.{required_field} has the wrong kind");
        }

        // The host is optional on the wire because a base URL the platform cannot parse has no
        // host — but it is declared, so a consumer reading the schema sees the field exists and
        // knows the null is meaningful rather than "the emitter forgot".
        let refused = lookup("ai.airgap.call_refused").expect("listed");
        assert!(
            refused.payload_fields.iter().any(|f| f.name == "host"),
            "call_refused declares host"
        );
    }

    #[test]
    fn required_fields_are_documented_where_they_are_claimed() {
        // Every required field of a live name the emitter shape is known for.
        for (name, field) in [
            ("page.published", "page_id"),
            ("page.published", "site_id"),
            ("page.published", "slug"),
            ("page.published", "revision_no"),
            ("user.created", "user_id"),
            ("site.created", "site_id"),
            ("media.created", "media_id"),
        ] {
            let entry = lookup(name).expect("listed");
            let found = entry
                .payload_fields
                .iter()
                .find(|candidate| candidate.name == field)
                .unwrap_or_else(|| panic!("{name} must declare {field}"));
            assert!(found.required, "{name}.{field} is required in the test");
            assert!(
                entry
                    .required_fields()
                    .any(|candidate| candidate.name == field),
                "{name}.{field} must be reachable through required_fields()"
            );
        }
    }

    #[test]
    fn the_event_name_for_an_entry_agrees_with_its_group() {
        for entry in CATALOGUE {
            let group = entry.group();
            assert!(
                entry.name.starts_with(&format!("{group}.")),
                "{} does not start with its own group",
                entry.name
            );
        }
    }

    #[test]
    fn names_of_helper_reads_correctly() {
        let content = in_area("content");
        assert!(names_of(&content).contains(&"page.published"));
    }
}
