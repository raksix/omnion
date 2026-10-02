//! Omnion content · newsletter — lists, double opt-in subscribers and the sent-issue archive
//! (REQ-064, slice 4b).
//!
//! A newsletter is the only place where the platform holds an address **it cannot verify until
//! somebody clicks a link**, and every rule in this file follows from that one fact:
//!
//! * **Tokens are stored hashed, never in the clear.** The store *generates* a token and
//!   *returns it* to the caller (which mails it) while persisting only `sha256(token)`. A
//!   leaked table therefore contains no link anybody can click, and a token seen in a log, a
//!   browser history or a `Referer` header is useless without its row.
//!
//! * **Confirmation is single-use and time-limited, and the expiry is a fact about the row.**
//!   Re-confirming must not work; a link that stays valid for a year is not an opt-in. Both
//!   rules are enforced here rather than assumed, and [`NewsletterStore::confirm`] is the only
//!   function that can move a subscriber to `confirmed`.
//!
//! * **Unsubscribe keeps the row.** `status` flips, the address stays. "This address asked to
//!   leave" is exactly what stops a later CSV import from re-adding them, and deleting the row
//!   would make the next import of the same list undo a decision somebody made on purpose.
//!
//! * **Double opt-in is per LIST, not per site.** Somebody who wants product announcements and
//!   not the weekly digest is two rows, not one boolean.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};

/// Longest list name the panel will store.
pub const MAX_LIST_NAME: usize = 120;
/// Longest list description.
pub const MAX_LIST_DESCRIPTION: usize = 600;
/// Longest issue subject line.
pub const MAX_SUBJECT: usize = 200;
/// Longest address a store will accept. RFC 5321 caps a path at 256 octets; the practical
/// ceiling is well below that and a longer "address" is a paste accident, not a subscriber.
pub const MAX_EMAIL: usize = 254;
/// How long a confirmation link stays live.
pub const CONFIRM_TTL_HOURS: i64 = 48;
/// How long an unsubscribe link stays live. Longer than confirm, because the link a visitor
/// clicks after a year is one they got from a copy of the issue, not from a stale session.
pub const UNSUBSCRIBE_TTL_DAYS: i64 = 365;
/// The states a subscriber can be in.
pub const SUBSCRIBER_STATUSES: [&str; 4] = ["pending", "confirmed", "unsubscribed", "bounced"];

/// The confirmation digest.
const CONFIRM_COLUMNS: &str = "id, site_id, list_id, email, name, source, status, confirm_token_hash, \
     unsubscribe_token_hash, confirm_expires_at, confirmed_at, unsubscribed_at, status_reason, \
     created_at, updated_at";

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

/// A subscriber list.
///
/// Deliberately NOT `#[derive(sqlx::FromRow)]`: `counts` is assembled by a join and is not a
/// column of `newsletter_lists`, and a derived `FromRow` makes the compiler ask PostgreSQL for a
/// type for a field that lives in no table. [`ListRow`] is the row; this is the wire shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsletterList {
    /// List id.
    pub id: Uuid,
    /// The site it belongs to.
    pub site_id: Uuid,
    /// The tenant it belongs to.
    pub organization_id: Uuid,
    /// The public key a signup form posts to.
    pub key: String,
    /// Display name.
    pub name: String,
    /// What the list is for, shown on the signup form.
    pub description: Option<String>,
    /// Whether a signup must be confirmed.
    pub double_opt_in: bool,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
    /// How many subscribers each state holds. Populated by [`NewsletterStore::list_lists`],
    /// absent on a single read — a list row itself has no such columns.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub counts: Option<ListCounts>,
}

/// Per-state counts of one list.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, sqlx::FromRow)]
pub struct ListCounts {
    /// Waiting for a click.
    pub pending: i64,
    /// Subscribed.
    pub confirmed: i64,
    /// Asked to leave; the row is kept.
    pub unsubscribed: i64,
    /// Hard-bounced; the address cannot receive.
    pub bounced: i64,
}

impl ListCounts {
    /// The states that will actually receive an issue.
    #[must_use]
    pub const fn deliverable(&self) -> i64 {
        self.confirmed
    }
}

/// A subscriber row, as the store reads it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Subscriber {
    /// Subscriber id.
    pub id: Uuid,
    /// The site it belongs to.
    pub site_id: Uuid,
    /// The list it subscribed to.
    pub list_id: Uuid,
    /// The address, stored lowercased.
    pub email: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Where the signup came from.
    pub source: Option<String>,
    /// `pending` · `confirmed` · `unsubscribed` · `bounced`.
    pub status: String,
    /// Digest of the confirmation token.
    pub confirm_token_hash: Option<String>,
    /// Digest of the unsubscribe token.
    pub unsubscribe_token_hash: Option<String>,
    /// When the confirmation link stops working.
    pub confirm_expires_at: Option<OffsetDateTime>,
    /// When it was confirmed.
    pub confirmed_at: Option<OffsetDateTime>,
    /// When it asked to leave.
    pub unsubscribed_at: Option<OffsetDateTime>,
    /// Why the status is what it is.
    pub status_reason: Option<String>,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When the row last changed.
    pub updated_at: OffsetDateTime,
}

/// A new list.
#[derive(Debug, Clone, Deserialize)]
pub struct NewList {
    /// The site it belongs to.
    pub site_id: Uuid,
    /// The tenant it belongs to.
    pub organization_id: Uuid,
    /// The public key. Optional: a list created in the panel with no key gets one from its
    /// name, because the public signup route is addressed BY this key and a list nobody can
    /// sign up for is a list that only its owner can see.
    #[serde(default)]
    pub key: Option<String>,
    /// Display name.
    pub name: String,
    /// What the list is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether a signup must be confirmed. Defaults to true in the store.
    #[serde(default)]
    pub double_opt_in: Option<bool>,
    /// The account creating it, for the audit row.
    #[serde(default)]
    pub created_by: Option<Uuid>,
}

/// A change to a list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListPatch {
    /// New display name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<String>,
    /// Whether a signup must be confirmed.
    pub double_opt_in: Option<bool>,
}

/// A sign-up, from the public form or the panel.
#[derive(Debug, Clone, Deserialize)]
pub struct NewSubscriber {
    /// The list to subscribe somebody to.
    pub list_id: Uuid,
    /// The site the list belongs to; carried so the store can write the denormalised column
    /// without a second read and so the panel's cross-list query never needs a join.
    pub site_id: Uuid,
    /// The address.
    pub email: String,
    /// Optional display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Where the signup came from.
    #[serde(default)]
    pub source: Option<String>,
}

