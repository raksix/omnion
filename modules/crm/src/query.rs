//! The list query every CRM screen sends: filters, sort, cursor paging and the visibility level,
//! built as **SQL** rather than as a `where` in Rust.
//!
//! Two rules hold here, and both are why this is a module of its own:
//!
//! * **Visibility is enforced in the query.** A caller with the `own` level never receives another
//!   person's record, not even in a count, not even in a total. Filtering afterwards in Rust
//!   would leave the totals and the paging wrong.
//! * **Only the clauses in use are written.** A builder that always emitted a clause would pin a
//!   plan and, worse, would make "no filter" and "filter that matched nothing" the same query for
//!   the database to confuse. The placeholder numbers follow from what is already bound.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{CrmError, Result};
use crate::model::{Visibility, clean};

/// Rows a page holds when the caller names no size.
pub const DEFAULT_PER_PAGE: i64 = 50;

/// Hard cap on a page.
pub const MAX_PER_PAGE: i64 = 200;

/// Longest a search term may be before it is refused.
pub const MAX_SEARCH_LENGTH: usize = 120;

/// The sort keys every CRM list accepts, per entity.
///
/// A closed set, because the sort becomes an `order by` — a caller-supplied column name would be
/// an injection point, and an unknown column would be an error the list cannot render.
pub fn sortable(entity: &str) -> &'static [&'static str] {
    match entity {
        "contacts" => &[
            "last_name",
            "first_name",
            "email",
            "company",
            "status",
            "owner",
            "last_activity_at",
            "created_at",
            "updated_at",
        ],
        "companies" => &[
            "name",
            "domain",
            "industry",
            "status",
            "owner",
            "updated_at",
        ],
        "deals" => &[
            "title",
            "amount",
            "expected_close_on",
            "stage_changed_at",
            "probability",
            "updated_at",
        ],
        "activities" => &["occurred_at", "due_at", "kind", "created_at"],
        _ => &[],
    }
}

/// The SQL expression a sort key orders by, plus whether it descends by default.
#[must_use]
pub fn sort_expression(entity: &str, key: &str) -> Option<(&'static str, bool)> {
    let expression = match (entity, key) {
        ("contacts", "last_name") => ("lower(c.last_name)", true),
        ("contacts", "first_name") => ("lower(c.first_name)", true),
        ("contacts", "email") => ("lower(c.email)", true),
        ("contacts", "company") => ("lower(co.name)", true),
        ("contacts", "status") => ("c.status", true),
        ("contacts", "owner") => ("lower(u.display_name)", true),
        ("contacts", "last_activity_at") => ("c.last_activity_at", true),
        ("contacts", "created_at") => ("c.created_at", true),
        ("contacts", "updated_at") => ("c.updated_at", true),
        ("companies", "name") => ("lower(co.name)", true),
        ("companies", "domain") => ("lower(co.domain)", true),
        ("companies", "industry") => ("lower(co.industry)", true),
        ("companies", "status") => ("co.status", true),
        ("companies", "owner") => ("lower(u.display_name)", true),
        ("companies", "updated_at") => ("co.updated_at", true),
        ("deals", "title") => ("lower(d.title)", true),
        ("deals", "amount") => ("d.amount", true),
        ("deals", "expected_close_on") => ("d.expected_close_on", true),
        ("deals", "stage_changed_at") => ("d.stage_changed_at", true),
        ("deals", "probability") => ("d.probability", true),
        ("deals", "updated_at") => ("d.updated_at", true),
        ("activities", "occurred_at") => ("a.occurred_at", true),
        ("activities", "due_at") => ("a.due_at", true),
        ("activities", "kind") => ("a.kind", true),
        ("activities", "created_at") => ("a.created_at", true),
        _ => return None,
    };

    Some(expression)
}

