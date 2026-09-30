//! The notification record and the query shapes the store accepts.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::vocabulary::{is_category, is_priority};

/// One notification addressed to one person.
///
/// A row is *the fact* — "a page of yours is waiting for approval" — and never a delivery
/// record. The channels it went out over are separate rows ([`crate::store::Delivery`]), which
/// is the split that makes "the e-mail failed but the panel shows it" representable instead of
/// a single status that is wrong for one of them.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Notification {
    /// The row's id.
    pub id: Uuid,
    /// Organization the fact belongs to (`None` = platform level).
    pub organization_id: Option<Uuid>,
    /// The person this is addressed to.
    pub user_id: Uuid,
    /// Which group it belongs to.
    pub category: String,
    /// How urgent it is.
    pub priority: String,
    /// One line, shown in the list and the bell.
    pub title: String,
    /// Optional second line.
    pub body: String,
    /// Where the row's link goes when it is followed; `None` means the notification is not
    /// actionable and the panel must not render a link that goes nowhere.
    pub url: Option<String>,
    /// What produced it, e.g. `page` or `security`.
    pub source_type: Option<String>,
    /// The producing record's id, as text.
    pub source_id: Option<String>,
    /// Structured detail for the detail drawer; never carries a secret.
    pub payload: serde_json::Value,
    /// When it was read, if it was.
    pub read_at: Option<OffsetDateTime>,
    /// When it was filed away, if it was. A read notification is a *badge* question; an
    /// archived one is a *list* question, and the filter is the difference.
    pub archived_at: Option<OffsetDateTime>,
    /// When it happened.
    pub created_at: OffsetDateTime,
}

impl Notification {
    /// `true` when the row is waiting for the reader.
    #[must_use]
    pub fn is_unread(&self) -> bool {
        self.read_at.is_none()
    }
}

/// A notification to write.
///
/// Built with [`NewNotification::to`] and the `with_*` setters. The category and priority are
/// validated in [`NewNotification::build`] rather than at the SQL boundary so the caller gets
/// a message naming the field instead of a check-constraint violation.
#[derive(Debug, Clone, PartialEq)]
pub struct NewNotification {
    /// The person to address.
    pub user_id: Uuid,
    /// Which group it belongs to.
    pub category: String,
    /// How urgent it is.
    pub priority: String,
    /// One line.
    pub title: String,
    /// Optional second line.
    pub body: String,
    /// Where its link goes.
    pub url: Option<String>,
    /// What produced it.
    pub source_type: Option<String>,
    /// The producing record's id.
    pub source_id: Option<String>,
    /// Structured detail.
    pub payload: serde_json::Value,
    /// Collapse repeats: a second emit with the same key is the *same* fact, and the table
    /// keeps one row for it.
    pub dedupe_key: Option<String>,
}

impl NewNotification {
    /// A notification of one category for one person, with the default priority.
    #[must_use]
    pub fn to(user_id: Uuid, category: &str, title: impl Into<String>) -> Self {
        Self {
            user_id,
            category: category.to_owned(),
            priority: "normal".to_owned(),
            title: title.into(),
            body: String::new(),
            url: None,
            source_type: None,
            source_id: None,
            payload: serde_json::Value::Object(serde_json::Map::new()),
            dedupe_key: None,
        }
    }

    /// Set the priority.
    #[must_use]
    pub fn with_priority(mut self, priority: impl Into<String>) -> Self {
        self.priority = priority.into();
        self
    }

    /// Set the body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    /// Set the link the row follows.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Set the producing record.
    #[must_use]
    pub fn with_source(
        mut self,
        source_type: impl Into<String>,
        source_id: impl Into<String>,
    ) -> Self {
        self.source_type = Some(source_type.into());
        self.source_id = Some(source_id.into());
        self
    }

    /// Set the structured detail.
    #[must_use]
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Collapse repeats under one key.
    #[must_use]
    pub fn with_dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    /// Validate the record before it is written.
    ///
    /// An empty title is refused rather than stored: the list renders the title as its own
    /// line, and a row with nothing there is a row the reader cannot act on, so it would sit
    /// in the inbox forever with nothing to say.
    ///
    /// # Errors
    /// Returns [`crate::NotificationError::Invalid`] naming the field that is wrong.
    pub fn build(self) -> crate::Result<Self, crate::error::NotificationError> {
        if !is_category(&self.category) {
            return Err(crate::error::NotificationError::invalid(format!(
                "category \"{}\" is not one of {:?}",
                self.category,
                crate::vocabulary::CATEGORIES
            )));
        }
        if !is_priority(&self.priority) {
            return Err(crate::error::NotificationError::invalid(format!(
                "priority \"{}\" is not one of {:?}",
                self.priority,
                crate::vocabulary::PRIORITIES
            )));
        }
        if self.title.trim().is_empty() {
            return Err(crate::error::NotificationError::invalid(
                "a notification needs a title".to_owned(),
            ));
        }
        Ok(self)
    }
}

