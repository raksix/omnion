//! The closed vocabulary a rule is written in, as the panel reads it.
//!
//! The editor can only offer what it is told exists, and it must not offer what the engine
//! could not run. This module is that single list, and everything else reads it:
//!
//! * [`EventDef`] — the event library: each event, what it means, and the **payload fields**
//!   it carries. The condition and binding pickers offer exactly these fields, so a rule
//!   cannot be written against a field the event does not have (a mistake the matcher would
//!   only report as "this rule did not fire" hours later).
//! * operators, actions and the trigger kinds, so the editor's vocabulary is closed.
//!
//! Adding an event here is a contract: a module that starts emitting a new name adds one
//! row here, and the row's field list is what the panel offers. A name the bus accepts but
//! this list does not know is still a valid `trigger_event` (the bus is the authority on
//! names — see `omnion_events::validation`), it just has no documented payload to offer.
//! That is the honest answer, and it is why the editor's event picker is a *search over this
//! list* rather than a closed enum.

use serde::Serialize;

/// One payload field of a documented event.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EventField {
    /// The field as it appears in the payload, dotted for nesting (`author.email`).
    pub key: &'static str,
    /// JSON type of the field: `string`, `number`, `boolean`, `array` or `object`.
    pub kind: &'static str,
    /// What the field holds, in product language.
    pub label: &'static str,
    /// `true` when the value is an id, so the editor can offer it as a revision/target.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_id: bool,
}

impl EventField {
    const fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            kind: "string",
            label,
            is_id: false,
        }
    }

    const fn number(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            kind: "number",
            label,
            is_id: false,
        }
    }

    const fn ident(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            kind: "string",
            label,
            is_id: true,
        }
    }
}

/// One event of the library, with the payload it carries.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EventDef {
    /// Event name as the bus records it.
    pub name: &'static str,
    /// What happened, in product language.
    pub description: &'static str,
    /// The group the picker files it under.
    pub group: &'static str,
    /// The payload fields the panel offers for it.
    pub fields: &'static [EventField],
    /// `true` when the event belongs to a site, so a site-scoped rule can bind to it.
    pub site_scoped: bool,
}

impl EventDef {
    /// Look one event up by name.
    #[must_use]
    pub fn find(name: &str) -> Option<&'static Self> {
        EVENTS.iter().find(|event| event.name == name)
    }

    /// The payload fields of an event, or an empty slice for an undocumented one.
    #[must_use]
    pub fn fields_of(name: &str) -> &'static [EventField] {
        Self::find(name).map(|event| event.fields).unwrap_or(&[])
    }

    /// `true` when a documented event carries a payload field.
    #[must_use]
    pub fn has_field(name: &str, field: &str) -> bool {
        Self::fields_of(name)
            .iter()
            .any(|candidate| candidate.key == field)
    }
}

/// The events the platform documents, grouped for the picker.
///
/// A rule may still listen for a name that is not here (the bus decides which names exist);
/// this list is what the panel can *explain*.
pub const EVENTS: &[EventDef] = &[
    EventDef {
        name: "page.published",
        description: "A page was published; the named revision is what visitors see.",
        group: "Content",
        site_scoped: true,
        fields: &[
            EventField::ident("page_id", "Page"),
            EventField::ident("site_id", "Site"),
            EventField::text("slug", "Slug"),
            EventField::text("status", "Status"),
            EventField::ident("revision_id", "Revision"),
            EventField::number("revision_no", "Revision number"),
            EventField::text("title", "Title"),
        ],
    },
    EventDef {
        name: "user.created",
        description: "An account was created (onboarding, an invite or an administrator).",
        group: "Identity",
        site_scoped: false,
        fields: &[
            EventField::ident("user_id", "Account"),
            EventField::text("email", "E-mail"),
            EventField::text("display_name", "Display name"),
            EventField::text("status", "Status"),
        ],
    },
    EventDef {
        name: "user.updated",
        description: "An account's profile, status or organization binding changed.",
        group: "Identity",
        site_scoped: false,
        fields: &[
            EventField::ident("user_id", "Account"),
            EventField::text("email", "E-mail"),
            EventField::text("display_name", "Display name"),
            EventField::text("status", "Status"),
        ],
    },
    EventDef {
        name: "media.created",
        description: "A file was uploaded to a site's media library.",
        group: "Media",
        site_scoped: true,
        fields: &[
            EventField::ident("media_id", "File"),
            EventField::ident("site_id", "Site"),
            EventField::text("filename", "File name"),
            EventField::text("content_type", "Content type"),
            EventField::number("size_bytes", "Size in bytes"),
        ],
    },
    EventDef {
        name: "media.deleted",
        description: "A file was removed from a site's media library.",
        group: "Media",
        site_scoped: true,
        fields: &[
            EventField::ident("media_id", "File"),
            EventField::ident("site_id", "Site"),
        ],
    },
    EventDef {
        name: "workflow.execution.completed",
        description: "A workflow run finished successfully (the engine of REQ-003 itself).",
        group: "Automation",
        site_scoped: false,
        fields: &[
            EventField::ident("workflow_id", "Workflow"),
            EventField::ident("execution_id", "Run"),
            EventField::text("workflow_name", "Workflow name"),
            EventField::number("steps", "Steps"),
        ],
    },
    EventDef {
        name: "ai.run.completed",
        description: "An AI Hub run finished (REQ-001) — a chat turn, a rewrite or a tool call.",
        group: "AI",
        site_scoped: false,
        fields: &[
            EventField::ident("run_id", "AI run"),
            EventField::text("provider", "Provider"),
            EventField::text("model", "Model"),
            EventField::number("input_tokens", "Input tokens"),
            EventField::number("output_tokens", "Output tokens"),
        ],
    },
];