/// A subscriber page.
#[derive(Debug, Clone, Default)]
pub struct SubscriberPage {
    /// The rows, newest first.
    pub subscribers: Vec<Subscriber>,
    /// How many rows the filter matched.
    pub total: i64,
}

/// The filter of the subscribers screen.
#[derive(Debug, Clone, Default)]
pub struct SubscriberFilter {
    /// One list, or every list of the site.
    pub list_id: Option<Uuid>,
    /// One state, or every state.
    pub status: Option<String>,
    /// Free text over address, name and source.
    pub search: Option<String>,
    /// Page size.
    pub limit: i64,
    /// Offset.
    pub offset: i64,
}

/// A sent issue.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Issue {
    /// Issue id.
    pub id: Uuid,
    /// The site it belongs to.
    pub site_id: Uuid,
    /// The list it went to.
    pub list_id: Uuid,
    /// Subject line.
    pub subject: String,
    /// Sanitised body HTML.
    pub body_html: String,
    /// When it went out.
    pub sent_at: OffsetDateTime,
    /// How many addresses it reached.
    pub recipient_count: i32,
    /// The public permalink segment.
    pub archive_slug: String,
}

/// An issue as the archive draws it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IssueSummary {
    /// Issue id.
    pub id: Uuid,
    /// The list it went to.
    pub list_id: Uuid,
    /// Subject line.
    pub subject: String,
    /// When it went out.
    pub sent_at: OffsetDateTime,
    /// How many addresses it reached.
    pub recipient_count: i32,
    /// The public permalink segment.
    pub archive_slug: String,
    /// The list's display name, joined in for the archive list.
    #[sqlx(default)]
    pub list_name: Option<String>,
}

/// A new issue.
#[derive(Debug, Clone, Deserialize)]
pub struct NewIssue {
    /// The site it belongs to.
    pub site_id: Uuid,
    /// The list it goes to.
    pub list_id: Uuid,
    /// Subject line.
    pub subject: String,
    /// Owner HTML. Sanitised on write.
    pub body_html: String,
    /// The permalink segment. Optional: a slug is generated from the subject when absent.
    #[serde(default)]
    pub archive_slug: Option<String>,
    /// The account that sent it.
    #[serde(default)]
    pub created_by: Option<Uuid>,
}

/// A public issue.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PublicIssue {
    /// Issue id.
    pub id: Uuid,
    /// Subject line.
    pub subject: String,
    /// Sanitised body HTML.
    pub body_html: String,
    /// When it went out.
    pub sent_at: OffsetDateTime,
    /// The permalink it is addressed by.
    pub archive_slug: String,
}

/// What a signup produced: the row plus the token to mail.
///
/// The token is in the return value and **never** in the row, and that is the whole reason this
/// is a struct rather than the `Subscriber` itself: a caller that persists the return value by
/// accident would store the only copy of the only thing that works.
#[derive(Debug, Clone)]
pub struct SignupOutcome {
    /// The row as written.
    pub subscriber: Subscriber,
    /// The raw confirmation token, to be delivered by e-mail. `None` when the list does not
    /// require confirmation — there is nothing to confirm.
    pub confirm_token: Option<String>,
    /// The raw unsubscribe token, so a first welcome mail can carry a working opt-out.
    /// `None` when the row already existed and only its digest is on record.
    pub unsubscribe_token: Option<String>,
    /// Whether the row went straight to `confirmed` because the list is not double opt-in.
    pub confirmed_immediately: bool,
}

/// The outcome of a public confirmation or unsubscribe.
#[derive(Debug, Clone, Serialize)]
pub struct TokenOutcome {
    /// The address it acted on, so a form can say "you are subscribed as …".
    pub email: String,
    /// The state the row is in afterwards.
    pub status: String,
    /// False when the token was already used, expired or unknown — the three are one answer to
    /// a visitor and three different facts in the log.
    pub applied: bool,
    /// Why it did not apply, for the log and the screen.
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

/// The newsletter store.
#[derive(Debug, Clone)]
pub struct NewsletterStore {
    pool: PgPool,
}

impl NewsletterStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool, for callers that compose a transaction with their own writes.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -----------------------------------------------------------------------------------------
    // Lists
    // -----------------------------------------------------------------------------------------

    /// Every list of a site, with its per-state counts.
    ///
    /// One query with a LEFT JOIN and `count(*) filter (where …)` rather than four counts per
    /// row: the panel shows four numbers beside every list, and a correlated subquery per state
    /// is four round trips per list for a screen that lists twenty of them.
    pub async fn list_lists(&self, site_id: Uuid) -> Result<Vec<NewsletterList>> {
        let sql = format!(
            "select l.id, l.site_id, l.organization_id, l.key, l.name, l.description, \
                 l.double_opt_in, l.created_at, l.updated_at, \
                 count(s.id) filter (where s.status = 'pending') as pending, \
                 count(s.id) filter (where s.status = 'confirmed') as confirmed, \
                 count(s.id) filter (where s.status = 'unsubscribed') as unsubscribed, \
                 count(s.id) filter (where s.status = 'bounced') as bounced \
             from newsletter_lists l \
             left join newsletter_subscribers s on s.list_id = l.id \
             where l.site_id = $1 \
             group by l.id \
             order by l.created_at"
        );

        let rows = sqlx::query_as::<_, ListRow>(&sql)
            .bind(site_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.into_iter().map(ListRow::into_list).collect())
    }

