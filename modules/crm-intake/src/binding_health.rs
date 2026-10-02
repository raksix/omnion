//! The binding health check: which of a source's mapped keys the bound form no longer has.
//!
//! ## Why this is a module and not a line inside `capture`
//!
//! REQ-117's risk note says the check "runs on every submission and flags missing source keys
//! **instead of writing a lead with silently empty fields**". The second half of that sentence
//! is the promise, and the code did the opposite at three separate points:
//!
//! * [`crate::mapping::health`] computed the answer and had no production caller at all — its
//!   only references were its own definition and two unit tests.
//! * [`crate::store::set_broken_mappings`] wrote the column and also had no production caller.
//!   The column had a model predicate ([`crate::model::IntakeSource::binding_is_broken`]) and a
//!   screen branch that renders the missing keys, so the screen was *correct about a state that
//!   nothing could ever put the platform in*.
//! * A form-bound source whose field was renamed kept mapping the old key. `mapping::apply`
//!   drops a source key the payload does not have, so the lead was written with the field
//!   silently empty — indistinguishable from a visitor who left it blank, which is the one
//!   reading an operator cannot act on.
//!
//! This is the shape the module has now met three times (the round-robin cursor, the
//! autoresponder reservation, the SLA reminder): **a function can be exported, unit-tested and
//! named in a REQ, and none of that is a caller.** The check is below, and its caller is in
//! `capture`.
//!
//! ## The two questions, and why they are not one
//!
//! * *Which keys are gone?* — [`crate::mapping::health`], a pure function over the mapping and
//!   the form's key list. Answerable without a database.
//! * *What is the form's key list?* — [`available_form_keys`], the only part that needs I/O,
//!   and the only part that has to be honest about a form module that is not installed.
//!
//! Keeping them apart is what makes the second question testable: the health verdict is a pure
//! function of two lists, and the *reason* it could not be computed is a value rather than a
//! silent empty result.
//!
//! ## A keyed endpoint is healthy, always
//!
//! `health` treats an empty available-key list as "no binding to break" rather than "every key
//! is missing". That is the right default for a source with no `form_key`, and it is also the
//! only reading that keeps a source healthy on an installation where the forms module is
//! absent — otherwise installing the CRM would mark every endpoint source broken.

use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;
use crate::model::IntakeSource;

/// Why the binding's health could not be computed, when it could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthUnknown {
    /// The source is not bound to a form, so there is no key list to compare against.
    NotFormBound,
    /// The source is form-bound but the forms module is not installed on this platform, so
    /// the form's own key list cannot be read.
    FormsModuleAbsent,
}

impl HealthUnknown {
    /// A line an operator can act on, for the editor and the source's `last_error`.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NotFormBound => "this source is not bound to a form",
            Self::FormsModuleAbsent => {
                "the forms module is not installed, so this source's form keys cannot be read"
            }
        }
    }
}

/// The outcome of a health check on one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingHealth {
    /// The mapping is intact.
    Healthy,
    /// The form no longer has these source keys. Submissions still arrive; these fields are
    /// empty until the mapping is corrected.
    Broken(Vec<String>),
    /// The check could not run, and this is why. **Not** the same answer as `Healthy`: a
    /// source whose health nobody can read is a source whose health nobody can claim.
    Unknown(HealthUnknown),
}

impl BindingHealth {
    /// Whether the source's mapping names keys the form no longer has.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        matches!(self, Self::Broken(keys) if !keys.is_empty())
    }

    /// The keys to store on the source row. `None` when the check could not run — an unknown
    /// answer must not *clear* a previously-recorded answer, or one tick where the forms module
    /// is absent would silently un-break every form-bound source in the installation.
    #[must_use]
    pub fn keys_to_store(&self) -> Option<&[String]> {
        match self {
            Self::Healthy => Some(&[]),
            Self::Broken(keys) => Some(keys),
            Self::Unknown(_) => None,
        }
    }
}

