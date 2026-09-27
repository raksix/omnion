//! Saved views: the filter, column and sort combination a person keeps (REQ-051, slice 2).
//!
//! A view is deliberately **the query, not a result**. It stores the same fields the list sends
//! (`search`, `owner`, `status`, `tag`, the date bounds, the sort) plus the column set, and the
//! screen re-issues it as a URL. That is what makes a view survive a record being added: a view
//! that stored ids would be stale the moment the list changed, and a view that stored a rendered
//! result would have to be refreshed by hand.
//!
//! Two rules make the store safe to share inside an organization:
//!
//! * A view is written in the **organization it was read in**, never in one the caller names, so
//!   a view cannot become a bridge between two tenants.
//! * Deleting removes the view, not the records it points at. A view is a lens.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{CrmError, Result};
use crate::query::ListQuery;

/// Entities a view may be saved for. Matches the migration's check constraint.
pub const VIEW_ENTITIES: [&str; 4] = ["contacts", "companies", "deals", "activities"];

/// Longest a view's name may be.
pub const MAX_VIEW_NAME: usize = 80;

/// A saved view as the screen reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct View {
    /// Identifier.
    pub id: Uuid,
    /// Organization the view lives in.
    pub organization_id: Uuid,
    /// Who owns it.
    pub owner_user_id: Uuid,
    /// What it filters (`contacts`, `companies`, …).
    pub entity: String,
    /// The name a person gave it.
    pub name: String,
    /// The filters, as the list query's own field names.
    pub filters: Value,
    /// The columns the list showed, in order. Empty means "every column".
    pub columns: Vec<String>,
    /// The sort, as `{ "key": …, "direction": … }`.
    pub sort: Value,
    /// Whether the organization may pick it up.
    pub is_shared: bool,
    /// When it was created.
    pub created_at: time::OffsetDateTime,
    /// When it last changed.
    pub updated_at: time::OffsetDateTime,
}

/// The body of a create or an update.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ViewChanges {
    /// What it filters.
    pub entity: String,
    /// The name.
    pub name: String,
    /// The filters.
    #[serde(default)]
    pub filters: Option<Value>,
    /// The columns.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// The sort.
    #[serde(default)]
    pub sort: Option<Value>,
    /// Whether it is shared.
    #[serde(default)]
    pub is_shared: Option<bool>,
}

/// What a view resolved to: a list query and a column set.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedView {
    /// The list request the view stands for.
    pub query: ListQuery,
    /// The columns it shows, empty when the caller has not chosen any.
    pub columns: Vec<String>,
    /// The sort key.
    pub sort: Option<String>,
    /// The sort direction.
    pub direction: Option<String>,
}

impl View {
    /// The list query this view stands for.
    ///
    /// A stored filter the list does not know is **dropped, not fatal**: a view saved before a
    /// field existed must still open. The one thing that would change the result silently is
    /// refused — an unknown `sort` key, because the list would order by something the person
    /// never chose.
    pub fn resolve(&self) -> Result<ResolvedView> {
        let filters = self.filters.clone();
        let mut query = ListQuery::default();

        let text = |key: &str| -> Option<String> {
            filters
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let number = |key: &str| filters.get(key).and_then(Value::as_i64);

        query.search = text("search");
        query.owner = text("owner");
        query.status = text("status");
        query.tag = text("tag");
        query.created_from = text("created_from").and_then(parse_date);
        query.created_to = text("created_to").and_then(parse_date);
        query.inactive_days = number("inactive_days").map(|value| value as i32);
        if let Some(archived) = filters.get("include_archived").and_then(Value::as_bool) {
            query.include_archived = Some(archived);
        }

        let sort_key: Option<String> = self
            .sort
            .get("key")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(key) = sort_key.as_deref() {
            if crate::query::sort_expression(&self.entity, key).is_none() {
                return Err(CrmError::InvalidQuery(format!(
                    "the saved view sorts by {key}, which is not a column of {}",
                    self.entity
                )));
            }
        }
        let direction: Option<String> = self
            .sort
            .get("direction")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "asc" | "desc"))
            .map(str::to_owned);
        query.sort = sort_key.clone();
        query.direction = direction.clone();

        Ok(ResolvedView {
            query,
            columns: self.columns.clone(),
            sort: sort_key,
            direction,
        })
    }
}

/// `YYYY-MM-DD` as a date, when the value is one.
fn parse_date(raw: String) -> Option<time::Date> {
    time::Date::parse(&raw, &time::format_description::well_known::Iso8601::DEFAULT).ok()
}

/// The columns the contact and company lists may show.
///
/// A closed set for the same reason the sort keys are: the column chooser becomes a header
/// label, and a label nobody controls would be a header the list cannot render.
pub fn available_columns(entity: &str) -> &'static [&'static str] {
    match entity {
        "contacts" => &[
            "name",
            "company",
            "email",
            "phone",
            "job_title",
            "owner",
            "status",
            "tags",
            "last_activity_at",
            "updated_at",
        ],
        "companies" => &[
            "name",
            "domain",
            "industry",
            "owner",
            "status",
            "tags",
            "contact_count",
            "updated_at",
        ],
        "deals" => &[
            "title",
            "company",
            "stage",
            "owner",
            "amount",
            "probability",
            "expected_close_on",
            "updated_at",
        ],
        "activities" => &["kind", "subject", "contact", "company", "owner", "occurred_at"],
        _ => &[],
    }
}