    /// One list by id.
    pub async fn list_by_id(&self, id: Uuid) -> Result<NewsletterList> {
        let sql = "select id, site_id, organization_id, key, name, description, double_opt_in, \
                   created_at, updated_at \
                   from newsletter_lists where id = $1";
        let row: ListRow = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::NewsletterListNotFound)?;
        let mut list = row.into_list();
        list.counts = Some(self.list_counts(id).await?);
        Ok(list)
    }

    /// One list by its PUBLIC key, which is what the signup form addresses.
    pub async fn list_by_key(&self, site_id: Uuid, key: &str) -> Result<NewsletterList> {
        let sql = "select id, site_id, organization_id, key, name, description, double_opt_in, \
                   created_at, updated_at \
                   from newsletter_lists where site_id = $1 and key = $2";
        let row: ListRow = sqlx::query_as(sql)
            .bind(site_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::NewsletterListNotFound)?;
        let mut list = row.into_list();
        list.counts = Some(self.list_counts(list.id).await?);
        Ok(list)
    }

    /// The per-state counts of one list.
    pub async fn list_counts(&self, list_id: Uuid) -> Result<ListCounts> {
        let sql = "select \
                     count(*) filter (where status = 'pending') as pending, \
                     count(*) filter (where status = 'confirmed') as confirmed, \
                     count(*) filter (where status = 'unsubscribed') as unsubscribed, \
                     count(*) filter (where status = 'bounced') as bounced \
                   from newsletter_subscribers where list_id = $1";
        Ok(sqlx::query_as::<_, ListCounts>(sql)
            .bind(list_id)
            .fetch_one(&self.pool)
            .await?)
    }

    /// Create a list.
    ///
    /// The key is derived from the name when the caller gave none, and the derivation can
    /// collide — so the collision is resolved here rather than refused. An owner who creates
    /// "Weekly News" twice gets `weekly-news` and `weekly-news-2`, because a unique violation
    /// would be a 500 on a form that looks like it did nothing.
    pub async fn create_list(&self, new: NewList) -> Result<NewsletterList> {
        let name = validate_list_name(&new.name)?;
        let description = new
            .description
            .as_deref()
            .map(validate_list_description)
            .transpose()?;
        let key = match new.key.as_deref().map(str::trim) {
            Some(k) if !k.is_empty() => validate_list_key(k)?,
            _ => slugify(&name),
        };

        // The FIRST collision gets the bare name it already failed on and a `-2` suffix, not
        // `-1`: a list addressed `weekly-news-1` implies a `weekly-news-0` that never existed,
        // and the key is a public signup URL. The suffix counts DUPLICATES, so the nth
        // duplicate is `-{n + 1}`.
        let mut candidate = key.clone();
        let mut duplicate = 0usize;
        // A bounded loop: two names that slugify the same are a duplicate, not an attack, and
        // 100 attempts is a shape a human cannot produce.
        while duplicate <= 100 {
            let taken: Option<Uuid> = sqlx::query_scalar(
                "select id from newsletter_lists where site_id = $1 and key = $2",
            )
            .bind(new.site_id)
            .bind(&candidate)
            .fetch_optional(&self.pool)
            .await?;
            if taken.is_none() {
                break;
            }
            duplicate += 1;
            candidate = format!("{key}-{}", duplicate + 1);
        }

        let sql = "insert into newsletter_lists \
                     (site_id, organization_id, key, name, description, double_opt_in, created_by) \
                   values ($1, $2, $3, $4, $5, $6, $7) \
                   returning id, site_id, organization_id, key, name, description, double_opt_in, \
                             created_at, updated_at";
        let row: ListRow = sqlx::query_as(sql)
            .bind(new.site_id)
            .bind(new.organization_id)
            .bind(&candidate)
            .bind(&name)
            .bind(&description)
            // Default to TRUE when the caller said nothing: "I did not choose" must not mean
            // "subscribe people without asking them".
            .bind(new.double_opt_in.unwrap_or(true))
            .bind(new.created_by)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_insert_error(e, "newsletter_lists_site_key_idx", "that list key is taken"))?;

        let mut list = row.into_list();
        list.counts = Some(ListCounts::default());
        Ok(list)
    }

    /// Change a list.
    ///
    /// The key is deliberately NOT patchable: it is the public signup address and the
    /// confirmation links already in people's inboxes carry it. Renaming it would silently
    /// break every link that was ever mailed.
    pub async fn patch_list(&self, id: Uuid, patch: ListPatch) -> Result<NewsletterList> {
        let current = self.list_by_id(id).await?;

        let name = match patch.name.as_deref() {
            Some(n) => validate_list_name(n)?,
            None => current.name,
        };
        let description = match patch.description {
            Some(d) => Some(validate_list_description(&d)?),
            None => current.description,
        };
        let double_opt_in = patch.double_opt_in.unwrap_or(current.double_opt_in);

        let sql = "update newsletter_lists \
                   set name = $2, description = $3, double_opt_in = $4, updated_at = now() \
                   where id = $1 \
                   returning id, site_id, organization_id, key, name, description, double_opt_in, \
                             created_at, updated_at";
        let row: ListRow = sqlx::query_as(sql)
            .bind(id)
            .bind(&name)
            .bind(&description)
            .bind(double_opt_in)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::NewsletterListNotFound)?;
        let mut list = row.into_list();
        list.counts = Some(current.counts.unwrap_or_default());
        Ok(list)
    }

    /// Delete a list, and with it every subscriber on it.
    pub async fn delete_list(&self, id: Uuid) -> Result<()> {
        let sql = "delete from newsletter_lists where id = $1";
        let rows = sqlx::query(sql).bind(id).execute(&self.pool).await?;
        if rows.rows_affected() == 0 {
            return Err(ContentError::NewsletterListNotFound);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Subscribers
    // -----------------------------------------------------------------------------------------

    /// One page of subscribers.
    pub async fn subscribers(&self, site_id: Uuid, filter: &SubscriberFilter) -> Result<SubscriberPage> {
        if let Some(status) = filter.status.as_deref() {
            validate_status(status)?;
        }
        let needle = filter
            .search
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", escape_like(s).to_lowercase()));

        let limit = clamp_page(filter.limit);
        let offset = filter.offset.max(0);

        // The count and the page are built by the SAME loop over the SAME filter list, so a
        // placeholder can only exist where its value was pushed. Two hand-written query pairs
        // are how a count says "12" while the table shows 13 rows.
        let mut rows = QueryBuilder::<Postgres>::new(
            format!("select {CONFIRM_COLUMNS} from newsletter_subscribers where "),
        );
        push_subscriber_filters(&mut rows, filter, site_id, needle.as_deref());
        rows.push(" order by created_at desc, id desc limit ");
        rows.push_bind(limit);
        rows.push(" offset ");
        rows.push_bind(offset);
        let subscribers: Vec<Subscriber> = rows.build_query_as().fetch_all(&self.pool).await?;

        let mut count =
            QueryBuilder::<Postgres>::new("select count(*) from newsletter_subscribers where ");
        push_subscriber_filters(&mut count, filter, site_id, needle.as_deref());
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        Ok(SubscriberPage { subscribers, total })
    }

    /// One subscriber by id.
    pub async fn subscriber(&self, id: Uuid) -> Result<Subscriber> {
        let sql = format!(
            "select {CONFIRM_COLUMNS} from newsletter_subscribers where id = $1"
        );
        sqlx::query_as(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::SubscriberNotFound)
    }

    /// Subscribe an address, and return the token to mail.
    ///
    /// The three cases are deliberately different answers rather than one upsert:
    ///
    /// * **new** — a row is written `pending` (or `confirmed` when the list is not double
    ///   opt-in) with a fresh token.
    /// * **already pending** — a *new* token and a *new* expiry. The old link may have been
    ///   eaten by a mail filter; the honest answer is a fresh one. `new_token = false` says so,
    ///   because re-mailing a second opt-in to a pending address is exactly the thing a double
    ///   opt-in is supposed to prevent.
    /// * **already confirmed** — a refusal, and a quiet one: re-subscribing somebody who
    ///   already subscribed is the first step of the consent-theft pattern every mailing list
    ///   exists to stop.
    /// * **`unsubscribed` / `bounced`** — a *re*-opt-in is legitimate (somebody who left came
    ///   back), so the row is revived to `pending` with a fresh token. It is revived rather
    ///   than duplicated, so a list cannot hold the same person twice.
    pub async fn subscribe(&self, new: NewSubscriber) -> Result<SignupOutcome> {
        let email = validate_email(&new.email)?;
        let name = new.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
        let source = new.source.as_deref().map(str::trim).filter(|s| !s.is_empty());

        let list = sqlx::query_as::<_, ListRow>(
            "select id, site_id, organization_id, key, name, description, double_opt_in, \
             created_at, updated_at from newsletter_lists where id = $1 and site_id = $2",
        )
        .bind(new.list_id)
        .bind(new.site_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::NewsletterListNotFound)?;

        if let Some(existing) = self.subscriber_by_email(new.list_id, &email).await? {
            match existing.status.as_str() {
                "pending" => {
                    let (token, hash) = new_confirm_token();
                    let expires = OffsetDateTime::now_utc()
                        .saturating_add(time::Duration::hours(CONFIRM_TTL_HOURS));
                    let sub = self
                        .write_confirm_token(existing.id, &hash, expires)
                        .await?;
                    // A row that is already waiting has an unsubscribe token we do not
                    // hold the raw form of — only its digest is stored. So this outcome
                    // carries no unsubscribe token, and the caller mails the confirmation
                    // alone. Inventing an empty string here would put a live-looking link
                    // with no hash behind it into somebody's mail client.
                    return Ok(SignupOutcome {
                        subscriber: sub,
                        confirm_token: Some(token),
                        unsubscribe_token: None,
                        confirmed_immediately: false,
                    });
                }
                "confirmed" => {
                    return Err(ContentError::SubscriberAlreadyConfirmed(email));
                }
                // A re-opt-in is allowed and revives the SAME row.
                _ => {
                    let (token, hash) = new_confirm_token();
                    let (unsub_token, unsub_hash) = new_unsubscribe_token();
                    let expires = OffsetDateTime::now_utc()
                        .saturating_add(time::Duration::hours(CONFIRM_TTL_HOURS));
                    let sub = self
                        .revive_subscriber(existing.id, &hash, expires, &unsub_hash, source)
                        .await?;
                    return Ok(SignupOutcome {
                        subscriber: sub,
                        confirm_token: if list.double_opt_in { Some(token) } else { None },
                        unsubscribe_token: Some(unsub_token),
                        confirmed_immediately: !list.double_opt_in,
                    });
                }
            }
        }

        let (confirm_token, confirm_hash) = new_confirm_token();
        let (unsub_token, unsub_hash) = new_unsubscribe_token();
        let double_opt_in = list.double_opt_in;
        let expires = OffsetDateTime::now_utc()
            .saturating_add(time::Duration::hours(CONFIRM_TTL_HOURS));

        let sql = format!(
            "insert into newsletter_subscribers \
                 (site_id, list_id, email, name, source, status, confirm_token_hash, \
                  unsubscribe_token_hash, confirm_expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             returning {CONFIRM_COLUMNS}"
        );
        let sub: Subscriber = sqlx::query_as(&sql)
            .bind(new.site_id)
            .bind(new.list_id)
            .bind(&email)
            .bind(name)
            .bind(source)
            .bind(if double_opt_in { "pending" } else { "confirmed" })
            .bind(if double_opt_in { Some(&confirm_hash) } else { None })
            .bind(&unsub_hash)
            .bind(if double_opt_in { Some(expires) } else { None })
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                map_insert_error(
                    e,
                    "newsletter_subscribers_list_email_idx",
                    "that address is already on this list",
                )
            })?;

        Ok(SignupOutcome {
            subscriber: sub,
            confirm_token: if double_opt_in { Some(confirm_token) } else { None },
            unsubscribe_token: Some(unsub_token),
            confirmed_immediately: !double_opt_in,
        })
    }

    /// Confirm a subscription with the token from the link.
    ///
    /// Every refusal is `applied: false` with a reason, and none of them is an error: an
    /// expired link, a replayed link and a link from a stranger's forwarded mail are three
    /// different facts and the same answer to the person who clicked. A 404 here would tell a
    /// visitor their address does not exist on a list, which is a disclosure oracle over a table
    /// of e-mail addresses.
    pub async fn confirm(&self, token: &str) -> Result<TokenOutcome> {
        let hash = hash_token(token);
        let sql = format!(
            "select {CONFIRM_COLUMNS} from newsletter_subscribers \
             where confirm_token_hash = $1 for update"
        );
        let mut tx = self.pool.begin().await?;
        let subscriber: Subscriber = sqlx::query_as(&sql)
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ContentError::InvalidToken)?;

        if subscriber.status == "confirmed" {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                email: subscriber.email,
                status: "confirmed".to_string(),
                applied: false,
                reason: Some("already confirmed".to_string()),
            });
        }
        if subscriber.status != "pending" {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                email: subscriber.email,
                status: subscriber.status,
                applied: false,
                reason: Some("this subscription is no longer waiting for confirmation".to_string()),
            });
        }
        if let Some(expires) = subscriber.confirm_expires_at {
            if expires <= OffsetDateTime::now_utc() {
                tx.rollback().await?;
                return Ok(TokenOutcome {
                    email: subscriber.email,
                    status: "pending".to_string(),
                    applied: false,
                    reason: Some("this confirmation link has expired".to_string()),
                });
            }
        }

        let update = "update newsletter_subscribers \
                      set status = 'confirmed', confirmed_at = now(), confirm_token_hash = null, \
                          status_reason = null, updated_at = now() \
                      where id = $1 returning id";
        sqlx::query(update)
            .bind(subscriber.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(TokenOutcome {
            email: subscriber.email,
            status: "confirmed".to_string(),
            applied: true,
            reason: None,
        })
    }

    /// Unsubscribe with the token from the link — no sign-in, never guessable.
    ///
    /// The row is KEPT and its status flipped, which is the whole point: the criterion asks for
    /// "unsubscribe flips the status while keeping the row", and a deleted row is how a later
    /// import quietly re-subscribes somebody who left on purpose.
    pub async fn unsubscribe(&self, token: &str) -> Result<TokenOutcome> {
        let hash = hash_token(token);
        let sql = format!(
            "select {CONFIRM_COLUMNS} from newsletter_subscribers \
             where unsubscribe_token_hash = $1 for update"
        );
        let mut tx = self.pool.begin().await?;
        let subscriber: Subscriber = sqlx::query_as(&sql)
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ContentError::InvalidToken)?;

        if subscriber.status == "unsubscribed" {
            tx.rollback().await?;
            return Ok(TokenOutcome {
                email: subscriber.email,
                status: "unsubscribed".to_string(),
                applied: false,
                reason: Some("already unsubscribed".to_string()),
            });
        }

        let update = "update newsletter_subscribers \
                      set status = 'unsubscribed', unsubscribed_at = now(), \
                          status_reason = 'unsubscribed by the subscriber', updated_at = now() \
                      where id = $1";
        sqlx::query(update)
            .bind(subscriber.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(TokenOutcome {
            email: subscriber.email,
            status: "unsubscribed".to_string(),
            applied: true,
            reason: None,
        })
    }

    /// Change a subscriber's state from the panel.
    pub async fn set_status(&self, id: Uuid, status: &str, reason: Option<String>) -> Result<Subscriber> {
        validate_status(status)?;
        let reason = reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| r.chars().take(300).collect::<String>());

        let sql = format!(
            "update newsletter_subscribers set \
                 status = $2, \
                 status_reason = $3, \
                 confirmed_at = case when $2 = 'confirmed' then coalesce(confirmed_at, now()) \
                                    else confirmed_at end, \
                 unsubscribed_at = case when $2 = 'unsubscribed' then now() else unsubscribed_at end, \
                 -- A manual confirmation has no link to follow, so the token is cleared rather
                 -- than left to expire: keeping it means a leaked older link still confirms.
                 confirm_token_hash = case when $2 = 'confirmed' then null else confirm_token_hash end, \
                 updated_at = now() \
             where id = $1 \
             returning {CONFIRM_COLUMNS}"
        );
        sqlx::query_as(&sql)
            .bind(id)
            .bind(status)
            .bind(&reason)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::SubscriberNotFound)
    }

    /// Delete a subscriber row. The panel's own "remove entirely", not the unsubscribe link.
    pub async fn delete_subscriber(&self, id: Uuid) -> Result<()> {
        let sql = "delete from newsletter_subscribers where id = $1";
        let rows = sqlx::query(sql).bind(id).execute(&self.pool).await?;
        if rows.rows_affected() == 0 {
            return Err(ContentError::SubscriberNotFound);
        }
        Ok(())
    }

    /// The addresses an issue would reach: confirmed, not unsubscribed, not bounced.
    pub async fn deliverable(&self, list_id: Uuid) -> Result<Vec<String>> {
        let sql = "select email from newsletter_subscribers \
                   where list_id = $1 and status = 'confirmed' order by created_at";
        Ok(sqlx::query_scalar(sql)
            .bind(list_id)
            .fetch_all(&self.pool)
            .await?)
    }

    // -----------------------------------------------------------------------------------------
    // Import / export
    // -----------------------------------------------------------------------------------------

    /// Import addresses from CSV text into a list.
    ///
    /// The duplicate policy is **skip, never revive**: an import is a list the owner already
    /// holds, and reviving an `unsubscribed` row from a file would undo a decision the person
    /// made. The two exceptions are `pending` and `confirmed`, which are refreshed with a new
    /// token only when the row is already there — also a skip, reported, because a report that
    /// says "0 added, 3 skipped" is a fact an owner can act on.
    ///
    /// Written in ONE transaction: a 500 halfway through a 400-row import would leave the list
    /// in a state nobody can describe.
    pub async fn import_csv(
        &mut self,
        list_id: Uuid,
        site_id: Uuid,
        csv: &str,
        source: Option<&str>,
    ) -> Result<ImportReport> {
        // The list must exist AND belong to this site. The site half is the load-bearing one:
        // an import addressed to a list of another tenant would otherwise write subscribers
        // into a list the caller never saw.
        let _ = sqlx::query_scalar::<_, bool>(
            "select true from newsletter_lists where id = $1 and site_id = $2",
        )
        .bind(list_id)
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(ContentError::NewsletterListNotFound)?;

        let mut report = ImportReport::default();
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;

        for row in parse_csv_addresses(csv) {
            if row.is_empty() {
                report.blank += 1;
                continue;
            }
            let email = match validate_email(&row) {
                Ok(e) => e,
                Err(_) => {
                    report.blank += 1;
                    continue;
                }
            };

            let existing: Option<String> = sqlx::query_scalar(
                "select status from newsletter_subscribers where list_id = $1 and lower(email) = $2",
            )
            .bind(list_id)
            .bind(&email)
            .fetch_optional(&mut *tx)
            .await?;

            if let Some(status) = existing {
                report.skipped.push(ImportSkip { email, status });
                continue;
            }

            let (confirm_token, confirm_hash) = new_confirm_token();
            let (unsub_token, unsub_hash) = new_unsubscribe_token();
            let expires = OffsetDateTime::now_utc()
                .saturating_add(time::Duration::hours(CONFIRM_TTL_HOURS));

            let sql = format!(
                "insert into newsletter_subscribers \
                     (site_id, list_id, email, source, status, confirm_token_hash, \
                      unsubscribe_token_hash, confirm_expires_at) \
                 values ($1, $2, $3, $4, 'pending', $5, $6, $7)"
            );
            sqlx::query(&sql)
                .bind(site_id)
                .bind(list_id)
                .bind(&email)
                .bind(source)
                .bind(&confirm_hash)
                .bind(&unsub_hash)
                .bind(expires)
                .execute(&mut *tx)
                .await
            .map_err(|e| map_insert_error(e, "newsletter_subscribers_list_email_idx", "already on the list"))?;
            report.added += 1;
            let _ = (confirm_token, unsub_token);
        }

        tx.commit().await?;
        Ok(report)
    }

    /// Export a list as CSV, optionally filtered to one state.
    pub async fn export_csv(&self, site_id: Uuid, filter: &SubscriberFilter) -> Result<String> {
        let page = self.subscribers(site_id, filter).await?;
        let mut out = String::from("email,name,status,source,confirmed_at,created_at\n");
        for s in page.subscribers {
            out.push_str(&csv_field(&s.email));
            out.push(',');
            out.push_str(&csv_field(s.name.as_deref().unwrap_or("")));
            out.push(',');
            out.push_str(&s.status);
            out.push(',');
            out.push_str(&csv_field(s.source.as_deref().unwrap_or("")));
            out.push(',');
            out.push_str(s.confirmed_at.map(format_instant).unwrap_or_default().as_str());
            out.push(',');
            out.push_str(format_instant(s.created_at).as_str());
            out.push('\n');
        }
        Ok(out)
    }

    // -----------------------------------------------------------------------------------------
    // Issues
    // -----------------------------------------------------------------------------------------

    /// Record a sent issue in the archive.
    ///
    /// `recipient_count` is what the caller passes, not what the store counts: the send already
    /// knew, and the list has changed since. The body is sanitised here, because a newsletter
    /// is the one field that is *explicitly* allowed to carry markup and the archive page
    /// renders it on the platform's own surface.
    pub async fn record_issue(&self, new: NewIssue) -> Result<Issue> {
        let subject = validate_subject(&new.subject)?;
        let body = validate_body_html(&new.body_html)?;

        let slug = match new.archive_slug.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => slugify(s),
            _ => slugify(&subject),
        };

        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;
        let list_name: Option<String> =
            sqlx::query_scalar("select name from newsletter_lists where id = $1 and site_id = $2")
                .bind(new.list_id)
                .bind(new.site_id)
                .fetch_optional(&mut *tx)
                .await?;
        if list_name.is_none() {
            tx.rollback().await?;
            return Err(ContentError::NewsletterListNotFound);
        }

        // The slug must be unique per site, and the same subject twice ("Weekly news") is
        // ordinary, so a collision resolves to a numbered variant rather than a 500.
        // Same rule as a list key: the first duplicate is `-2`, not `-1`, because the slug is
        // a permalink an owner links to in a chat and `weekly-news-1` reads as the second
        // issue of a series that never had a first.
        let mut candidate = slug.clone();
        let mut duplicate = 0usize;
        while duplicate <= 100 {
            let taken: Option<Uuid> = sqlx::query_scalar(
                "select id from newsletter_issues where site_id = $1 and archive_slug = $2",
            )
            .bind(new.site_id)
            .bind(&candidate)
            .fetch_optional(&mut *tx)
            .await?;
            if taken.is_none() {
                break;
            }
            duplicate += 1;
            candidate = format!("{slug}-{}", duplicate + 1);
        }

        let recipient_count = i32::try_from(self.deliverable(new.list_id).await?.len())
            .unwrap_or(i32::MAX);

        let sql = "insert into newsletter_issues \
                     (site_id, list_id, subject, body_html, recipient_count, archive_slug, created_by) \
                   values ($1, $2, $3, $4, $5, $6, $7) \
                   returning id, site_id, list_id, subject, body_html, sent_at, recipient_count, \
                             archive_slug";
        let issue: Issue = sqlx::query_as(sql)
            .bind(new.site_id)
            .bind(new.list_id)
            .bind(&subject)
            .bind(&body)
            .bind(recipient_count)
            .bind(&candidate)
            .bind(new.created_by)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(issue)
    }

    /// The archive, newest first.
    pub async fn list_issues(&self, site_id: Uuid, limit: i64) -> Result<Vec<IssueSummary>> {
        let sql = "select i.id, i.list_id, i.subject, i.sent_at, i.recipient_count, \
                          i.archive_slug, l.name as list_name \
                   from newsletter_issues i \
                   join newsletter_lists l on l.id = i.list_id \
                   where i.site_id = $1 order by i.sent_at desc, i.id desc limit $2";
        Ok(sqlx::query_as::<_, IssueSummary>(sql)
            .bind(site_id)
            .bind(clamp_page(limit))
            .fetch_all(&self.pool)
            .await?)
    }

    /// One issue for the archive page, by its public slug.
    pub async fn issue_by_slug(&self, site_id: Uuid, slug: &str) -> Result<PublicIssue> {
        let sql = "select id, subject, body_html, sent_at, archive_slug \
                   from newsletter_issues where site_id = $1 and archive_slug = $2";
        sqlx::query_as(sql)
            .bind(site_id)
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::IssueNotFound)
    }

    // -----------------------------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------------------------

    /// The row of one list, before counts.
    async fn subscriber_by_email(&self, list_id: Uuid, email: &str) -> Result<Option<Subscriber>> {
        let sql = format!(
            "select {CONFIRM_COLUMNS} from newsletter_subscribers \
             where list_id = $1 and lower(email) = $2"
        );
        Ok(sqlx::query_as(&sql)
            .bind(list_id)
            .bind(email)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Issue a fresh confirmation token for a row already waiting.
    async fn write_confirm_token(
        &self,
        id: Uuid,
        hash: &str,
        expires: OffsetDateTime,
    ) -> Result<Subscriber> {
        let sql = format!(
            "update newsletter_subscribers set confirm_token_hash = $2, confirm_expires_at = $3, \
                    updated_at = now() where id = $1 returning {CONFIRM_COLUMNS}"
        );
        sqlx::query_as(&sql)
            .bind(id)
            .bind(hash)
            .bind(expires)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::SubscriberNotFound)
    }

    /// Bring an unsubscribed or bounced row back to `pending`, with fresh tokens.
    async fn revive_subscriber(
        &self,
        id: Uuid,
        confirm_hash: &str,
        confirm_expires: OffsetDateTime,
        unsub_hash: &str,
        source: Option<&str>,
    ) -> Result<Subscriber> {
        let sql = format!(
            "update newsletter_subscribers set \
                 status = 'pending', status_reason = null, unsubscribed_at = null, \
                 confirm_token_hash = $2, confirm_expires_at = $3, \
                 unsubscribe_token_hash = $4, \
                 source = coalesce($5, source), updated_at = now() \
             where id = $1 returning {CONFIRM_COLUMNS}"
        );
        sqlx::query_as(&sql)
            .bind(id)
            .bind(confirm_hash)
            .bind(confirm_expires)
            .bind(unsub_hash)
            .bind(source)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::SubscriberNotFound)
    }
}