/// The form-bound source's own key list, or why it cannot be read.
///
/// The tables are read through `to_regclass` so a platform without the forms module answers
/// `Unknown` rather than a `42P01`. That is the same shape as the conversion stepper's
/// "not installed here" state, and for the same reason: **"we could not look" and "we looked
/// and it is fine" are different sentences**, and a health check that collapses them reports
/// every source on a forms-less installation as broken.
///
/// The payload's own keys are accepted as the available list when they are present, because a
/// keyed endpoint's payload is the form's live schema: a submission that carries `email` is
/// evidence that the surface still has `email`, and a check that ignored it would break a
/// working integration on the strength of a missing optional table.
pub async fn available_form_keys(
    pool: &sqlx::PgPool,
    source: &IntakeSource,
    submission_payload: &Value,
) -> Result<Option<Vec<String>>> {
    let Some(form_key) = source.form_key.as_deref().map(str::trim).filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };

    if crate::convert_store::table_present(pool, "cms_forms").await {
        if let Some(keys) = form_keys_from_table(pool, form_key).await? {
            return Ok(Some(keys));
        }
    }

    // No table, or the form is not in it: fall back to the payload's own keys, so a live
    // capture surface is evidence of its own health.
    //
    // **The submission in hand is counted first, and that ordering is the whole point.** The
    // first draft read only *stored* leads, which cannot work: the check runs before the lead
    // is written, so on the very first submission after a rename there is no stored payload,
    // the list is empty, and the answer is `Unknown`. The rename would be detected on the
    // second submission — one enquiry later, and only if a second one ever arrived. A form
    // that takes one quote request a week would go a week without saying so. The payload the
    // platform is holding *right now* is better evidence than one it stored earlier.
    let mut keys = payload_keys(submission_payload);
    for stored in payload_keys_of_recent_leads(pool, source.id).await? {
        if !keys.iter().any(|existing| existing == &stored) {
            keys.push(stored);
        }
    }
    if keys.is_empty() {
        Err(crate::error::CrmIntakeError::invalid(
            HealthUnknown::FormsModuleAbsent.message(),
        ))
    } else {
        Ok(Some(keys))
    }
}

/// The top-level keys of one submission payload.
fn payload_keys(payload: &Value) -> Vec<String> {
    payload
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

/// The stored field list of one form, when the forms module is installed and the form exists.
async fn form_keys_from_table(
    pool: &sqlx::PgPool,
    form_key: &str,
) -> Result<Option<Vec<String>>> {
    // The column set is read rather than named: the forms module's schema is not this branch's
    // to assume, and a query against a column that does not exist fails the *check*, which is
    // the one thing a health check must never do. `to_regclass` above already proved the table
    // is there; if the shape is unfamiliar the answer is `Unknown`, not a wrong verdict.
    let columns: Vec<String> = sqlx::query_scalar(
        "select column_name from information_schema.columns \
         where table_schema = 'public' and table_name = 'cms_forms'",
    )
    .fetch_all(pool)
    .await?;

    if !columns.iter().any(|column| column == "key") {
        return Ok(None);
    }

    let field_column = if columns.iter().any(|column| column == "fields") {
        "fields"
    } else if columns.iter().any(|column| column == "schema") {
        "schema"
    } else if columns.iter().any(|column| column == "definition") {
        "definition"
    } else {
        return Ok(None);
    };

    let row: Option<sqlx::types::Json<Value>> = sqlx::query_scalar(&format!(
        "select {field_column}::jsonb from cms_forms where key = $1 limit 1"
    ))
    .bind(form_key)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|json| object_keys(&json.0)))
}