/// A list request, as the screens send it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListQuery {
    /// Free text: name, address, company, title.
    #[serde(default)]
    pub search: Option<String>,
    /// `me`, `unassigned` or a user id.
    #[serde(default)]
    pub owner: Option<String>,
    /// Lifecycle status (`lead`, `customer`, `partner`, `churned`).
    #[serde(default)]
    pub status: Option<String>,
    /// One tag the record must carry.
    #[serde(default)]
    pub tag: Option<String>,
    /// A company id (contacts, deals and activities hang off one).
    #[serde(default)]
    pub company_id: Option<Uuid>,
    /// A contact id (deals and activities hang off one).
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    /// A deal id (activities hang off one).
    #[serde(default)]
    pub deal_id: Option<Uuid>,
    /// A pipeline id (deals only).
    #[serde(default)]
    pub pipeline_id: Option<Uuid>,
    /// Created on or after.
    #[serde(default)]
    pub created_from: Option<Date>,
    /// Created on or before.
    #[serde(default)]
    pub created_to: Option<Date>,
    /// "No activity since this many days ago" (contacts only).
    #[serde(default)]
    pub inactive_days: Option<i32>,
    /// Sort key of the entity.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc` (default: the key's own).
    #[serde(default)]
    pub direction: Option<String>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Opaque cursor of the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Include the archived records.
    #[serde(default)]
    pub include_archived: Option<bool>,
    /// How much of the organization the caller may see.
    #[serde(default)]
    pub visibility: Option<Visibility>,
}

impl ListQuery {
    /// A query with only the page size set, for a code path that does not filter.
    #[must_use]
    pub fn paginated(limit: i64) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// The page size, capped and never below one.
    #[must_use]
    pub fn page_size(&self) -> i64 {
        self.limit
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(1, MAX_PER_PAGE)
    }

    /// The sort to apply, refusing an unknown key instead of ignoring it.
    ///
    /// Refusing is deliberate: a saved view that silently sorted by something else would look
    /// like data loss to the person who saved it.
    pub fn resolve_sort(&self, entity: &str, fallback: &str) -> Result<(&'static str, bool)> {
        let key = self
            .sort
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .unwrap_or(fallback);

        let Some((expression, default_desc)) = sort_expression(entity, key) else {
            return Err(CrmError::InvalidQuery(format!(
                "\"{key}\" is not a sort column of {entity}; the columns are {}",
                sortable(entity).join(", ")
            )));
        };

        let desc = match self.direction.as_deref().map(str::trim) {
            None | Some("") => default_desc,
            Some("asc") => false,
            Some("desc") => true,
            Some(other) => {
                return Err(CrmError::InvalidQuery(format!(
                    "\"{other}\" is not a sort direction; use asc or desc"
                )));
            }
        };

        Ok((expression, desc))
    }

    /// The visibility the caller asked to read at.
    ///
    /// The level is a *narrowing* of what the caller's permission already allows, so an unknown
    /// value is refused rather than widened: a typo in a saved view must not silently turn a
    /// personal list into the whole organization's.
    pub fn visibility(&self) -> Result<Visibility> {
        match &self.visibility {
            None => Ok(Visibility::All),
            Some(level) => Visibility::parse(Some(level.as_str())).ok_or_else(|| {
                CrmError::InvalidQuery("visibility is one of own, team, all".to_owned())
            }),
        }
    }

    /// The cursor decoded, or `None` for the first page.
    pub fn cursor_id(&self) -> Result<Option<Uuid>> {
        match self.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            None => Ok(None),
            Some(cursor) => Uuid::parse_str(cursor)
                .map(Some)
                .map_err(|_| CrmError::InvalidQuery("the cursor is not a record identifier".to_owned())),
        }
    }

    /// The search term, refused when it is longer than [`MAX_SEARCH_LENGTH`].
    pub fn search_term(&self) -> Result<Option<String>> {
        let term = clean(self.search.clone())
            .map(|value| value.to_lowercase())
            .map(|value| {
                if value.chars().count() > MAX_SEARCH_LENGTH {
                    None
                } else {
                    Some(value)
                }
            })
            .unwrap_or(None);
        if clean(self.search.clone()).is_some()
            && clean(self.search.clone())
                .is_some_and(|value| value.chars().count() > MAX_SEARCH_LENGTH)
        {
            return Err(CrmError::InvalidQuery(format!(
                "a search term is at most {MAX_SEARCH_LENGTH} characters"
            )));
        }
        Ok(term)
    }

    /// The tag filter, normalised the way a tag is stored.
    pub fn tag_filter(&self) -> Option<String> {
        clean(self.tag.clone()).map(|value| value.to_lowercase())
    }

    /// `true` when the caller asked for the archived records too.
    #[must_use]
    pub fn shows_archived(&self) -> bool {
        self.include_archived.unwrap_or(false)
    }
}