/// The columns a view may store: the known ones, deduped, in the order they were chosen.
///
/// An unknown column is dropped rather than refused — the same reason an unknown filter is
/// dropped: a chooser that breaks when a column is renamed is a chooser people stop using. The
/// **sort** is refused, because an unknown sort would change the order silently.
pub fn clean_columns(entity: &str, columns: &[String]) -> Vec<String> {
    let available = available_columns(entity);
    let mut cleaned: Vec<String> = Vec::new();
    for column in columns {
        let trimmed = column.trim();
        if trimmed.is_empty() || !available.contains(&trimmed) {
            continue;
        }
        if !cleaned.iter().any(|existing| existing == trimmed) {
            cleaned.push(trimmed.to_owned());
        }
    }
    cleaned
}

/// Validate a create or an update, and return the values to write.
///
/// The body names the entity, not the caller: a screen that sends `entity=contacts` and a screen
/// that sends `entity=companies` must not be able to disagree with the URL they were posted from,
/// so the argument exists only to state that expectation in the signature and is checked against
/// the body — a mismatch is a `400`, not a silent win for the body.
pub fn validate(entity: &str, changes: &ViewChanges) -> Result<NormalisedView> {
    let kind = changes.entity.trim().to_lowercase();
    if !VIEW_ENTITIES.contains(&kind.as_str()) {
        return Err(CrmError::invalid(
            "view",
            "entity",
            format!(
                "a view filters one of: {}",
                VIEW_ENTITIES.join(", ")
            ),
        ));
    }

    if kind != entity.trim().to_lowercase() {
        return Err(CrmError::invalid(
            "view",
            "entity",
            format!("this screen saves {entity} views, the body asked for {kind}"),
        ));
    }

    let name = changes.name.trim().to_owned();
    if name.is_empty() {
        return Err(CrmError::invalid("view", "name", "a saved view needs a name"));
    }
    if name.chars().count() > MAX_VIEW_NAME {
        return Err(CrmError::invalid(
            "view",
            "name",
            format!("a view name is at most {MAX_VIEW_NAME} characters"),
        ));
    }

    let sort = match changes.sort.as_ref() {
        None | Some(Value::Null) => json!({}),
        Some(value) => {
            let key = value.get("key").and_then(Value::as_str).unwrap_or_default();
            if !key.is_empty() && crate::query::sort_expression(&kind, key).is_none() {
                return Err(CrmError::invalid(
                    "view",
                    "sort",
                    format!("{key} is not a column of {entity}"),
                ));
            }
            let direction = match value.get("direction").and_then(Value::as_str) {
                Some("asc") | Some("desc") => value.get("direction").cloned().unwrap(),
                _ => json!("desc"),
            };
            json!({ "key": key, "direction": direction })
        }
    };

    Ok(NormalisedView {
        entity: kind.clone(),
        name,
        filters: changes
            .filters
            .clone()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| json!({})),
        columns: changes
            .columns
            .as_deref()
            .map(|columns| clean_columns(&kind, columns))
            .unwrap_or_default(),
        sort,
        is_shared: changes.is_shared.unwrap_or(false),
    })
}