/// One channel a notification was tried on, and what became of it.
///
/// **The reader's own view of the queue, not the runner's.** This is the type the detail drawer
/// renders, and it exists because "it is in my panel but the e-mail never arrived" has to be a
/// *row the reader can see* rather than an inference from the absence of something. A status
/// the reader cannot see is a status they cannot trust: the platform knows a channel failed and
/// the notification is delivered all the same, and that is exactly the state a person needs to
/// be shown.
///
/// The `attempts`/`max_attempts` pair is carried because the interesting answer is not "failed"
/// but "failed after four tries", and a drawer that prints only the state makes a delivery
/// retried once look identical to one abandoned.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct DeliveryRow {
    /// The channel this attempt was made over.
    pub channel: String,
    /// Where it got to: `pending`, `sent`, `failed` or `skipped`.
    pub status: String,
    /// How many times it has been tried, including the one in flight.
    pub attempts: i32,
    /// The number of tries before it gives up.
    pub max_attempts: i32,
    /// The transport's own status code, when it answered with one.
    pub response_status: Option<i32>,
    /// Why it did not go out, in the platform's words rather than a raw driver error.
    pub error: Option<String>,
    /// When it was delivered, if it was.
    pub sent_at: Option<OffsetDateTime>,
    /// When the attempt is next due; `None` once the row is no longer retryable.
    pub next_attempt_at: Option<OffsetDateTime>,
}

/// The filters a list read accepts.
///
/// One struct rather than a dozen query parameters, and the reason is the same as everywhere
/// else in this workspace: the WHERE clause and the values it binds are generated by ONE loop
/// over ONE list, so a placeholder cannot exist where no value was pushed.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Keep only these categories.
    pub categories: Vec<String>,
    /// `true` for unread only, `false` for read only, `None` for both.
    pub unread: Option<bool>,
    /// Keep only these priorities.
    pub priorities: Vec<String>,
    /// Keep only notifications that went out over this channel.
    pub channel: Option<String>,
    /// Hide the archived rows.
    pub include_archived: bool,
    /// Hide the rows that have been read.
    pub include_read: bool,
    /// The instant to page from, exclusive. This is the keyset: `created_at, id` is the
    /// stable pair, and a plain `offset` would skip or repeat a row whenever one arrives while
    /// the reader is paging.
    pub before: Option<OffsetDateTime>,
    /// Page size.
    pub limit: i64,
}

/// One page of notifications.
#[derive(Debug, Clone)]
pub struct NotificationPage {
    /// The rows.
    pub notifications: Vec<Notification>,
    /// Whether another page exists behind this one.
    pub has_more: bool,
}

/// The grouped counts the bell shows.
///
/// A **real** count per category, never a display number: a group whose domain does not exist
/// in this installation reports zero and hides itself in the panel, which is the difference
/// between "nothing is waiting" and "this platform cannot count that".
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    /// How many notifications are unread in total.
    pub unread: i64,
    /// Per category: the category and how many of it are unread.
    pub by_category: Vec<CategoryCount>,
}

/// How many unread notifications one category holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CategoryCount {
    /// The category name.
    pub category: String,
    /// How many of it are unread.
    pub count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::NotificationError;

    fn user() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn a_plain_notification_builds() {
        let built = NewNotification::to(user(), "approval", "A page is waiting")
            .build()
            .expect("valid");
        assert_eq!(built.priority, "normal");
        assert!(built.body.is_empty());
        assert_eq!(built.dedupe_key, None);
    }

    #[test]
    fn the_default_build_names_the_category_it_refused() {
        let error = NewNotification::to(user(), "invoice", "x")
            .build()
            .expect_err("invoice is not a category");
        assert!(matches!(error, NotificationError::Invalid(_)));
        assert!(error.to_string().contains("invoice"));
        assert!(error.to_string().contains("approval"));
    }

    #[test]
    fn the_default_build_names_the_priority_it_refused() {
        let error = NewNotification::to(user(), "ticket", "x")
            .with_priority("urgent")
            .build()
            .expect_err("urgent is not a priority");
        assert!(error.to_string().contains("urgent"));
    }

    #[test]
    fn a_title_of_whitespace_is_refused() {
        let error = NewNotification::to(user(), "system", "   ")
            .build()
            .expect_err("blank title");
        assert!(error.to_string().contains("title"));
    }

    #[test]
    fn setters_compose_without_overwriting_each_other() {
        let built = NewNotification::to(user(), "security", "New sign-in")
            .with_priority("high")
            .with_body("From a device we do not know")
            .with_url("/settings/iam/sessions")
            .with_source("session", "abc")
            .with_dedupe_key("security:session:abc")
            .build()
            .expect("valid");
        assert_eq!(built.priority, "high");
        assert_eq!(built.body, "From a device we do not know");
        assert_eq!(built.url.as_deref(), Some("/settings/iam/sessions"));
        assert_eq!(built.source_type.as_deref(), Some("session"));
        assert_eq!(built.dedupe_key.as_deref(), Some("security:session:abc"));
    }

    #[test]
    fn unread_is_read_at() {
        let now = OffsetDateTime::now_utc();
        let mut row = Notification {
            id: Uuid::new_v4(),
            organization_id: None,
            user_id: user(),
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            title: "t".to_owned(),
            body: String::new(),
            url: None,
            source_type: None,
            source_id: None,
            payload: serde_json::json!({}),
            read_at: None,
            archived_at: None,
            created_at: now,
        };
        assert!(row.is_unread());
        row.read_at = Some(now);
        assert!(!row.is_unread());
    }
}