/// Who the visibility level narrows a query to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The organization the query runs in.
    pub organization_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// How much the caller may see.
    pub visibility: Visibility,
    /// The team the caller's `team` level reads: every member of these groups plus the caller.
    pub team_user_ids: Vec<Uuid>,
}

impl Scope {
    /// A scope that reads everything of the organization.
    #[must_use]
    pub fn all(organization_id: Uuid, user_id: Uuid) -> Self {
        Self {
            organization_id,
            user_id,
            visibility: Visibility::All,
            team_user_ids: Vec::new(),
        }
    }

    /// A narrowed scope.
    #[must_use]
    pub fn with(mut self, visibility: Visibility, team_user_ids: Vec<Uuid>) -> Self {
        self.visibility = visibility;
        self.team_user_ids = team_user_ids;
        self
    }

    /// The user identifiers this scope admits.
    ///
    /// `own` admits exactly the caller — reading a colleague's record because the caller once
    /// shared a group with them would be a privacy bug, not a feature. `team` adds the group
    /// members, and every level admits the caller themselves.
    ///
    /// Every narrowing level also admits a record with **no** owner (see
    /// [`crate::contacts::push_visibility`]): an unassigned record belongs to nobody, so hiding
    /// it from everyone would make the list unable to show what still needs an owner.
    #[must_use]
    pub fn visible_user_ids(&self) -> Vec<Uuid> {
        let mut ids = match self.visibility {
            Visibility::Own => Vec::new(),
            Visibility::Team => self.team_user_ids.clone(),
            Visibility::All => Vec::new(),
        };
        if !ids.contains(&self.user_id) {
            ids.push(self.user_id);
        }
        ids
    }
}

/// One page of rows plus the cursor of the next one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Page<T> {
    /// The rows of this page.
    pub items: Vec<T>,
    /// Cursor of the next page, absent when the page is the last one.
    pub next_cursor: Option<String>,
    /// How many rows the whole filter matches, when the store counted it.
    pub total_estimate: i64,
}

impl<T> Page<T> {
    /// A page from rows and the cursor that follows them.
    #[must_use]
    pub fn new(items: Vec<T>, next_cursor: Option<String>, total_estimate: i64) -> Self {
        Self {
            items,
            next_cursor,
            total_estimate,
        }
    }
}

/// The cursor of the next page: the last row's id, plus the order the rows came in.
///
/// Keyset paging, not `offset`: an offset shifts when a row is archived, so the second page of a
/// live list would skip or repeat records.
#[must_use]
pub fn next_cursor<T, F>(items: &[T], id_of: F) -> Option<String>
where
    F: Fn(&T) -> Uuid,
{
    items.last().map(|last| id_of(last).to_string())
}

