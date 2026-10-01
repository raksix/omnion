//! Errors of the AI Hub.

/// `Uuid` is needed for [`AiHubError::IdentityNotFound`], which names the id the caller asked
/// for. The message has to carry it: "no identity in this organization" tells an operator
/// nothing when they were editing one they can see, while the id points straight at the row.
use uuid::Uuid;

/// What can go wrong while the AI Hub talks to a provider — or while the platform reads its own
/// rows.
///
/// The variants are the taxonomy the API surface maps onto HTTP: a provider that was never
/// connected (`ProviderNotFound`), one the operator switched off (`ProviderDisabled`), a model
/// the registry does not carry (`ModelNotFound`), an installation without a default model
/// (`NoDefaultModel`), a request the platform (or the provider) refuses before the model sees it
/// (`Invalid*`), and the two ways a provider can fail at the far end — it did not answer at all
/// (`Transport`) or it answered with a refusal (`Upstream`).
#[derive(Debug, thiserror::Error)]
pub enum AiHubError {
    /// The database refused the read or the write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// No provider with that id.
    #[error("no AI provider carries that id")]
    ProviderNotFound,
    /// Another provider already uses the name.
    #[error("an AI provider named \"{0}\" already exists")]
    ProviderNameTaken(String),
    /// No model with that id, or no model with that key on that provider.
    #[error("no AI model matches that request")]
    ModelNotFound,
    /// The installation has no (enabled) default model to route to.
    #[error("no default AI model is configured")]
    NoDefaultModel,
    /// No route decision carries that id.
    ///
    /// A 404 rather than an empty answer: the log screen opens a decision by id, and a row that
    /// is not there is a stale bookmark, not an installation whose log happens to be empty.
    #[error("no route decision with that id")]
    DecisionNotFound,
    /// The resolved provider is switched off.
    #[error("the AI provider \"{0}\" is disabled")]
    ProviderDisabled(String),
    /// The provider the installation points at cannot be removed.
    ///
    /// Removing the default would leave every task-routed request with nowhere to go, and the
    /// repair the store does afterwards promotes an arbitrary row the operator never chose. The
    /// refusal names the provider so the message can tell them which one to move first.
    #[error(
        "the AI provider \"{0}\" is the installation default; set another default before removing it"
    )]
    ProviderIsDefault(String),
    /// The provider definition is unusable.
    #[error("invalid provider: {0}")]
    InvalidProvider(String),
    /// The model definition (or the model selection) is unusable.
    #[error("invalid model: {0}")]
    InvalidModel(String),
    /// The agent definition is unusable.
    ///
    /// A separate code from `invalid_model` because the fix is in a different place: an
    /// `invalid_model` sends an operator to the registry, an `invalid_agent` sends them to the
    /// agent form, and a client that collapses the two shows the wrong field's message under
    /// this one.
    #[error("invalid agent: {0}")]
    InvalidAgent(String),
    /// The run cannot be started, continued or finished as asked.
    ///
    /// Carries the reason a resume was refused and the limit a goal broke, so the API can put
    /// the text under the field that caused it rather than answering with a generic 400.
    #[error("invalid run: {0}")]
    InvalidRun(String),
    /// A workspace path or an upload breaks one of the workspace's own rules (REQ-099 slice 2).
    ///
    /// Its own code, not `InvalidAgent` or `InvalidRun`, because the fix lands in the Workspace
    /// tab and a client that maps it onto the agent form shows a path refusal above the name
    /// field — the one place the reader is not looking when the file picker refused a file.
    /// The message always carries the limit that was broken, so the panel can print it.
    #[error("invalid workspace file: {0}")]
    InvalidFile(String),
    /// A skill definition or an attachment breaks one of the registry's rules (REQ-099 slice 3).
    ///
    /// Its own code, for the same reason `InvalidFile` has one: the refusal belongs under the
    /// field that caused it on the Skills form or the Skills tab, and a client that maps it
    /// onto `invalid_agent` prints "the agent is invalid" above a row that is about a skill.
    /// The message names the offending key, because the spec's own acceptance criterion is
    /// that a skill attaching an unknown tool fails "with the key named".
    #[error("invalid skill: {0}")]
    InvalidSkill(String),
    /// No skill carries that key, for this organization.
    ///
    /// The "for this organization" half is the point: a built-in has no organization, and a
    /// custom key in another tenant must be indistinguishable from a key nobody wrote.
    #[error("no skill `{0}` in this registry")]
    SkillNotFound(String),
    /// The skill is a built-in and cannot be rewritten.
    ///
    /// Separate from `InvalidSkill` because the answer is not "your request was malformed" —
    /// it is "this row is not yours to change", which is a different thing for a client to
    /// render and for the panel to keep offering an Edit button for.
    #[error("{0}")]
    SkillReadOnly(String),
    /// The skill key is taken, or the attachment already exists.
    #[error("{0}")]
    SkillConflict(String),
    /// A tool limit, key or argument set is not acceptable (REQ-100).
    ///
    /// Its own code, for the same reason `InvalidSkill` has one: the refusal belongs on a
    /// specific field of the tool's Limits section, and a client that maps it onto `invalid_tool`
    /// prints "the tool is invalid" above a timeout field. The message names the field and the
    /// range, because the ranges are the request's (1000–300000 ms, 1–200 calls) and an
    /// operator who is told the range can fix it without reading this crate.
    #[error("invalid tool: {0}")]
    InvalidTool(String),
    /// No tool carries that key in the registry.
    ///
    /// `NotFound` rather than a 403-style refusal, including for a retired row's key: the
    /// detail screen has to be able to open a retired tool and show why it retired, so a key
    /// that exists must be found, and a key that never existed must not be.
    #[error("no tool `{0}` in the registry")]
    ToolNotFound(String),
    /// An AI identity's key, name, description or grant map is not acceptable (REQ-100).
    ///
    /// Its own code, for the same reason `InvalidTool` has one: the refusal belongs on a
    /// specific field of the identity form or of one matrix cell, and a client that maps it
    /// onto `invalid_tool` would tell an operator their *identity* is invalid when the thing
    /// that is invalid is a key they typed. Every message names the field and its rule.
    #[error("invalid identity: {0}")]
    InvalidIdentity(String),
    /// An approval request, a decision or a class policy breaks one of the gate's own rules
    /// (REQ-101).
    ///
    /// Its own code, for the same reason `InvalidIdentity` has one: the refusal belongs on a
    /// *field* of the approval form — a class nobody has heard of, an expiry of zero, a
    /// rejection with no reason — and a client that maps it onto `invalid_tool` or
    /// `invalid_run` would print the wrong sentence above the wrong input. Every message names
    /// the field and the rule it broke.
    #[error("invalid approval: {0}")]
    InvalidApproval(String),
    /// No approval carries that id in this organization.
    ///
    /// `NotFound`, never a 403, and that is a tenancy property rather than a preference: an
    /// approval id that exists in another tenant must not be confirmable by its status code, or
    /// the inbox becomes an existence oracle for every approval in the installation — including
    /// the ones that name a page somebody is about to delete.
    #[error("no approval `{0}` in this organization")]
    ApprovalNotFound(Uuid),
    /// A change set breaks one of its own rules (REQ-101 slice 3).
    ///
    /// Its own code, not `InvalidApproval`, because the refusal belongs on a different screen:
    /// an approval's problem is a field of the **review** form, while a change set's problem is
    /// an operation in an **editable list** — "operation 3 is a create that names a target id".
    /// A client that mapped this onto `invalid_approval` would print the sentence above the
    /// title field, which is the one control the reviewer is not looking at when a row of the
    /// list is refused. Every message names the operation it is about.
    #[error("invalid change set: {0}")]
    InvalidChangeSet(String),
    /// No change set carries that id in this organization.
    ///
    /// `NotFound` for the same tenancy reason as [`Self::ApprovalNotFound`]: a set id that
    /// exists in another tenant must not be distinguishable from one that does not exist.
    #[error("no change set `{0}` in this organization")]
    ChangeSetNotFound(Uuid),
    /// No identity carries that id in this organization.
    ///
    /// `NotFound` rather than a 403, and that is a tenancy property rather than a preference:
    /// an id that exists in another tenant must not be confirmable by the status code, or the
    /// identity list becomes an existence oracle for the whole installation's AI permissions.
    #[error("no identity `{0}` in this organization")]
    IdentityNotFound(Uuid),
    /// An identity with that key already exists in this scope.
    ///
    /// A `409`, not a `400`, because the request is valid and the *state* is what refuses it —
    /// the folded unique index is doing its job, and the panel resolves it by offering a
    /// different key rather than by rewriting the user's input.
    #[error("{0}")]
    IdentityConflict(String),
    /// The request needs a capability the model does not claim.
    ///
    /// The refusal happens before any call leaves the process, so a caller that asked for a
    /// stream from a model that cannot stream gets this instead of a provider error 20 seconds
    /// later — and the message names both the model and the capability, so the fix is a flag
    /// edit rather than a search.
    #[error("the model \"{model}\" does not support {capability}")]
    CapabilityUnsupported {
        /// Wire key of the model that was asked.
        model: String,
        /// Wire name of the capability it does not claim.
        capability: &'static str,
    },
    /// The chat request itself is unusable.
    #[error("invalid chat request: {0}")]
    InvalidChatRequest(String),
    /// The provider could not be reached.
    #[error("the AI provider did not answer: {0}")]
    Transport(String),
    /// A guard rule breaks one of the guard's own rules (REQ-105).
    ///
    /// Its own code, for the same reason `InvalidTool` and `InvalidApproval` have one: the
    /// refusal belongs on a *field* of the rule form — a key that is not `[a-z0-9_.-]{2,60}`,
    /// an expression that does not compile, a priority outside 1–999 — and a client that
    /// mapped this onto `invalid_tool` would print "the tool is invalid" above the pattern
    /// input. Every message names the field and the rule it broke.
    ///
    /// A **compiled** pattern is what makes this variant exist at all: an invalid expression
    /// is a field error at save time and can never reach a request, so there is no second
    /// error shape for a bad rule.
    #[error("invalid guard rule: {0}")]
    InvalidGuardRule(String),
    /// No guard rule carries that id in this organization.
    ///
    /// `NotFound`, never a 403, for the same tenancy reason as [`Self::ApprovalNotFound`]: a
    /// rule id that exists in another tenant must not be distinguishable by status code, or
    /// the rules screen becomes an existence oracle for the whole installation's detection
    /// rules — which is a map of the platform's own knowledge about what its tenants' data
    /// looks like.
    #[error("no guard rule `{0}` in this organization")]
    GuardRuleNotFound(Uuid),
    /// A guard rule with that key already exists in this scope.
    ///
    /// A `409` rather than a `400`: the request is well-formed and the *state* refuses it —
    /// the folded unique index did its job, and the panel resolves it by offering a different
    /// key instead of rewriting what the operator typed.
    #[error("{0}")]
    GuardRuleConflict(String),
    /// An exemption breaks one of its own rules (REQ-105).
    ///
    /// Its own code rather than `InvalidGuardRule`, because the refusal belongs on a different
    /// form: an exemption has a **reason** and an **expiry**, and "an exemption needs a reason"
    /// printed above a regex input would be the wrong sentence above the wrong control.
    #[error("invalid guard exemption: {0}")]
    InvalidGuardExemption(String),
    /// The guard refused to build: too many enabled rules (REQ-105).
    ///
    /// A configuration error surfaced on the request that hit it, and its own code because
    /// the fix is on the rules screen rather than in the payload. The request is explicit that
    /// this "refuses to start in the guard (the API answers a configuration error) rather than
    /// slowing every call" — which is why it is not a 400: nothing about the caller's payload
    /// is wrong.
    #[error("{0}")]
    GuardConfiguration(String),
    /// The provider answered, but refused the request.
    #[error("the AI provider answered with status {status}: {message}")]
    Upstream {
        /// HTTP status the provider answered with.
        status: u16,
        /// Provider message, trimmed to a sane length.
        message: String,
    },
    /// The answer stream broke halfway through.
    #[error("the AI provider's answer stream failed: {0}")]
    Stream(String),
    /// The provider answered with something the platform cannot read.
    #[error("the AI provider answered with an unusable body: {0}")]
    Malformed(String),
}