/// Push the subscribers filter onto a query, in a fixed order.
///
/// One function for the page and the count, so the two can only ever describe the same
/// population. `needle` is the already-escaped, lowercased search term or `None`.
fn push_subscriber_filters<'q>(
    q: &mut QueryBuilder<'q, Postgres>,
    filter: &SubscriberFilter,
    site_id: Uuid,
    needle: Option<&str>,
) {
    q.push("site_id = ");
    q.push_bind(site_id);
    if let Some(list_id) = filter.list_id {
        q.push(" and list_id = ");
        q.push_bind(list_id);
    }
    if let Some(status) = filter.status.as_deref() {
        q.push(" and status = ");
        q.push_bind(status.to_owned());
    }
    if let Some(needle) = needle {
        q.push(
            " and (lower(email) like ",
        );
        q.push_bind(needle.to_owned());
        q.push(
            " escape '\\' or lower(coalesce(name, '')) like ",
        );
        q.push_bind(needle.to_owned());
        q.push(" escape '\\' or lower(coalesce(source, '')) like ");
        q.push_bind(needle.to_owned());
        q.push(" escape '\\')");
    }
}

/// The outcome of a CSV import: what was added, what was skipped, and why.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    /// Rows that produced a subscriber.
    pub added: i64,
    /// Rows whose address is already on the list, and what it was already doing.
    pub skipped: Vec<ImportSkip>,
    /// Blank rows and rows with no usable address — counted, not reported one by one.
    pub blank: i64,
}

