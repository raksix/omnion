//! Errors of the AI Hub.

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