impl AiHubError {
    /// `true` when the failure happened at the provider, not inside Omnion.
    #[must_use]
    pub fn is_upstream(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Upstream { .. } | Self::Stream(_) | Self::Malformed(_)
        )
    }

    /// Stable, machine-readable code for this failure.
    ///
    /// The API puts it in its error bodies and in the `error` frame of a chat stream, so a
    /// client can branch on the reason without reading prose.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Database(_) => "internal_error",
            Self::ProviderNotFound => "provider_not_found",
            Self::ProviderNameTaken(_) => "provider_name_taken",
            Self::ModelNotFound => "model_not_found",
            Self::NoDefaultModel => "no_default_model",
            Self::DecisionNotFound => "decision_not_found",
            Self::ProviderDisabled(_) => "provider_disabled",
            Self::ProviderIsDefault(_) => "provider_is_default",
            Self::InvalidProvider(_) => "invalid_provider",
            Self::InvalidModel(_) => "invalid_model",
            Self::InvalidAgent(_) => "invalid_agent",
            Self::InvalidRun(_) => "invalid_run",
            Self::InvalidFile(_) => "invalid_file",
            Self::InvalidSkill(_) => "invalid_skill",
            Self::SkillNotFound(_) => "skill_not_found",
            Self::SkillReadOnly(_) => "skill_read_only",
            Self::SkillConflict(_) => "skill_conflict",
            Self::InvalidTool(_) => "invalid_tool",
            Self::ToolNotFound(_) => "tool_not_found",
            // The identity codes (REQ-100 slice 2). Distinct from `invalid_tool` on purpose: the
            // refusal belongs on a field of the identity form or of one matrix cell, and a client
            // that mapped it onto the tool's code would print "the tool is invalid" above a
            // permission cell.
            Self::InvalidIdentity(_) => "invalid_identity",
            Self::InvalidApproval(_) => "invalid_approval",
            Self::ApprovalNotFound(_) => "approval_not_found",
            // The change-set codes (REQ-101 slice 3). Distinct from the approval pair on
            // purpose: the refusal belongs on a row of the editor's list, not on a field of
            // the review form, and a client that collapsed them would print the sentence
            // above the title.
            Self::InvalidChangeSet(_) => "invalid_change_set",
            Self::ChangeSetNotFound(_) => "change_set_not_found",
            Self::IdentityNotFound(_) => "identity_not_found",
            Self::IdentityConflict(_) => "identity_conflict",
            // The guard codes (REQ-105). Four distinct shapes, not one, because the four answers
            // land on four different screens: a bad field is a `400` naming the field, a missing
            // id is a `404` (never a 403 — a rule id that exists must not be confirmable by its
            // status code), a taken key is a `409` the panel resolves by offering another, and
            // the rule budget is a configuration error with no field to blame.
            Self::InvalidGuardRule(_) => "invalid_guard_rule",
            Self::GuardRuleNotFound(_) => "guard_rule_not_found",
            Self::GuardRuleConflict(_) => "guard_rule_conflict",
            Self::InvalidGuardExemption(_) => "invalid_guard_exemption",
            Self::GuardConfiguration(_) => "guard_configuration",
            Self::CapabilityUnsupported { .. } => "capability_unsupported",
            Self::InvalidChatRequest(_) => "invalid_chat_request",
            Self::Transport(_) => "provider_unreachable",
            Self::Upstream { .. } => "provider_error",
            Self::Stream(_) => "stream_failed",
            Self::Malformed(_) => "provider_malformed",
        }
    }
}

/// Result alias of the AI Hub.
pub type Result<T> = std::result::Result<T, AiHubError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_failures_are_recognised() {
        assert!(AiHubError::Transport("connect refused".to_owned()).is_upstream());
        assert!(
            AiHubError::Upstream {
                status: 429,
                message: "slow down".to_owned()
            }
            .is_upstream()
        );
        assert!(AiHubError::Stream("half a frame".to_owned()).is_upstream());
        assert!(!AiHubError::ProviderNotFound.is_upstream());
        assert!(!AiHubError::NoDefaultModel.is_upstream());
    }

    #[test]
    fn messages_carry_the_provider_detail() {
        let error = AiHubError::Upstream {
            status: 401,
            message: "bad key".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "the AI provider answered with status 401: bad key"
        );
    }
}