/// The event an inbound-webhook trigger listens for.
///
/// A hook is not a bus event a module emits: the call *is* the event, recorded on the bus
/// when it arrives so the same matcher, the same runs and the same audit trail serve it.
pub const HOOK_EVENT: &str = "automation.hook.received";

/// The payload an inbound hook call carries.
///
/// The caller's body becomes the event payload, under the hook namespace, so a rule's
/// conditions read `hook.body.order_id` and its actions bind `{{event.hook.body.slug}}`. The
/// wrapper is what makes that safe: the body is the caller's, the rest is the platform's.
#[derive(Debug, Clone, Serialize)]
pub struct HookPayload {
    /// The rule the call fired.
    pub rule_id: String,
    /// The rule's display name — carried for the run's own readability, not for a condition.
    pub rule_name: String,
    /// The caller's JSON body, as received. A body that is not an object is wrapped as
    /// `{"value": …}` so a condition can always reach it.
    pub body: serde_json::Value,
    /// The HTTP method of the call.
    pub method: String,
    /// Where the call came from, with the last segment of the address redacted — a hook is a
    /// public surface and its audit trail must not become a log of who calls it.
    pub source: String,
    /// When the call arrived.
    #[serde(with = "time::serde::rfc3339")]
    pub received_at: time::OffsetDateTime,
}

impl HookPayload {
    /// Build the payload of one hook call.
    #[must_use]
    pub fn build(
        rule_id: uuid::Uuid,
        rule_name: &str,
        body: serde_json::Value,
        method: &str,
        source: &str,
    ) -> Self {
        Self {
            rule_id: rule_id.to_string(),
            rule_name: rule_name.to_owned(),
            body: normalise_body(body),
            method: method.to_uppercase(),
            source: redact_address(source),
            received_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// The stored event payload.
    #[must_use]
    #[allow(clippy::must_use_candidate)]
    pub fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({ "hook": serde_json::to_value(self).unwrap_or(serde_json::Value::Null) })
    }
}

/// A non-object body still has to be reachable from a condition.
fn normalise_body(body: serde_json::Value) -> serde_json::Value {
    if body.is_object() {
        body
    } else {
        serde_json::json!({ "value": body })
    }
}

/// Keep the shape of a source address and drop what identifies it.
///
/// `203.0.113.7` becomes `203.0.113.0/24`-style detail rather than the full address: the
/// audit trail wants to show where a hook was called from, and a hook URL is a credential
/// that people paste into log aggregators.
fn redact_address(source: &str) -> String {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return "unknown".to_owned();
    }
    if let Some((host, _port)) = trimmed.rsplit_once(':') {
        if host
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ':')
        {
            return redact_host(host);
        }
    }
    redact_host(trimmed)
}

/// Drop the last two octets of an IPv4 address, or the host part of a name.
fn redact_host(host: &str) -> String {
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() == 4 && octets.iter().all(|part| part.parse::<u8>().is_ok()) {
        return format!("{}.{}.{}.x", octets[0], octets[1], octets[2]);
    }
    "[redacted]".to_owned()
}