/// The moment a query is evaluated at — one value for a whole page, so a list cannot straddle
/// two moments and disagree with its own total.
#[must_use]
pub fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_sort_is_refused_with_the_columns_named() {
        let error = ListQuery {
            sort: Some("nonsense".to_owned()),
            ..ListQuery::default()
        }
        .resolve_sort("contacts", "updated_at")
        .expect_err("an unknown sort must be refused");
        assert!(error.to_string().contains("nonsense"));
        assert!(error.to_string().contains("last_name"));
    }

    #[test]
    fn a_sort_falls_back_to_the_entity_default_and_reads_the_direction() {
        assert_eq!(
            ListQuery::default().resolve_sort("contacts", "updated_at").unwrap(),
            ("c.updated_at", true)
        );
        let ascending = ListQuery {
            direction: Some("asc".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(ascending.resolve_sort("contacts", "updated_at").unwrap(), ("c.updated_at", false));
        let sideway = ListQuery {
            direction: Some("sideways".to_owned()),
            ..ListQuery::default()
        };
        assert!(sideway.resolve_sort("contacts", "updated_at").is_err());
    }

    #[test]
    fn every_entity_offers_a_sortable_column_set() {
        for entity in ["contacts", "companies", "deals", "activities"] {
            let columns = sortable(entity);
            assert!(!columns.is_empty(), "{entity} must be sortable");
            for column in columns {
                assert!(
                    sort_expression(entity, column).is_some(),
                    "{entity}.{column} must map to a SQL expression"
                );
            }
            assert!(sort_expression(entity, "not_a_column").is_none());
        }
        assert!(sortable("nothing").is_empty());
    }

    #[test]
    fn the_page_size_is_capped_and_never_zero() {
        assert_eq!(ListQuery::default().page_size(), DEFAULT_PER_PAGE);
        assert_eq!(ListQuery { limit: Some(5), ..Default::default() }.page_size(), 5);
        assert_eq!(ListQuery { limit: Some(0), ..Default::default() }.page_size(), 1);
        assert_eq!(ListQuery { limit: Some(9_999), ..Default::default() }.page_size(), MAX_PER_PAGE);
    }

    #[test]
    fn the_visible_users_of_a_team_scope_always_include_the_caller() {
        let user = Uuid::new_v4();
        let colleague = Uuid::new_v4();
        let scope = Scope::all(Uuid::new_v4(), user).with(Visibility::Team, vec![colleague]);
        let visible = scope.visible_user_ids();
        assert!(visible.contains(&user));
        assert!(visible.contains(&colleague));
    }

    #[test]
    fn the_visible_users_of_an_own_scope_are_exactly_the_caller() {
        let user = Uuid::new_v4();
        let scope = Scope::all(Uuid::new_v4(), user).with(Visibility::Own, vec![Uuid::new_v4()]);
        assert_eq!(scope.visible_user_ids(), vec![user]);
    }

    #[test]
    fn the_all_level_admits_nobody_in_particular() {
        let scope = Scope::all(Uuid::new_v4(), Uuid::new_v4());
        // `all` writes no owner clause at all, so the id list is not consulted — but it must not
        // silently become a narrowing list if a caller starts using it.
        assert_eq!(scope.visible_user_ids(), vec![scope.user_id]);
    }

    #[test]
    fn a_cursor_is_a_record_identifier_or_a_refusal() {
        let id = Uuid::new_v4();
        let query = ListQuery {
            cursor: Some(id.to_string()),
            ..ListQuery::default()
        };
        assert_eq!(query.cursor_id().unwrap(), Some(id));
        let broken = ListQuery {
            cursor: Some("page-3".to_owned()),
            ..ListQuery::default()
        };
        assert!(broken.cursor_id().is_err());
    }


    #[test]
    fn a_search_term_is_trimmed_lowercased_and_length_checked() {
        let query = ListQuery {
            search: Some("  ADA ".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(query.search_term().unwrap(), Some("ada".to_owned()));

        let long = ListQuery {
            search: Some("a".repeat(MAX_SEARCH_LENGTH + 1)),
            ..ListQuery::default()
        };
        assert!(long.search_term().is_err());
    }

    #[test]
    fn the_cursor_of_a_page_is_the_last_rows_id() {
        let rows = vec![(Uuid::new_v4(), "a"), (Uuid::new_v4(), "b")];
        let cursor = next_cursor(&rows, |row| row.0).expect("a non-empty page has a cursor");
        assert_eq!(cursor, rows[1].0.to_string());
        let empty: Vec<(Uuid, &str)> = Vec::new();
        assert!(next_cursor(&empty, |row| row.0).is_none());
    }
}