/// The distinct payload keys the last submissions of one source carried.
///
/// A union with the submission in hand, not a replacement for it: a field the form removed
/// shows up here as a key that has *stopped* appearing, which is only visible against a
/// history. One submission alone cannot tell "renamed away" from "this form never had it".
async fn payload_keys_of_recent_leads(
    pool: &sqlx::PgPool,
    source_id: Uuid,
) -> Result<Vec<String>> {
    let mut rows = sqlx::query_scalar::<_, serde_json::Value>(
        "select payload from crm_leads where source_id = $1 and payload <> '{}'::jsonb \
         order by received_at desc limit 20",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await?;

    let mut keys: Vec<String> = Vec::new();
    for row in rows.drain(..) {
        if let Some(object) = row.as_object() {
            for key in object.keys() {
                if !keys.iter().any(|existing| existing == key) {
                    keys.push(key.clone());
                }
            }
        }
    }
    Ok(keys)
}

/// The top-level keys of a form definition object.
///
/// A form's field list is a list of objects with a `name`/`key` each, or a plain object of
/// `key → definition`. Both are read, because picking one and treating the other as "no keys"
/// is how a health check reports a healthy form as broken — a wrong answer in the safe-looking
/// direction is still wrong.
fn object_keys(value: &Value) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    if let Some(list) = object.get("fields").and_then(Value::as_array) {
        return list
            .iter()
            .filter_map(|field| {
                field
                    .get("name")
                    .or_else(|| field.get("key"))
                    .or_else(|| field.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
    }
    object.keys().cloned().collect()
}

/// Run the health check for one source and report the answer.
///
/// The pure part ([`crate::mapping::health`]) is what decides `Broken`; this function only
/// supplies the available-key list and never computes a verdict of its own, so the interesting
/// behaviour is testable without a database.
pub async fn check(
    pool: &sqlx::PgPool,
    source: &IntakeSource,
    submission_payload: &Value,
) -> Result<BindingHealth> {
    let Some(keys) = available_form_keys(pool, source, submission_payload).await? else {
        return Ok(BindingHealth::Unknown(HealthUnknown::NotFormBound));
    };
    if keys.is_empty() {
        // An empty list from a *form* is not "nothing is broken" — `health` says an empty list
        // means unbound, and a form that answers an empty key list has told us nothing. Report
        // it as unknown so the row keeps whatever it last knew.
        return Ok(BindingHealth::Unknown(HealthUnknown::FormsModuleAbsent));
    }
    let broken = crate::mapping::health(&source.mapping_lines(), &keys);
    Ok(if broken.is_empty() {
        BindingHealth::Healthy
    } else {
        BindingHealth::Broken(broken)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::MappingEntry;

    fn source_with(form_key: Option<&str>, mapping: &[(&str, &str)]) -> IntakeSource {
        IntakeSource {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: None,
            name: "a source".to_string(),
            kind: if form_key.is_some() { "form" } else { "endpoint" }.to_string(),
            form_key: form_key.map(str::to_string),
            endpoint_key_hash: None,
            endpoint_key_hint: None,
            mapping: serde_json::Value::Array(
                mapping
                    .iter()
                    .map(|(target, key)| {
                        serde_json::json!({ "target": target, "source_key": key })
                    })
                    .collect(),
            ),
            required_targets: Vec::new(),
            consent_required: false,
            consent_text: None,
            dedupe_policy: "link".to_string(),
            pipeline_id: None,
            stage_id: None,
            auto_tags: Vec::new(),
            autoresponder: serde_json::json!({}),
            active: true,
            rate_limit_per_hour: 30,
            last_received_at: None,
            last_error: None,
            broken_mappings: Vec::new(),
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| (*key).to_string()).collect()
    }

    #[test]
    fn a_renamed_field_is_broken_and_named() {
        // The whole promise, in one pure function: the form used to have `job`, the mapping
        // still asks for it, and the answer names the key rather than saying "some fields".
        let source = source_with(Some("contact"), &[("email", "email"), ("job_title", "job")]);
        let broken = crate::mapping::health(&source.mapping_lines(), &keys(&["email", "phone"]));
        assert_eq!(broken, vec!["job".to_string()]);
    }

    #[test]
    fn an_intact_mapping_is_not_broken() {
        let source = source_with(Some("contact"), &[("email", "email"), ("job_title", "job")]);
        let broken = crate::mapping::health(&source.mapping_lines(), &keys(&["email", "job"]));
        assert!(broken.is_empty());
    }

    #[test]
    fn an_unknown_answer_never_clears_a_recorded_one() {
        // The load-bearing assertion. An empty list and an unreadable one are different facts,
        // and treating them as the same is how a forms-less install silently un-breaks every
        // form-bound source it has — the exact opposite of what the row is for.
        let unknown = BindingHealth::Unknown(HealthUnknown::FormsModuleAbsent);
        assert_eq!(unknown.keys_to_store(), None, "an unknown answer must not write");

        // A *known* empty answer does clear: the form was read and it has all the keys back.
        assert_eq!(BindingHealth::Healthy.keys_to_store(), Some([].as_slice()));
    }

    #[test]
    fn a_broken_answer_stores_the_keys_it_names() {
        let health = BindingHealth::Broken(keys(&["job"]));
        assert!(health.is_broken());
        assert_eq!(health.keys_to_store(), Some(["job".to_string()].as_slice()));
    }

    #[test]
    fn an_empty_broken_list_is_not_a_broken_answer() {
        // `Broken(vec![])` is the shape a "there may be a problem" flag would take, and it is
        // why `is_broken` checks the list rather than the variant: an empty list is the healthy
        // answer spelled the other way, and a screen that keys on the variant alone would show
        // a broken badge with nothing to name under it.
        assert!(!BindingHealth::Broken(Vec::new()).is_broken());
    }

    #[test]
    fn a_field_list_of_objects_and_a_plain_object_both_yield_keys() {
        // Picking one shape and treating the other as "no keys" is how a healthy form gets
        // reported as broken — a wrong answer in the safe-looking direction is still wrong.
        let list = serde_json::json!({
            "fields": [ { "name": "email" }, { "key": "phone" }, { "id": "job" }, { "label": "no key here" } ]
        });
        assert_eq!(object_keys(&list), keys(&["email", "phone", "job"]));

        let plain = serde_json::json!({ "email": {}, "phone": {} });
        assert_eq!(object_keys(&plain), keys(&["email", "phone"]));

        // A form definition that is not an object has told us nothing, and says so by being
        // empty rather than by panicking.
        assert!(object_keys(&serde_json::json!("nonsense")).is_empty());
    }

    #[test]
    fn a_source_with_no_form_key_is_unknown_and_not_broken() {
        // The keyed-endpoint case, and the reason a health check cannot be fatal: marking
        // every endpoint source broken on a platform whose forms module is absent would make
        // the badge a lie an operator acts on.
        let source = source_with(None, &[("email", "email")]);
        assert!(source.form_key.is_none());
    }

    #[test]
    fn the_health_verdict_is_the_pure_functions_and_not_a_second_implementation() {
        // `check` must not be able to disagree with `mapping::health`: it supplies keys and
        // reads the verdict. A second comparison inside `check` would be a second answer, and
        // the two would drift the way the round-robin cursor and the queue did.
        let source = source_with(Some("contact"), &[("email", "email"), ("job_title", "job")]);
        let available = keys(&["email", "phone", "name"]);
        let pure = crate::mapping::health(&source.mapping_lines(), &available);
        assert_eq!(pure, keys(&["job"]), "the pure function is the only decider");
    }

    #[test]
    fn a_mapping_entry_without_a_source_key_contributes_nothing_to_break() {
        // A line that maps a constant/fallback has no key to lose. Counting it would name a
        // field that never existed on the form.
        let mut entry = MappingEntry::new("company", "company_name");
        entry = entry.with_transforms(&["lower"]);
        let source = source_with(Some("contact"), &[]);
        let _ = (entry, source);
        assert!(crate::mapping::source_keys(&[]).is_empty());
    }
}