/// A view that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalisedView {
    /// What it filters.
    pub entity: String,
    /// The name.
    pub name: String,
    /// The filters.
    pub filters: Value,
    /// The columns.
    pub columns: Vec<String>,
    /// The sort.
    pub sort: Value,
    /// Whether it is shared.
    pub is_shared: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view to resolve in the tests below.
    fn view_fixture() -> View {
        View {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            owner_user_id: Uuid::nil(),
            entity: "contacts".to_owned(),
            name: "My open leads".to_owned(),
            filters: json!({ "status": "lead", "owner": "me" }),
            columns: vec!["name".to_owned(), "status".to_owned()],
            sort: json!({ "key": "updated_at", "direction": "desc" }),
            is_shared: false,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_view_resolves_to_the_list_query_it_stands_for() {
        let resolved = view_fixture().resolve().expect("the view resolves");
        assert_eq!(resolved.query.status.as_deref(), Some("lead"));
        assert_eq!(resolved.query.owner.as_deref(), Some("me"));
        assert_eq!(resolved.columns, vec!["name".to_owned(), "status".to_owned()]);
        assert_eq!(resolved.sort.as_deref(), Some("updated_at"));
        assert_eq!(resolved.direction.as_deref(), Some("desc"));
    }

    #[test]
    fn a_filter_the_list_does_not_know_is_dropped_rather_than_fatal() {
        let mut view = view_fixture();
        view.filters = json!({ "status": "lead", "moon_phase": "waxing" });
        let resolved = view.resolve().expect("an unknown filter does not break a view");
        assert_eq!(resolved.query.status.as_deref(), Some("lead"));
    }

    #[test]
    fn an_unknown_sort_is_refused_because_it_would_reorder_silently() {
        let mut view = view_fixture();
        view.sort = json!({ "key": "phase_of_the_moon", "direction": "desc" });
        let error = view.resolve().expect_err("an unknown sort is refused");
        assert!(error.to_string().contains("phase_of_the_moon"), "{error}");
    }

    #[test]
    fn a_view_without_a_sort_opens_on_the_list_default() {
        let mut view = view_fixture();
        view.sort = json!({});
        let resolved = view.resolve().unwrap();
        assert!(resolved.sort.is_none());
        assert!(resolved.direction.is_none());
    }

    #[test]
    fn a_date_filter_that_is_not_a_date_is_dropped_not_read_as_a_year() {
        let mut view = view_fixture();
        view.filters = json!({ "created_from": "last tuesday" });
        assert!(view.resolve().unwrap().query.created_from.is_none());
    }

    #[test]
    fn an_iso_date_filter_is_kept() {
        let mut view = view_fixture();
        view.filters = json!({ "created_from": "2026-01-31" });
        let resolved = view.resolve().unwrap();
        assert_eq!(
            resolved.query.created_from.map(|date| date.to_string()),
            Some("2026-01-31".to_owned())
        );
    }

    #[test]
    fn a_direction_that_is_not_a_direction_is_ignored() {
        let mut view = view_fixture();
        view.sort = json!({ "key": "updated_at", "direction": "sideways" });
        let resolved = view.resolve().unwrap();
        assert_eq!(resolved.sort.as_deref(), Some("updated_at"));
        assert!(resolved.direction.is_none());
    }

    #[test]
    fn a_saved_view_that_names_another_entity_is_refused() {
        let changes = ViewChanges {
            entity: "invoices".to_owned(),
            name: "Nope".to_owned(),
            ..ViewChanges::default()
        };
        let error = validate("invoices", &changes).expect_err("the entity must be one of the four");
        assert!(error.to_string().contains("contacts"), "{error}");
    }

    #[test]
    fn a_nameless_view_is_refused_on_its_name() {
        let changes = ViewChanges {
            entity: "contacts".to_owned(),
            name: "   ".to_owned(),
            ..ViewChanges::default()
        };
        let error = validate("contacts", &changes).expect_err("a view needs a name");
        assert!(error.to_string().contains("name"), "{error}");
    }

    #[test]
    fn a_view_of_another_entity_cannot_sort_by_a_column_it_does_not_have() {
        let changes = ViewChanges {
            entity: "companies".to_owned(),
            name: "Wrong sort".to_owned(),
            sort: Some(json!({ "key": "last_activity_at" })),
            ..ViewChanges::default()
        };
        let error = validate("companies", &changes).expect_err("the sort must belong to the entity");
        assert!(error.to_string().contains("last_activity_at"), "{error}");
    }

    #[test]
    fn the_column_chooser_keeps_the_known_columns_in_the_order_they_were_chosen() {
        let cleaned = clean_columns(
            "contacts",
            &["status".to_owned(), "name".to_owned(), "status".to_owned()],
        );
        assert_eq!(
            cleaned,
            vec!["status".to_owned(), "name".to_owned()],
            "the duplicate is dropped and the order is the person's"
        );
    }

    #[test]
    fn the_column_chooser_drops_a_column_the_entity_does_not_have() {
        let cleaned = clean_columns("companies", &["name".to_owned(), "job_title".to_owned()]);
        assert_eq!(cleaned, vec!["name".to_owned()]);
    }

    #[test]
    fn a_saved_view_defaults_to_private_and_to_the_lists_sort() {
        let changes = ViewChanges {
            entity: "contacts".to_owned(),
            name: "New view".to_owned(),
            ..ViewChanges::default()
        };
        let normalised = validate("contacts", &changes).unwrap();
        assert!(!normalised.is_shared);
        assert_eq!(normalised.sort, json!({}));
        assert_eq!(normalised.filters, json!({}));
    }

    #[test]
    fn a_filters_value_that_is_not_an_object_is_refused_to_become_a_query() {
        let changes = ViewChanges {
            entity: "contacts".to_owned(),
            name: "Broken".to_owned(),
            filters: Some(json!("status=lead")),
            ..ViewChanges::default()
        };
        let normalised = validate("contacts", &changes).unwrap();
        assert_eq!(
            normalised.filters,
            json!({}),
            "a non-object filter stores nothing rather than a string the list cannot read"
        );
    }

    #[test]
    fn a_sort_without_a_direction_gets_the_lists_default_direction() {
        let changes = ViewChanges {
            entity: "contacts".to_owned(),
            name: "Newest first".to_owned(),
            sort: Some(json!({ "key": "created_at" })),
            ..ViewChanges::default()
        };
        let normalised = validate("contacts", &changes).unwrap();
        assert_eq!(normalised.sort["key"], json!("created_at"));
        assert_eq!(normalised.sort["direction"], json!("desc"));
    }
}