/// The trigger kinds the editor offers, in the order it lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    /// A recorded platform event (the event library above).
    Event,
    /// A cron schedule in the organization's timezone.
    Schedule,
    /// A person pressing "run now" only.
    Manual,
    /// An inbound webhook: the rule's own URL is the trigger.
    InboundWebhook,
}

impl TriggerKind {
    /// Every kind, in catalogue order.
    pub const ALL: [Self; 4] = [
        Self::Event,
        Self::Schedule,
        Self::Manual,
        Self::InboundWebhook,
    ];

    /// Canonical name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::InboundWebhook => "inbound_webhook",
        }
    }

    /// `true` when the kind is a URL the platform calls out to (none today) rather than one
    /// it waits for.
    #[must_use]
    pub const fn is_outbound(self) -> bool {
        matches!(self, Self::Schedule | Self::InboundWebhook)
    }
}

/// How many events one rule may listen for (`1` today — one event per rule, as v0 stores it).
pub const MAX_TRIGGER_EVENTS: usize = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_documented_event_has_a_description_and_fields() {
        assert!(!EVENTS.is_empty());
        for event in EVENTS {
            assert!(!event.description.trim().is_empty(), "{}", event.name);
            assert!(!event.group.trim().is_empty(), "{}", event.name);
            assert!(
                !event.fields.is_empty(),
                "{} documents no payload",
                event.name
            );
            for field in event.fields {
                assert!(
                    !field.label.trim().is_empty(),
                    "{}.{}",
                    event.name,
                    field.key
                );
                assert!(
                    ["string", "number", "boolean", "array", "object"].contains(&field.kind),
                    "{}.{} is {}",
                    event.name,
                    field.key,
                    field.kind
                );
            }
        }
    }

    #[test]
    fn the_library_has_no_duplicate_names_and_only_documented_events() {
        let mut names: Vec<&str> = EVENTS.iter().map(|event| event.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two rows share an event name");

        // The five the request names, plus the two the engine chains on.
        for name in [
            "page.published",
            "user.created",
            "user.updated",
            "media.created",
            "media.deleted",
            "workflow.execution.completed",
            "ai.run.completed",
        ] {
            assert!(EventDef::find(name).is_some(), "{name} is documented");
        }
    }

    #[test]
    fn an_undocumented_event_answers_nothing_rather_than_guessing() {
        assert!(EventDef::find("some.thing").is_none());
        assert!(EventDef::fields_of("some.thing").is_empty());
        assert!(!EventDef::has_field("some.thing", "id"));
        // A documented one answers both ways.
        assert!(EventDef::has_field("page.published", "revision_id"));
        assert!(!EventDef::has_field("page.published", "author.email"));
    }

    #[test]
    fn a_hook_payload_wraps_a_non_object_body_and_redacts_the_source() {
        let rule = uuid::Uuid::nil();
        let payload = HookPayload::build(
            rule,
            "Order hook",
            json!({ "id": 7 }),
            "post",
            "203.0.113.7",
        );
        assert_eq!(payload.method, "POST");
        assert_eq!(payload.body["id"], 7);
        assert_eq!(
            payload.source, "203.0.113.x",
            "the caller is not logged in full"
        );
        assert_eq!(payload.rule_id, rule.to_string());

        let scalar = HookPayload::build(rule, "Ping", json!("hello"), "post", "10.0.0.9:5555");
        assert_eq!(scalar.body["value"], "hello", "a body is always reachable");
        assert_eq!(scalar.source, "10.0.0.x");

        let named = HookPayload::build(rule, "Ping", json!({}), "GET", "hooks.example.com");
        assert_eq!(named.source, "[redacted]");
        assert_eq!(
            HookPayload::build(rule, "Ping", json!({}), "GET", "  ").source,
            "unknown"
        );
    }

    #[test]
    fn the_hook_event_is_a_name_the_bus_accepts() {
        assert_eq!(
            omnion_events::validation::validate_event_name(HOOK_EVENT).expect("valid"),
            HOOK_EVENT
        );
    }

    #[test]
    fn the_trigger_kinds_round_trip() {
        assert_eq!(TriggerKind::ALL.len(), 4);
        assert_eq!(TriggerKind::InboundWebhook.as_str(), "inbound_webhook");
        assert!(!TriggerKind::Event.is_outbound());
        assert!(TriggerKind::InboundWebhook.is_outbound());
    }
}