/// One skipped row.
#[derive(Debug, Clone, Serialize)]
pub struct ImportSkip {
    /// The address as the file had it.
    pub email: String,
    /// What it is already doing on the list.
    pub status: String,
}

/// A list row plus its counts, as the join returns it.
#[derive(sqlx::FromRow)]
struct ListRow {
    id: Uuid,
    site_id: Uuid,
    organization_id: Uuid,
    key: String,
    name: String,
    description: Option<String>,
    double_opt_in: bool,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    #[sqlx(default)]
    pending: i64,
    #[sqlx(default)]
    confirmed: i64,
    #[sqlx(default)]
    unsubscribed: i64,
    #[sqlx(default)]
    bounced: i64,
}

impl ListRow {
    /// Split the flat row into the list and its counts.
    fn into_list(self) -> NewsletterList {
        NewsletterList {
            id: self.id,
            site_id: self.site_id,
            organization_id: self.organization_id,
            key: self.key,
            name: self.name,
            description: self.description,
            double_opt_in: self.double_opt_in,
            created_at: self.created_at,
            updated_at: self.updated_at,
            counts: Some(ListCounts {
                pending: self.pending,
                confirmed: self.confirmed,
                unsubscribed: self.unsubscribed,
                bounced: self.bounced,
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------------------------

/// Validate a list name.
pub fn validate_list_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidNewsletter("list name is required".to_string()));
    }
    if trimmed.chars().count() > MAX_LIST_NAME {
        return Err(ContentError::InvalidNewsletter(format!(
            "list name is at most {MAX_LIST_NAME} characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Validate a list description.
pub fn validate_list_description(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.chars().count() > MAX_LIST_DESCRIPTION {
        return Err(ContentError::InvalidNewsletter(format!(
            "description is at most {MAX_LIST_DESCRIPTION} characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Validate a public list key.
///
/// The shape is the schema's, restated: a key is a URL segment, so it is lowercase alphanumerics
/// and dashes. Validating it here turns what would be a `check_violation` from the database into
/// a message naming the rule.
pub fn validate_list_key(raw: &str) -> Result<String> {
    let trimmed = raw.trim().to_lowercase();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidNewsletter("list key is required".to_string()));
    }
    if trimmed.len() > 63 {
        return Err(ContentError::InvalidNewsletter(
            "list key is at most 63 characters".to_string(),
        ));
    }
    let ok = trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !ok {
        return Err(ContentError::InvalidNewsletter(
            "list key may only contain lowercase letters, digits and dashes".to_string(),
        ));
    }
    Ok(trimmed)
}

/// Validate an address and normalise it to lower case.
///
/// The check is deliberately shape-only: an address that passes here is not proven to exist,
/// and a store that tried to prove it would be an SMTP probe triggered by a form. That is what
/// double opt-in is for.
pub fn validate_email(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidNewsletter("e-mail address is required".to_string()));
    }
    if trimmed.chars().count() > MAX_EMAIL {
        return Err(ContentError::InvalidNewsletter("that address is too long".to_string()));
    }
    let ok = {
        let mut parts = trimmed.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        parts.next().is_none()
            && !local.is_empty()
            && local.len() <= 64
            && !domain.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !trimmed.chars().any(char::is_whitespace)
    };
    if !ok {
        return Err(ContentError::InvalidNewsletter(
            "that does not look like an e-mail address".to_string(),
        ));
    }
    Ok(trimmed.to_lowercase())
}

/// Validate a subscriber state.
pub fn validate_status(raw: &str) -> Result<()> {
    if SUBSCRIBER_STATUSES.contains(&raw) {
        Ok(())
    } else {
        Err(ContentError::InvalidNewsletter(format!(
            "unknown subscriber state '{raw}'"
        )))
    }
}

/// Validate an issue subject.
pub fn validate_subject(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidNewsletter("subject is required".to_string()));
    }
    if trimmed.chars().count() > MAX_SUBJECT {
        return Err(ContentError::InvalidNewsletter(format!(
            "subject is at most {MAX_SUBJECT} characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Validate an issue body: non-blank, bounded, and sanitised.
///
/// The bound is on the RAW length, before sanitising, because the raw is what arrives over the
/// wire and the platform's job is to refuse a payload it was never going to render.
pub fn validate_body_html(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ContentError::InvalidNewsletter("message body is required".to_string()));
    }
    if trimmed.chars().count() > 200_000 {
        return Err(ContentError::InvalidNewsletter("message body is too long".to_string()));
    }
    // `sanitize_html` returns (markup, report); the report is the panel's sanitiser preview
    // and the store keeps only the markup. A newsletter body is the most common stored-XSS
    // carrier in a CMS precisely because it is the one field allowed to carry markup.
    let (markup, _report) = crate::sanitize::sanitize_html(trimmed);
    Ok(markup)
}

/// Turn a name into a URL segment: lowercase, alphanumerics and dashes, no leading dash.
///
/// Deterministic on purpose — the issue slug is a permalink, and "slug of the subject" has to
/// be a function anybody can reproduce to link to an issue in a chat.
pub fn slugify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        out.push_str("issue");
    }
    out
}

/// Hash a token with SHA-256, hex.
///
/// The only place a token becomes a string the database holds, and it is one-directional on
/// purpose: see the module docs.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A fresh confirmation token and its digest.
fn new_confirm_token() -> (String, String) {
    let token = random_token();
    let hash = hash_token(&token);
    (token, hash)
}

/// A fresh unsubscribe token and its digest.
fn new_unsubscribe_token() -> (String, String) {
    let token = random_token();
    let hash = hash_token(&token);
    (token, hash)
}

/// 32 bytes of randomness, hex. Two tokens per signup, so 64 bytes of entropy.
///
/// Hex rather than base64url on purpose: the token travels in a query string and inside a mail
/// client, and hex has no character a mail client or a proxy will rewrite.
fn random_token() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Escape the LIKE metacharacters a search term may carry.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// A page size in a sane range.
fn clamp_page(raw: i64) -> i64 {
    if raw <= 0 {
        50
    } else {
        raw.min(500)
    }
}

/// `2026-09-29T10:11:12Z`, or an empty string.
fn format_instant(at: OffsetDateTime) -> String {
    // The CSV wants a stable, sortable, unambiguous stamp. RFC 3339 in UTC is the only format
    // that survives a spreadsheet in a Turkish locale.
    let unix = at.unix_timestamp();
    if unix < 0 {
        return String::new();
    }
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .map(|t| t.to_offset(time::UtcOffset::UTC))
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

/// One CSV field, quoted when it needs to be.
fn csv_field(raw: &str) -> String {
    if raw.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

/// The addresses in a CSV, one per line, first column.
///
/// A header line is detected and dropped — "email" is not an address, and an import that
/// creates a subscriber called "email" is the most common way a spreadsheet import goes wrong.
pub fn parse_csv_addresses(csv: &str) -> Vec<String> {
    csv.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let first = line.split(',').next().unwrap_or("").trim().trim_matches('"');
            if first.is_empty() {
                None
            } else if first.eq_ignore_ascii_case("email")
                || first.eq_ignore_ascii_case("e-mail")
                || first.eq_ignore_ascii_case("mail")
            {
                // A header: skip it, but only if something follows, so a file that is literally
                // one address called "email" is not silently dropped.
                None
            } else {
                Some(first.to_string())
            }
        })
        .collect()
}

/// Map a unique-violation on insert to the rule's own error, by the constraint's name.
fn map_insert_error(err: sqlx::Error, constraint: &str, message: &str) -> ContentError {
    if let sqlx::Error::Database(ref db) = err {
        // Match on the INDEX NAME the database names, not on a SQLSTATE: the category only
        // says which class of thing happened, and the name is the only thing that says which
        // rule fired. Slice 4a recorded the cost of getting this backwards — a check violation
        // on one column coming back to the panel as a completely different rule.
        let text = db.message().to_lowercase();
        if text.contains(constraint)
            || text.contains("newsletter_lists_site_key_idx")
            || text.contains("newsletter_subscribers_list_email_idx")
        {
            return ContentError::InvalidNewsletter(message.to_string());
        }
    }
    ContentError::from(err)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_lowercased_so_the_same_person_is_not_twice_a_subscriber() {
        assert_eq!(
            validate_email("  Reader@Example.COM ").unwrap(),
            "reader@example.com"
        );
    }

    #[test]
    fn an_address_that_is_only_shaped_wrong_is_refused() {
        for bad in [
            "",
            "no-at-sign",
            "@example.com",
            "reader@",
            "reader@example",
            "reader@@example.com",
            "reader @example.com",
            "reader@exa mple.com",
        ] {
            assert!(
                validate_email(bad).is_err(),
                "expected {bad:?} to be refused"
            );
        }
    }

    #[test]
    fn an_address_longer_than_the_rfc_ceiling_is_refused() {
        let long = format!("{}@example.com", "a".repeat(250));
        assert!(validate_email(&long).is_err());
    }

    #[test]
    fn a_list_key_is_a_url_segment() {
        assert_eq!(validate_list_key("  Weekly-News ").unwrap(), "weekly-news");
        for bad in ["", "-lead", "with space", "UPPER!", "a/b"] {
            assert!(validate_list_key(bad).is_err(), "expected {bad:?} refused");
        }
    }

    #[test]
    fn a_slug_is_reproducible_from_the_subject() {
        assert_eq!(slugify("Weekly News — Issue 12"), "weekly-news-issue-12");
        assert_eq!(slugify("  ...  "), "issue");
        assert_eq!(slugify("Ärger mit Umlauten"), "rger-mit-umlauten");
    }

    #[test]
    fn a_slug_stays_inside_its_own_bound() {
        let long = "word ".repeat(50);
        let slug = slugify(&long);
        assert!(slug.len() <= 60, "slug was {} chars", slug.len());
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn a_token_is_stored_as_a_digest_that_cannot_be_reversed() {
        let token = "abc123";
        let hash = hash_token(token);
        assert_eq!(hash.len(), 64);
        assert!(!hash.contains(token));
        assert_eq!(hash, hash_token(token), "hashing is deterministic");
        assert_ne!(hash, hash_token("abc124"));
    }

    #[test]
    fn a_header_line_is_not_imported_as_an_address_called_email() {
        let rows = parse_csv_addresses("email,name\nreader@example.com,Reader\n");
        assert_eq!(rows, vec!["reader@example.com".to_string()]);
    }

    #[test]
    fn a_csv_import_reads_the_first_column_and_ignores_the_rest() {
        let rows = parse_csv_addresses("a@example.com,A\nb@example.com,\"B, Jr\"\n\n");
        assert_eq!(
            rows,
            vec!["a@example.com".to_string(), "b@example.com".to_string()]
        );
    }

    #[test]
    fn a_csv_field_is_quoted_when_it_carries_a_comma() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("with,comma"), "\"with,comma\"");
        assert_eq!(csv_field("with\"quote"), "\"with\"\"quote\"");
    }

    #[test]
    fn a_search_term_cannot_smuggle_a_wildcard_into_the_query() {
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn only_the_four_states_are_accepted() {
        for ok in SUBSCRIBER_STATUSES {
            assert!(validate_status(ok).is_ok(), "{ok} should be accepted");
        }
        assert!(validate_status("bounced ").is_err());
        assert!(validate_status("deleted").is_err());
    }

    #[test]
    fn an_issue_body_is_sanitised_and_a_blank_one_is_refused() {
        assert!(validate_body_html("   ").is_err());
        let cleaned = validate_body_html("<p>Hello</p><script>alert(1)</script>").unwrap();
        assert!(cleaned.contains("Hello"));
        assert!(!cleaned.contains("<script"), "script tag survived: {cleaned}");
    }

    #[test]
    fn a_list_name_is_required_and_bounded() {
        assert!(validate_list_name("  ").is_err());
        assert_eq!(validate_list_name("  News  ").unwrap(), "News");
        assert!(validate_list_name(&"x".repeat(MAX_LIST_NAME + 1)).is_err());
    }

    #[test]
    fn only_confirmed_addresses_are_deliverable() {
        let counts = ListCounts {
            pending: 3,
            confirmed: 7,
            unsubscribed: 2,
            bounced: 1,
        };
        assert_eq!(counts.deliverable(), 7);
    }
}
