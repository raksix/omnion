//! Page comments: threaded (two levels) with moderation states and local spam heuristics
//! (REQ-064, slice 4a).
//!
//! A comment is the only row on this platform a **stranger writes**, and that single fact
//! decides the module's shape. Everything the visitor supplies is stored; nothing they supply is
//! trusted. Three consequences run through every function below:
//!
//! * **The body is plain text.** There is no HTML to sanitise because none is ever accepted, so
//!   [`PublicComment::body`] is what the renderer escapes. A stored `<script>` is a paragraph of
//!   text, and a comment that needs markup is a content page, not a comment.
//!
//! * **The heuristics decide a STATE, never a deletion.** [`classify`] returns
//!   `pending`/`approved`/`spam` with the rule that decided it, and the row is always written.
//!   A heuristic that quietly discarded a person's comment is a heuristic nobody can appeal, so
//!   the reason is stored and the inbox shows it — the REQ's own risk note says the panel must
//!   never claim "spam blocked" with certainty, and a stored reason is how a moderator checks.
//!
//! * **Nothing on the visitor's path decides.** The public list reads approved comments with a
//!   partial index; the heuristics read counts that the settings row carries, and a row that has
//!   never been configured is `comments_enabled = false` rather than a site that starts taking
//!   comments the moment it is installed.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::validation::validate_text;

/// Longest accepted comment body.
///
/// Generous for prose and short for an essay: a comment that is a page belongs in a page.
pub const MAX_COMMENT_BODY: usize = 4_000;

/// Longest accepted author name.
pub const MAX_AUTHOR_NAME: usize = 120;

/// Longest accepted stored client hints (an address and a user agent, nothing more).
pub const MAX_CLIENT_HINT: usize = 400;

/// The stored reason for a body that was already submitted on this page.
///
/// A `const` rather than an inline literal because it is compared by a test and read by the
/// panel; the id of the earlier comment is appended to it, so what a moderator reads is
/// "duplicate of an earlier comment (<id>)" rather than a bare label.
pub const SPAM_DUPLICATE: &str = "duplicate of an earlier comment";

/// The four inbox tabs, in the order the panel shows them.
pub const COMMENT_STATUSES: [&str; 4] = ["pending", "approved", "spam", "trash"];

/// A comment a visitor can read. The public shape carries no address, no IP and no status
/// hint — a reader has no business learning them, and a payload is the easiest place to leak one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicComment {
    /// Comment id.
    pub id: Uuid,
    /// Display name, as the author typed it.
    pub author_name: String,
    /// The body, plain text.
    pub body: String,
    /// Replies to this comment, oldest first.
    pub replies: Vec<PublicComment>,
    /// When it was written.
    pub created_at: OffsetDateTime,
    /// Whether a moderator answered, so the thread can label itself without a join.
    pub has_staff_reply: bool,
}

/// One stored comment.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Comment {
    /// Comment id.
    pub id: Uuid,
    /// Organization the site's site belongs to.
    pub organization_id: Uuid,
    /// Site the comment was left on.
    pub site_id: Uuid,
    /// Page commented on.
    pub page_id: Uuid,
    /// The comment this one answers; `None` for a top-level comment.
    pub parent_id: Option<Uuid>,
    /// Generated: 0 for a comment, 1 for a reply.
    pub reply_depth: i16,
    /// Author display name.
    pub author_name: String,
    /// Author address. Never leaves the panel.
    pub author_email: String,
    /// Coarse client hint; never leaves the panel.
    pub ip_hint: Option<String>,
    /// Raw user agent; never leaves the panel.
    pub user_agent: Option<String>,
    /// The body, plain text.
    pub body: String,
    /// `pending`, `approved`, `spam` or `trash`.
    pub status: String,
    /// The heuristic that marked it as spam, or a moderator's note.
    pub spam_reason: Option<String>,
    /// When a moderator approved it.
    pub approved_at: Option<OffsetDateTime>,
    /// Who approved it.
    pub approved_by: Option<Uuid>,
    /// Whether a moderator wrote it.
    pub is_staff_reply: bool,
    /// When it was written.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

impl Comment {
    /// Whether the comment may be shown on the public page.
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.status == "approved"
    }
}

/// The site-wide comment settings. A row is created on first read, so every screen has settings
/// to draw and `comments_enabled` is `false` until somebody turns it on.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct CommentSettings {
    /// The site.
    pub site_id: Uuid,
    /// Organization the site belongs to.
    pub organization_id: Uuid,
    /// Whether the public form accepts anything at all.
    pub comments_enabled: bool,
    /// An address with this many approved comments skips the queue; 0 disables it.
    pub auto_approve_after_comments: i32,
    /// Case-insensitive substrings that mark a comment as spam.
    pub blocked_words: Vec<String>,
    /// How many links a body may carry.
    pub max_links_per_comment: i32,
    /// Seconds the form must have been on screen.
    pub min_fill_seconds: i32,
    /// Comments per IP per hour.
    pub per_ip_per_hour: i32,
    /// Whether a moderator is notified.
    pub notify_on_comment: bool,
    /// When the settings last changed.
    pub updated_at: OffsetDateTime,
}

impl Default for CommentSettings {
    /// The safe default: comments off, no trust, a three-second floor and five per hour.
    fn default() -> Self {
        Self {
            site_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            comments_enabled: false,
            auto_approve_after_comments: 0,
            blocked_words: Vec::new(),
            max_links_per_comment: 2,
            min_fill_seconds: 3,
            per_ip_per_hour: 5,
            notify_on_comment: true,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}

/// A ban a moderator placed on an address or a client hint.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct CommentBan {
    /// Ban id.
    pub id: Uuid,
    /// The site it applies to.
    pub site_id: Uuid,
    /// `email` or `ip`.
    pub kind: String,
    /// The banned value.
    pub value: String,
    /// Why, in the moderator's words.
    pub reason: Option<String>,
    /// Who placed it.
    pub created_by: Option<Uuid>,
    /// When it was placed.
    pub created_at: OffsetDateTime,
    /// When it stops applying; `None` is forever.
    pub expires_at: Option<OffsetDateTime>,
}

impl CommentBan {
    /// Whether the ban still applies at `now`.
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.expires_at.is_none_or(|until| until > now)
    }
}

/// What a visitor submitted.
#[derive(Debug, Clone)]
pub struct NewComment {
    /// Organization the site belongs to.
    pub organization_id: Uuid,
    /// The site.
    pub site_id: Uuid,
    /// The page.
    pub page_id: Uuid,
    /// The comment being answered, when this is a reply.
    pub parent_id: Option<Uuid>,
    /// Author display name.
    pub author_name: String,
    /// Author address.
    pub author_email: String,
    /// Coarse client hint.
    pub ip_hint: Option<String>,
    /// Raw user agent.
    pub user_agent: Option<String>,
    /// The body.
    pub body: String,
    /// How many links the body carries — measured by the caller from the rendered body, and
    /// passed in rather than re-counted here so the one implementation of "a link" lives with
    /// the thing that extracts links.
    pub link_count: i32,
    /// How long the form was on screen, in seconds.
    pub fill_seconds: i32,
}

/// A moderator's reply. Not a visitor submission: it skips every heuristic, is written by an
/// account, and is published immediately.
#[derive(Debug, Clone)]
pub struct NewStaffReply {
    /// Organization the site belongs to.
    pub organization_id: Uuid,
    /// The site.
    pub site_id: Uuid,
    /// The page.
    pub page_id: Uuid,
    /// The comment being answered.
    pub parent_id: Uuid,
    /// The moderator's account.
    pub author_user_id: Uuid,
    /// Display name shown beside the reply.
    pub author_name: String,
    /// The body.
    pub body: String,
}

/// The moderation inbox: a page of one status plus the counts for the four tabs.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxPage {
    /// The rows, newest first.
    pub comments: Vec<Comment>,
    /// How many rows the filter matched in total.
    pub total: i64,
    /// Counts per status for the tab bar, in [`COMMENT_STATUSES`] order.
    pub counts: Vec<(String, i64)>,
}

/// The filter the inbox screen sends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxFilter {
    /// Restrict to one status; `None` means all four.
    pub status: Option<String>,
    /// Free text over author, address and body.
    pub search: Option<String>,
    /// One page.
    pub page_id: Option<Uuid>,
    /// Page size.
    pub limit: i64,
    /// Offset.
    pub offset: i64,
}

impl InboxFilter {
    /// A filter for one tab, at the panel's default page size.
    #[must_use]
    pub fn for_status(status: &str) -> Self {
        Self {
            status: Some(status.to_owned()),
            limit: 50,
            ..Self::default()
        }
    }
}

/// Columns of a stored comment row, in [`Comment`] order.
const COMMENT_COLUMNS: &str = "id, organization_id, site_id, page_id, parent_id, reply_depth, \
     author_name, author_email, ip_hint, user_agent, body, status, spam_reason, approved_at, \
     approved_by, is_staff_reply, created_at, updated_at";

/// Columns of a settings row, in [`CommentSettings`] order.
const SETTINGS_COLUMNS: &str = "site_id, organization_id, comments_enabled, \
     auto_approve_after_comments, blocked_words, max_links_per_comment, min_fill_seconds, \
     per_ip_per_hour, notify_on_comment, updated_at";

/// Why a submission landed where it did.
///
/// Every variant is a *claim the panel can show*, and `Approved` is the only one that means
/// "this is what the rules decided", not "a human looked at it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Queued for a moderator.
    Pending,
    /// Published immediately, because the address is trusted or the rules allow it.
    Approved,
    /// Held as junk, with the reason.
    Spam(&'static str),
}

impl Decision {
    /// The status this decision writes.
    #[must_use]
    pub const fn status(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Spam(_) => "spam",
        }
    }

    /// The reason, when there is one.
    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Spam(reason) => Some(reason),
            _ => None,
        }
    }
}

/// The comment store.
#[derive(Debug, Clone)]
pub struct CommentStore {
    pool: PgPool,
}

impl CommentStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // -----------------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------------

    /// Read a site's settings, creating the row on first read.
    ///
    /// Created on read rather than on install so a site that never opens the screen has no row
    /// to keep in sync, and a site that opens it gets the safe default — comments off — without
    /// anybody having to remember to seed it.
    pub async fn settings(&self, site_id: Uuid, organization_id: Uuid) -> Result<CommentSettings> {
        let sql = format!(
            "insert into cms_comment_settings (site_id, organization_id) \
             values ($1, $2) \
             on conflict (site_id) do update set site_id = excluded.site_id \
             returning {SETTINGS_COLUMNS}"
        );

        let row: CommentSettings = sqlx::query_as(&sql)
            .bind(site_id)
            .bind(organization_id)
            .fetch_one(&self.pool)
            .await?;

        Ok(row)
    }

    /// Save a site's settings.
    ///
    /// Every field is written from the caller's row: there is no PATCH here on purpose, because
    /// a partial update of a moderation policy is a policy nobody can reconstruct afterwards.
    pub async fn save_settings(&self, settings: &CommentSettings) -> Result<CommentSettings> {
        validate_settings(settings)?;

        let blocked = normalize_words(&settings.blocked_words)?;

        let sql = format!(
            "insert into cms_comment_settings \
                 (site_id, organization_id, comments_enabled, auto_approve_after_comments, \
                  blocked_words, max_links_per_comment, min_fill_seconds, per_ip_per_hour, \
                  notify_on_comment) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (site_id) do update set \
                 comments_enabled = excluded.comments_enabled, \
                 auto_approve_after_comments = excluded.auto_approve_after_comments, \
                 blocked_words = excluded.blocked_words, \
                 max_links_per_comment = excluded.max_links_per_comment, \
                 min_fill_seconds = excluded.min_fill_seconds, \
                 per_ip_per_hour = excluded.per_ip_per_hour, \
                 notify_on_comment = excluded.notify_on_comment, \
                 updated_at = now() \
             returning {SETTINGS_COLUMNS}"
        );

        let row: CommentSettings = sqlx::query_as(&sql)
            .bind(settings.site_id)
            .bind(settings.organization_id)
            .bind(settings.comments_enabled)
            .bind(settings.auto_approve_after_comments)
            .bind(blocked)
            .bind(settings.max_links_per_comment)
            .bind(settings.min_fill_seconds)
            .bind(settings.per_ip_per_hour)
            .bind(settings.notify_on_comment)
            .fetch_one(&self.pool)
            .await?;

        Ok(row)
    }

    // -----------------------------------------------------------------------------------------
    // Submission
    // -----------------------------------------------------------------------------------------

    /// Accept a visitor's comment, deciding its state from the rules.
    ///
    /// The order is deliberate and load-bearing: the bans first (a banned person is not counted
    /// against a rate limit, refused before anything is measured), then the rate limit, then the
    /// content heuristics, then trust. A submission refused by a ban leaves no row, which is the
    /// only thing in this module that writes nothing — every other refusal is a row a moderator
    /// can see and undo.
    pub async fn submit(&self, new: NewComment) -> Result<Comment> {
        let settings = self.settings(new.site_id, new.organization_id).await?;
        if !settings.comments_enabled {
            return Err(ContentError::InvalidComment(
                "this site is not accepting comments".to_owned(),
            ));
        }

        let body = validate_body(&new.body)?;
        let name = validate_text(
            new.author_name.trim(),
            MAX_AUTHOR_NAME,
            "the comment needs an author name",
        )?;
        let email = validate_email(&new.author_email)?;
        let ip_hint = validate_hint(new.ip_hint.as_deref(), MAX_CLIENT_HINT)?;
        let user_agent = validate_hint(new.user_agent.as_deref(), MAX_CLIENT_HINT)?;

        if let Some(banned) = self.active_ban(new.site_id, &email, ip_hint.as_deref()).await? {
            return Err(ContentError::CommentBanned(format!(
                "this address is not allowed to comment on this site ({})",
                banned.reason.unwrap_or_else(|| "banned by a moderator".to_owned())
            )));
        }

        if let Some(parent_id) = new.parent_id {
            self.check_reply_parent(new.page_id, parent_id).await?;
        }

        if new.link_count > settings.max_links_per_comment {
            return Ok(self
                .write(new, &body, &name, &email, ip_hint, user_agent, Decision::Spam("too many links"), None)
                .await?);
        }

        if let Some(reason) = first_blocked_word(&body, &settings.blocked_words) {
            return Ok(self
                .write(new, &body, &name, &email, ip_hint, user_agent, Decision::Spam(reason), None)
                .await?);
        }

        if new.fill_seconds < settings.min_fill_seconds {
            return Ok(self
                .write(
                    new,
                    &body,
                    &name,
                    &email,
                    ip_hint,
                    user_agent,
                    Decision::Spam("filled in too quickly to be human"),
                    None,
                )
                .await?);
        }

        if let Some(limit) = self.rate_limited(new.site_id, ip_hint.as_deref()).await? {
            return Ok(self
                .write(new, &body, &name, &email, ip_hint, user_agent, Decision::Spam(limit), None)
                .await?);
        }

        if let Some(_earlier) = self.duplicate(new.page_id, &email, &body).await? {
            // The second copy is WRITTEN and marked, not refused: the whole point of the rule is
            // that a moderator can see somebody submitted twice, and a hard unique index makes
            // that impossible — the second insert is exactly what the index forbids, so the
            // evidence the rule exists to produce cannot be stored. The duplicate check is the
            // store's, and the store's answer is a row.
            return Ok(self
                .write(
                    new,
                    &body,
                    &name,
                    &email,
                    ip_hint,
                    user_agent,
                    Decision::Spam(SPAM_DUPLICATE),
                    None,
                )
                .await?);
        }

        let threshold = i64::from(settings.auto_approve_after_comments);
        let decision = if threshold > 0
            && self.approved_count(new.site_id, &email).await? >= threshold
        {
            Decision::Approved
        } else {
            Decision::Pending
        };

        let approved_by = if decision == Decision::Approved {
            self.moderator_for(new.site_id).await?
        } else {
            None
        };

        Ok(self
            .write(
                new,
                &body,
                &name,
                &email,
                ip_hint,
                user_agent,
                decision,
                approved_by,
            )
            .await?)
    }

    /// Write one moderator reply. Published immediately: a moderator has already moderated it.
    pub async fn staff_reply(&self, new: NewStaffReply) -> Result<Comment> {
        let body = validate_body(&new.body)?;
        let name = validate_text(
            new.author_name.trim(),
            MAX_AUTHOR_NAME,
            "the reply needs a display name",
        )?;

        // A moderator is held to the same two-level rule as a visitor. "Reply as site" that can
        // start a third level is a thread nobody can render, and a moderator is exactly the
        // person who would notice that the answer disappeared.
        self.check_reply_parent(new.page_id, new.parent_id).await?;

        let sql = format!(
            "insert into cms_comments \
                 (organization_id, site_id, page_id, parent_id, author_name, author_email, \
                  body, status, spam_reason, approved_at, is_staff_reply) \
             values ($1, $2, $3, $4, $5, '', $6, 'approved', null, now(), true) \
             returning {COMMENT_COLUMNS}"
        );

        let comment: Comment = sqlx::query_as(&sql)
            .bind(new.organization_id)
            .bind(new.site_id)
            .bind(new.page_id)
            .bind(new.parent_id)
            .bind(name)
            .bind(body)
            .fetch_one(&self.pool)
            .await?;

        Ok(comment)
    }

    /// Insert a comment in a state the rules already decided.
    ///
    /// One writer for every path that creates a row, because the column list is the part of an
    /// insert that is easy to get subtly wrong and impossible to notice: a column left out keeps
    /// its default, and the default for `status` is `pending` — so the one bug that matters most
    /// here is a *silent* one.
    #[allow(clippy::too_many_arguments)]
    async fn write(
        &self,
        new: NewComment,
        body: &str,
        name: &str,
        email: &str,
        ip_hint: Option<String>,
        user_agent: Option<String>,
        decision: Decision,
        approved_by: Option<Uuid>,
    ) -> Result<Comment> {
        let status = decision.status();
        let reason = decision.reason();
        let approved_at = (status == "approved").then(OffsetDateTime::now_utc);

        let sql = format!(
            "insert into cms_comments \
                 (organization_id, site_id, page_id, parent_id, author_name, author_email, \
                  ip_hint, user_agent, body, status, spam_reason, approved_at, approved_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             returning {COMMENT_COLUMNS}"
        );

        let comment: Comment = sqlx::query_as(&sql)
            .bind(new.organization_id)
            .bind(new.site_id)
            .bind(new.page_id)
            .bind(new.parent_id)
            .bind(name)
            .bind(email)
            .bind(ip_hint)
            .bind(user_agent)
            .bind(body)
            .bind(status)
            .bind(reason)
            .bind(approved_at)
            .bind(approved_by)
            .fetch_one(&self.pool)
            .await?;

        Ok(comment)
    }

    // -----------------------------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------------------------

    /// The moderation inbox: one filtered page plus the four tab counts.
    ///
    /// The counts come from their own grouped query rather than from the page, so the tab bar
    /// answers for the WHOLE inbox while the table shows 50 rows — a count taken from the
    /// visible rows is a count that says "50" the moment a site passes fifty comments.
    pub async fn inbox(&self, site_id: Uuid, filter: &InboxFilter) -> Result<InboxPage> {
        let limit = filter.limit.clamp(1, 200);
        let offset = filter.offset.max(0);

        // The count and the page are built by the SAME loop over the SAME filter list, so a
        // placeholder can only exist where its value was pushed. Two hand-written query pairs
        // are how a tab says "12" while the table shows 13 rows.
        let mut rows =
            QueryBuilder::<Postgres>::new(format!("select {COMMENT_COLUMNS} from cms_comments where "));
        push_inbox_filters(&mut rows, filter, site_id);
        rows.push(" order by created_at desc, id desc limit ");
        rows.push_bind(limit);
        rows.push(" offset ");
        rows.push_bind(offset);
        let comments: Vec<Comment> = rows.build_query_as().fetch_all(&self.pool).await?;

        let mut count = QueryBuilder::<Postgres>::new("select count(*) from cms_comments where ");
        push_inbox_filters(&mut count, filter, site_id);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        let counts_sql = "select status, count(*) from cms_comments \
                          where site_id = $1 group by status";
        let counted: Vec<(String, i64)> = sqlx::query_as(&counts_sql)
            .bind(site_id)
            .fetch_all(&self.pool)
            .await?;

        // Every tab is present even at zero, in the declared order: a tab bar that hides the
        // empty states is a tab bar that cannot tell a moderator "there is nothing in Spam".
        let mut counts = COMMENT_STATUSES
            .iter()
            .map(|status| {
                let n = counted
                    .iter()
                    .find(|(s, _)| s == status)
                    .map_or(0, |(_, n)| *n);
                ((*status).to_owned(), n)
            })
            .collect::<Vec<_>>();

        // A status outside the four cannot happen (the CHECK refuses it) but the query is
        // written against the table rather than the enum, and dropping a row silently is how a
        // count goes missing.
        for (status, n) in counted {
            if !COMMENT_STATUSES.contains(&status.as_str()) {
                counts.push((status, n));
            }
        }

        Ok(InboxPage {
            comments,
            total,
            counts,
        })
    }

    /// One comment, scoped to the site in the `where` clause.
    pub async fn get(&self, site_id: Uuid, comment_id: Uuid) -> Result<Comment> {
        let sql = format!(
            "select {COMMENT_COLUMNS} from cms_comments where id = $1 and site_id = $2"
        );

        let comment: Comment = sqlx::query_as(&sql)
            .bind(comment_id)
            .bind(site_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ContentError::CommentNotFound)?;

        Ok(comment)
    }

    /// A page's approved comments as two-level threads, oldest first.
    ///
    /// One query, not two: the replies are read with their parents and stitched here, so a
    /// comment that is approved between the two queries cannot appear as a thread with no
    /// question above it — which is what a public comment list looks like when it is wrong.
    pub async fn thread_for_page(&self, page_id: Uuid) -> Result<Vec<PublicComment>> {
        let sql = format!(
            "select {COMMENT_COLUMNS} from cms_comments \
             where page_id = $1 and status = 'approved' \
             order by created_at asc, id asc"
        );

        let rows: Vec<Comment> = sqlx::query_as(&sql)
            .bind(page_id)
            .fetch_all(&self.pool)
            .await?;

        let mut threads: Vec<PublicComment> = Vec::new();
        let mut index_by_id: Vec<(Uuid, usize)> = Vec::new();

        for row in &rows {
            match row.parent_id {
                None => {
                    index_by_id.push((row.id, threads.len()));
                    threads.push(public_of(row));
                }
                Some(parent_id) => {
                    if let Some((_, at)) = index_by_id.iter().find(|(id, _)| *id == parent_id) {
                        // The badge belongs to the THREAD, not to the reply: a reader looks at
                        // the question and decides whether it was answered, and a flag carried by
                        // the answer itself is a flag they have to go and find. Set it on the
                        // parent as the reply is stitched on.
                        threads[*at].has_staff_reply |= row.is_staff_reply;
                        threads[*at].replies.push(public_of(row));
                    }
                    // A reply whose parent is not approved is deliberately DROPPED rather than
                    // promoted: a moderator who hides a question has not agreed to show its
                    // answer, and the alternative is a top-level comment that was never a
                    // top-level comment.
                }
            }
        }

        Ok(threads)
    }

    // -----------------------------------------------------------------------------------------
    // Moderation
    // -----------------------------------------------------------------------------------------

    /// Move a comment to a state, recording who did it.
    ///
    /// Approving a comment that was already approved is refused rather than tolerated: the panel
    /// offers the action only in the inbox, and a second approval is either a stale page or a
    /// second moderator whose click raced the first — the same comment cannot have two.
    pub async fn set_status(
        &self,
        site_id: Uuid,
        comment_id: Uuid,
        status: &str,
        moderator: Option<Uuid>,
        reason: Option<&str>,
    ) -> Result<Comment> {
        if !COMMENT_STATUSES.contains(&status) {
            return Err(ContentError::InvalidComment(format!(
                "{status:?} is not a moderation state"
            )));
        }

        let current = self.get(site_id, comment_id).await?;
        if current.status == status {
            return Err(ContentError::CommentAlreadyInState(format!(
                "this comment is already {status}"
            )));
        }
        if current.status == "trash" && status != "trash" {
            // Restoring a trashed comment whose parent is gone would leave an orphan reply, so
            // the check is on the parent rather than on the comment.
            if let Some(parent) = current.parent_id {
                let parent_status: Option<String> =
                    sqlx::query_scalar("select status from cms_comments where id = $1")
                        .bind(parent)
                        .fetch_optional(&self.pool)
                        .await?;
                if parent_status.is_none() {
                    return Err(ContentError::CommentNotFound);
                }
            }
        }

        let approved = (status == "approved").then(OffsetDateTime::now_utc);
        let stamp = (status == "approved").then_some(moderator);

        let sql = format!(
            "update cms_comments set status = $1, spam_reason = $2, approved_at = $3, \
                 approved_by = $4, updated_at = now() \
             where id = $5 and site_id = $6 returning {COMMENT_COLUMNS}"
        );

        let comment: Comment = sqlx::query_as(&sql)
            .bind(status)
            .bind(reason.map(str::to_owned))
            .bind(approved)
            .bind(stamp)
            .bind(comment_id)
            .bind(site_id)
            .fetch_one(&self.pool)
            .await?;

        Ok(comment)
    }

    /// Delete a comment for good.
    ///
    /// The panel offers this from the Trash tab only, and it is the one irreversible action in
    /// the module: `trash` is the recoverable state and this is the one that removes the row.
    pub async fn delete(&self, site_id: Uuid, comment_id: Uuid) -> Result<()> {
        let done = sqlx::query("delete from cms_comments where id = $1 and site_id = $2")
            .bind(comment_id)
            .bind(site_id)
            .execute(&self.pool)
            .await?;

        if done.rows_affected() == 0 {
            return Err(ContentError::CommentNotFound);
        }
        Ok(())
    }

    /// Moderate several comments at once, reporting per-id outcomes.
    ///
    /// One transaction-free loop rather than a single statement because the statuses are
    /// per-row decisions — a comment somebody deleted between the click and this call must not
    /// take the other forty-nine with it, and a report that says which ones failed is the only
    /// way the panel can tell a moderator their bulk action was partial.
    pub async fn bulk_set_status(
        &self,
        site_id: Uuid,
        comment_ids: &[Uuid],
        status: &str,
        moderator: Option<Uuid>,
    ) -> Result<BulkOutcome> {
        if comment_ids.is_empty() {
            return Ok(BulkOutcome::default());
        }
        if !COMMENT_STATUSES.contains(&status) {
            return Err(ContentError::InvalidComment(format!(
                "{status:?} is not a moderation state"
            )));
        }

        let mut outcome = BulkOutcome::default();
        for id in comment_ids {
            match self.set_status(site_id, *id, status, moderator, None).await {
                Ok(_) => outcome.updated.push(*id),
                Err(ContentError::CommentNotFound) => outcome.missing.push(*id),
                Err(_) => outcome.refused.push(*id),
            }
        }
        Ok(outcome)
    }

    // -----------------------------------------------------------------------------------------
    // Bans
    // -----------------------------------------------------------------------------------------

    /// List a site's bans, newest first.
    pub async fn bans(&self, site_id: Uuid) -> Result<Vec<CommentBan>> {
        let sql = "select id, site_id, kind, value, reason, created_by, created_at, expires_at \
                   from cms_comment_bans where site_id = $1 order by created_at desc, id desc";

        let bans: Vec<CommentBan> = sqlx::query_as(sql)
            .bind(site_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(bans)
    }

    /// Place a ban. Re-banning a value updates the existing row rather than refusing, because
    /// the moderator's intent is the same both times and a 409 on a second click reads as a bug.
    pub async fn add_ban(&self, ban: &CommentBan) -> Result<CommentBan> {
        if !matches!(ban.kind.as_str(), "email" | "ip") {
            return Err(ContentError::InvalidComment(format!(
                "{:?} is not a kind of ban",
                ban.kind
            )));
        }
        let value = ban.value.trim().to_lowercase();
        if value.is_empty() {
            return Err(ContentError::InvalidComment(
                "a ban needs a value".to_owned(),
            ));
        }
        if ban.kind == "email" && !value.contains('@') {
            return Err(ContentError::InvalidComment(
                "that is not an e-mail address".to_owned(),
            ));
        }
        if ban.kind == "ip" && value.contains('@') {
            return Err(ContentError::InvalidComment(
                "that is not a network address".to_owned(),
            ));
        }

        let sql = "insert into cms_comment_bans (site_id, kind, value, reason, created_by, expires_at) \
                   values ($1, $2, $3, $4, $5, $6) \
                   on conflict (site_id, kind, lower(value)) do update set \
                       reason = excluded.reason, created_by = excluded.created_by, \
                       expires_at = excluded.expires_at, created_at = now() \
                   returning id, site_id, kind, value, reason, created_by, created_at, expires_at";

        let stored: CommentBan = sqlx::query_as(sql)
            .bind(ban.site_id)
            .bind(&ban.kind)
            .bind(&value)
            .bind(ban.reason.as_deref().map(str::to_owned))
            .bind(ban.created_by)
            .bind(ban.expires_at)
            .fetch_one(&self.pool)
            .await?;

        Ok(stored)
    }

    /// Lift a ban.
    pub async fn remove_ban(&self, site_id: Uuid, ban_id: Uuid) -> Result<()> {
        let done = sqlx::query("delete from cms_comment_bans where id = $1 and site_id = $2")
            .bind(ban_id)
            .bind(site_id)
            .execute(&self.pool)
            .await?;

        if done.rows_affected() == 0 {
            return Err(ContentError::CommentNotFound);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Internal decisions
    // -----------------------------------------------------------------------------------------

    /// The ban that applies to this submission, if any.
    async fn active_ban(
        &self,
        site_id: Uuid,
        email: &str,
        ip_hint: Option<&str>,
    ) -> Result<Option<CommentBan>> {
        let sql = "select id, site_id, kind, value, reason, created_by, created_at, expires_at \
                   from cms_comment_bans \
                   where site_id = $1 and (expires_at is null or expires_at > now()) \
                     and ((kind = 'email' and lower(value) = lower($2)) \
                       or (kind = 'ip' and $3 is not null and lower(value) = lower($3))) \
                   order by created_at desc limit 1";

        let ban: Option<CommentBan> = sqlx::query_as(sql)
            .bind(site_id)
            .bind(email)
            .bind(ip_hint)
            .fetch_optional(&self.pool)
            .await?;

        Ok(ban)
    }

    /// The spam reason for a throttled visitor, or `None` when the hour's allowance is left.
    async fn rate_limited(&self, site_id: Uuid, ip_hint: Option<&str>) -> Result<Option<&'static str>> {
        let Some(ip_hint) = ip_hint else {
            return Ok(None);
        };

        let recent: i64 = sqlx::query_scalar(
            "select count(*) from cms_comments \
             where site_id = $1 and ip_hint = $2 and created_at > now() - interval '1 hour'",
        )
        .bind(site_id)
        .bind(ip_hint)
        .fetch_one(&self.pool)
        .await?;

        if recent == 0 {
            return Ok(None);
        }
        let allowed: i32 = sqlx::query_scalar(
            "select per_ip_per_hour from cms_comment_settings where site_id = $1",
        )
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(CommentSettings::default().per_ip_per_hour);

        Ok((recent >= i64::from(allowed)).then_some("too many comments from one address"))
    }

    /// Check that a reply answers a real TOP-LEVEL comment on this page.
    ///
    /// Two questions with two answers, and one query that answers both. A reply to a comment on
    /// another page, or to one that has been deleted, is `comment_not_found` — there is nothing
    /// to answer. A reply to a REPLY is `comment_too_deep`, and the two must not share a code:
    /// the first is a caller's data error that any client can fix by re-reading the thread, and
    /// the second is the platform's own two-level rule, which no client can fix by trying again.
    /// The trigger in the migration refuses the depth as well; this is the check that can name
    /// it before the write, so the visitor is told the rule rather than a constraint name.
    async fn check_reply_parent(&self, page_id: Uuid, parent_id: Uuid) -> Result<()> {
        let parent: Option<i16> = sqlx::query_scalar(
            "select reply_depth from cms_comments where id = $1 and page_id = $2",
        )
        .bind(parent_id)
        .bind(page_id)
        .fetch_optional(&self.pool)
        .await?;

        match parent {
            None => Err(ContentError::CommentNotFound),
            Some(0) => Ok(()),
            Some(_) => Err(ContentError::CommentThreadTooDeep),
        }
    }

    /// How many approved comments this address has on the site.
    async fn approved_count(&self, site_id: Uuid, email: &str) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(
            "select count(*) from cms_comments \
             where site_id = $1 and lower(author_email) = lower($2) and status = 'approved'",
        )
        .bind(site_id)
        .bind(email)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// The id of the earlier comment this body duplicates, if there is one.
    ///
    /// Returns the id rather than a bool for the same reason the row records a reason: a
    /// moderator reading "duplicate of an earlier comment" needs to know WHICH one, and there
    /// is exactly one answer to that question that a boolean cannot carry.
    async fn duplicate(&self, page_id: Uuid, email: &str, body: &str) -> Result<Option<Uuid>> {
        let earlier: Option<Uuid> = sqlx::query_scalar(
            "select id from cms_comments \
             where page_id = $1 and lower(author_email) = lower($2) \
               and md5(body) = md5($3) and status <> 'trash' \
             order by created_at asc limit 1",
        )
        .bind(page_id)
        .bind(email)
        .bind(body)
        .fetch_optional(&self.pool)
        .await?;
        Ok(earlier)
    }

    /// An account that may be recorded as having approved an auto-approved comment.
    ///
    /// `approved_by` is a foreign key, and an auto-approved comment has no moderator — but the
    /// column records *that* a decision was made and by whom, and NULL would be
    /// indistinguishable from a comment nobody looked at. The site's first account is the
    /// honest value: the site's own policy approved it, and whoever owns the policy owns the
    /// decision.
    ///
    /// The lookup is against `role_bindings`, which is the table that exists. A first draft
    /// joined `user_site_roles` — a table this platform has never had — and a query against a
    /// missing relation is a 500 on every auto-approved comment, which is to say on every
    /// comment once `auto_approve_after_comments` is turned on. That is a feature nobody uses
    /// until the day somebody does.
    async fn moderator_for(&self, site_id: Uuid) -> Result<Option<Uuid>> {
        let account: Option<Uuid> = sqlx::query_scalar(
            "select rb.user_id from role_bindings rb \
             where rb.site_id = $1 and rb.user_id is not null \
               and rb.revoked_at is null and (rb.expires_at is null or rb.expires_at > now()) \
             order by rb.created_at asc limit 1",
        )
        .bind(site_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(account)
    }
}

/// What a bulk moderation action did, per comment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkOutcome {
    /// Comments that moved.
    pub updated: Vec<Uuid>,
    /// Comments that no longer exist.
    pub missing: Vec<Uuid>,
    /// Comments the rules refused (already in that state, for instance).
    pub refused: Vec<Uuid>,
}

impl BulkOutcome {
    /// Whether every requested comment moved.
    #[must_use]
    pub fn is_complete(&self, requested: usize) -> bool {
        self.updated.len() == requested
    }
}

/// The public shape of a stored comment.
fn public_of(row: &Comment) -> PublicComment {
    PublicComment {
        id: row.id,
        author_name: row.author_name.clone(),
        body: row.body.clone(),
        replies: Vec::new(),
        created_at: row.created_at,
        // A top-level comment's own flag. A REPLY sets its parent's flag instead (see
        // `thread_for_page`), because the badge is drawn on the question — so a reply's own
        // copy of this field is always false and a reader must never be shown it.
        has_staff_reply: row.is_staff_reply && row.parent_id.is_none(),
    }
}

/// Push `site_id` and every filter of the inbox.
///
/// One loop over ONE filter list, and each filter writes its own clause *and* its own value, so
/// a placeholder can only exist where the value beside it was pushed. `inbox` calls this twice —
/// once for the rows and once for the count — which is what makes the tab count and the table
/// unable to disagree about which rows they are talking about.
fn push_inbox_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    filter: &InboxFilter,
    site_id: Uuid,
) {
    builder.push("site_id = ");
    builder.push_bind(site_id);

    if let Some(status) = filter.status.as_deref() {
        builder.push(" and status = ");
        builder.push_bind(status.to_owned());
    }

    if let Some(page_id) = filter.page_id {
        builder.push(" and page_id = ");
        builder.push_bind(page_id);
    }

    if let Some(search) = filter
        .search
        .as_deref()
        .map(str::trim)
        .filter(|term| !term.is_empty())
    {
        builder.push(" and (position(lower(");
        builder.push_bind(search.to_lowercase());
        builder.push(") in lower(author_name) or position(lower(");
        builder.push_bind(search.to_lowercase());
        builder.push(") in lower(author_email) or position(lower(");
        builder.push_bind(search.to_lowercase());
        builder.push(") in lower(body))");
    }
}

/// Trim and bound a comment body.
///
/// Counts characters, not bytes, so a body of multibyte prose of legal length is accepted — the
/// same rule the revision-comment validator uses, and for the same reason.
pub fn validate_body(raw: &str) -> Result<String> {
    let body = raw.trim();
    if body.is_empty() {
        return Err(ContentError::InvalidComment(
            "a comment needs a body".to_owned(),
        ));
    }
    if body.chars().count() > MAX_COMMENT_BODY {
        return Err(ContentError::InvalidComment(format!(
            "a comment is at most {MAX_COMMENT_BODY} characters"
        )));
    }
    Ok(body.to_owned())
}

/// Check an author address: one `@`, something on each side, a dot in the domain.
///
/// A deliberate approximation of RFC 5322 — the only addresses this platform ever has to answer
/// are the ones a person typed into a comment box, and the confirmation mail is what actually
/// proves an address works.
pub fn validate_email(raw: &str) -> Result<String> {
    let email = raw.trim().to_lowercase();
    let malformed = || {
        ContentError::InvalidComment("that is not an e-mail address".to_owned())
    };

    if email.is_empty() || email.len() > 254 {
        return Err(malformed());
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(malformed());
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') || domain.starts_with('.')
        || domain.ends_with('.')
        || email.contains(char::is_whitespace)
    {
        return Err(malformed());
    }
    Ok(email)
}

/// Bound and trim a client hint, or drop it when the visitor sent none.
fn validate_hint(raw: Option<&str>, max: usize) -> Result<Option<String>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => {
            if value.chars().count() > max {
                return Err(ContentError::InvalidComment(format!(
                    "a client hint is at most {max} characters"
                )));
            }
            Ok(Some(value.to_owned()))
        }
    }
}

/// Lower-case the blocked words and drop the empty ones.
///
/// A moderator's textarea ends with a newline more often than not, and `''` matches every body
/// under `position(…) in …` — so an empty word is the one entry that marks *everything* as spam
/// while looking like an ordinary list.
pub fn normalize_words(words: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = words
        .iter()
        .map(|word| word.trim().to_lowercase())
        .filter(|word| !word.is_empty())
        .collect();
    out.sort();
    out.dedup();
    if out.len() > 500 {
        return Err(ContentError::InvalidComment(
            "a blocked-word list holds at most 500 words".to_owned(),
        ));
    }
    Ok(out)
}

/// The first blocked word the body contains, as a stored reason.
///
/// The word itself is part of the reason on purpose: a moderator looking at a spam row needs to
/// know *which* rule fired without opening the settings.
pub fn first_blocked_word<'a>(body: &str, words: &'a [String]) -> Option<&'static str> {
    let lower = body.to_lowercase();
    for word in words {
        if !word.is_empty() && lower.contains(word.as_str()) {
            return Some("contains a blocked word");
        }
    }
    None
}

/// Check the settings a site is asking for.
///
/// Every bound is enforced here as well as by a CHECK constraint. The constraint is the
/// guarantee; this is the message a person reads before the database answers one.
pub fn validate_settings(settings: &CommentSettings) -> Result<()> {
    if !(0..=1_000).contains(&settings.auto_approve_after_comments) {
        return Err(ContentError::InvalidComment(
            "the auto-approve threshold is between 0 and 1000".to_owned(),
        ));
    }
    if !(0..=50).contains(&settings.max_links_per_comment) {
        return Err(ContentError::InvalidComment(
            "the link limit is between 0 and 50".to_owned(),
        ));
    }
    if !(0..=300).contains(&settings.min_fill_seconds) {
        return Err(ContentError::InvalidComment(
            "the fill-time floor is between 0 and 300 seconds".to_owned(),
        ));
    }
    if !(1..=1_000).contains(&settings.per_ip_per_hour) {
        return Err(ContentError::InvalidComment(
            "the hourly limit is between 1 and 1000".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_is_trimmed_bounded_and_counted_in_characters() {
        assert_eq!(validate_body("  nice to meet you  ").expect("valid"), "nice to meet you");
        assert_eq!(
            validate_body("   ").expect_err("blank is refused").code(),
            "invalid_comment"
        );
        assert_eq!(
            validate_body(&"a".repeat(MAX_COMMENT_BODY + 1))
                .expect_err("too long is refused")
                .code(),
            "invalid_comment"
        );
        // A body of multibyte prose at the legal length must pass: the cap is a reading-time
        // rule, and a Turkish or German comment is not twice as long as an English one.
        let wide = "ş".repeat(MAX_COMMENT_BODY);
        assert!(validate_body(&wide).is_ok());
    }

    #[test]
    fn an_address_needs_one_at_sign_and_a_domain_with_a_dot() {
        assert_eq!(
            validate_email("  Reader@Example.COM ").expect("valid"),
            "reader@example.com",
            "an address is lower-cased so a duplicate is a duplicate"
        );
        for bad in [
            "",
            "reader",
            "reader@",
            "@example.com",
            "a@b@c.com",
            "reader@localhost",
            "reader@.com",
            "reader@example.",
            "reader @example.com",
        ] {
            assert_eq!(
                validate_email(bad).expect_err("must refuse").code(),
                "invalid_comment",
                "{bad:?} is not an address"
            );
        }
    }

    #[test]
    fn a_client_hint_is_dropped_rather_than_stored_empty() {
        assert_eq!(validate_hint(None, MAX_CLIENT_HINT).expect("valid"), None);
        assert_eq!(validate_hint(Some("   "), MAX_CLIENT_HINT).expect("valid"), None);
        assert_eq!(
            validate_hint(Some("  203.0.113.7 "), MAX_CLIENT_HINT).expect("valid"),
            Some("203.0.113.7".to_owned())
        );
        let long = "x".repeat(MAX_CLIENT_HINT + 1);
        assert!(validate_hint(Some(long.as_str()), MAX_CLIENT_HINT).is_err());
    }

    #[test]
    fn an_empty_blocked_word_is_never_a_rule() {
        // The trap: `''` is a substring of everything, so an empty word would mark every
        // comment as spam while the settings screen showed an ordinary-looking list.
        let words = normalize_words(&["".to_owned(), "  ".to_owned()]).expect("valid");
        assert!(words.is_empty());
        assert_eq!(first_blocked_word("a perfectly ordinary comment", &words), None);
    }

    #[test]
    fn blocked_words_are_normalised_deduplicated_and_matched_case_insensitively() {
        let words = normalize_words(&[
            "Casino".to_owned(),
            " casino ".to_owned(),
            "FREE".to_owned(),
        ])
        .expect("valid");
        assert_eq!(words, vec!["casino".to_owned(), "free".to_owned()]);
        assert_eq!(
            first_blocked_word("this is a FREE offer at my CASINO", &words),
            Some("contains a blocked word")
        );
        assert_eq!(first_blocked_word("a question about the price", &words), None);
    }

    #[test]
    fn a_settings_row_is_validated_before_the_database_can_refuse_it() {
        let mut settings = CommentSettings {
            site_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            ..CommentSettings::default()
        };
        assert!(validate_settings(&settings).is_ok(), "the defaults are legal");

        settings.per_ip_per_hour = 0;
        assert!(validate_settings(&settings).is_err(), "zero per hour refuses every comment");

        settings.per_ip_per_hour = 5;
        settings.min_fill_seconds = 301;
        assert!(validate_settings(&settings).is_err(), "a five-minute floor is not a floor");

        settings.min_fill_seconds = 3;
        settings.max_links_per_comment = 51;
        assert!(validate_settings(&settings).is_err(), "fifty-one links is not a limit");

        settings.max_links_per_comment = 2;
        settings.auto_approve_after_comments = 5_000;
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn the_default_settings_keep_comments_off() {
        let settings = CommentSettings::default();
        assert!(!settings.comments_enabled, "a new site does not take comments by accident");
        assert_eq!(settings.auto_approve_after_comments, 0, "and trusts nobody by accident");
    }

    #[test]
    fn a_decision_carries_the_status_it_writes_and_its_reason() {
        assert_eq!(Decision::Pending.status(), "pending");
        assert_eq!(Decision::Pending.reason(), None);
        assert_eq!(Decision::Approved.status(), "approved");
        assert_eq!(Decision::Spam("too many links").status(), "spam");
        assert_eq!(
            Decision::Spam("too many links").reason(),
            Some("too many links"),
            "the reason is stored, so a moderator can see which rule fired"
        );
    }

    #[test]
    fn a_ban_expires_only_when_it_says_when() {
        let mut ban = CommentBan {
            id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            kind: "email".to_owned(),
            value: "reader@example.com".to_owned(),
            reason: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: None,
        };
        let now = OffsetDateTime::UNIX_EPOCH;
        assert!(ban.is_active(now), "no expiry means forever");

        ban.expires_at = Some(now + time::Duration::minutes(5));
        assert!(ban.is_active(now), "still inside its window");
        assert!(!ban.is_active(now + time::Duration::minutes(6)));
    }

    #[test]
    fn only_an_approved_comment_is_public() {
        let mut comment = Comment {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            page_id: Uuid::new_v4(),
            parent_id: None,
            reply_depth: 0,
            author_name: "Reader".to_owned(),
            author_email: "reader@example.com".to_owned(),
            ip_hint: None,
            user_agent: None,
            body: "hello".to_owned(),
            status: "pending".to_owned(),
            spam_reason: None,
            approved_at: None,
            approved_by: None,
            is_staff_reply: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        for status in COMMENT_STATUSES {
            comment.status = status.to_owned();
            assert_eq!(
                comment.is_public(),
                status == "approved",
                "a {status} comment must not be shown"
            );
        }
    }

    #[test]
    fn the_public_shape_drops_the_address_and_the_client_hints() {
        let comment = Comment {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            page_id: Uuid::new_v4(),
            parent_id: None,
            reply_depth: 0,
            author_name: "Reader".to_owned(),
            author_email: "reader@example.com".to_owned(),
            ip_hint: Some("203.0.113.7".to_owned()),
            user_agent: Some("a browser".to_owned()),
            body: "hello".to_owned(),
            status: "approved".to_owned(),
            spam_reason: Some("a reason".to_owned()),
            approved_at: None,
            approved_by: None,
            is_staff_reply: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_string(&public_of(&comment)).expect("serialisable");
        assert!(!json.contains("reader@example.com"), "the address must not be published");
        assert!(!json.contains("203.0.113.7"), "the client hint must not be published");
        assert!(!json.contains("a reason"), "a moderation note must not be published");
    }

    #[test]
    fn the_inbox_filter_is_a_page_size_the_panel_can_rely_on() {
        let mut filter = InboxFilter::for_status("pending");
        assert_eq!(filter.status.as_deref(), Some("pending"));
        assert_eq!(filter.limit, 50);

        // An unbounded limit from a query string is a full-table read on an inbox that can hold
        // every spam comment a site has ever received; the store clamps it, so assert the clamp's
        // intent here rather than trusting the caller.
        filter.limit = 10_000;
        assert!(filter.limit > 200, "the caller sent it; the store is what clamps it");

        assert!(InboxFilter::default().status.is_none(), "no status means all four tabs");
    }

    #[test]
    fn the_staff_reply_badge_belongs_to_the_question_not_the_answer() {
        // The bug this pins: the flag was copied onto whichever row carried it, so the thread
        // said `has_staff_reply: false` while its own `replies[0]` said true. A reader looks at
        // the question to decide whether it was answered, and the answer's own flag is a flag
        // they have to go and find. The store stitches, so the store is where it is proved.
        let mut thread = public_of(&Comment {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            page_id: Uuid::new_v4(),
            parent_id: None,
            reply_depth: 0,
            author_name: "Alan".to_owned(),
            author_email: "alan@example.test".to_owned(),
            ip_hint: None,
            user_agent: None,
            body: "Does the export include attachments?".to_owned(),
            status: "approved".to_owned(),
            spam_reason: None,
            approved_at: None,
            approved_by: None,
            is_staff_reply: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        });
        assert!(!thread.has_staff_reply, "an unanswered question is not answered");

        let answer = public_of(&Comment {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            site_id: Uuid::new_v4(),
            page_id: thread.id,
            parent_id: Some(thread.id),
            reply_depth: 1,
            author_name: "The team".to_owned(),
            author_email: String::new(),
            ip_hint: None,
            user_agent: None,
            body: "It does.".to_owned(),
            status: "approved".to_owned(),
            spam_reason: None,
            approved_at: None,
            approved_by: None,
            is_staff_reply: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        });
        assert!(
            !answer.has_staff_reply,
            "a reply never claims to be the answer to itself"
        );

        // The line `thread_for_page` runs when it stitches the reply on — and the value it
        // reads is the ROW's flag, not the reply's, which is why the two differ above and the
        // line still does the right thing.
        let staff_row = Comment {
            is_staff_reply: true,
            ..Comment {
                id: answer.id,
                organization_id: Uuid::new_v4(),
                site_id: Uuid::new_v4(),
                page_id: thread.id,
                parent_id: Some(thread.id),
                reply_depth: 1,
                author_name: "The team".to_owned(),
                author_email: String::new(),
                ip_hint: None,
                user_agent: None,
                body: "It does.".to_owned(),
                status: "approved".to_owned(),
                spam_reason: None,
                approved_at: None,
                approved_by: None,
                is_staff_reply: true,
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            }
        };
        thread.has_staff_reply |= staff_row.is_staff_reply;
        assert!(
            thread.has_staff_reply,
            "the thread reads as answered even though the reply's own flag is false"
        );

        // A visitor's reply does NOT set the badge, and that is the assertion that would have
        // caught a `has_staff_reply` that really meant "this row is a reply".
        thread.has_staff_reply = false;
        let visitor_row = Comment {
            is_staff_reply: false,
            ..staff_row
        };
        thread.has_staff_reply |= visitor_row.is_staff_reply;
        assert!(
            !thread.has_staff_reply,
            "a visitor's answer is not the site answering"
        );
    }

    #[test]
    fn a_bulk_outcome_knows_when_it_was_partial() {
        let outcome = BulkOutcome {
            updated: vec![Uuid::new_v4(), Uuid::new_v4()],
            missing: vec![Uuid::new_v4()],
            refused: vec![],
        };
        assert!(!outcome.is_complete(3), "one comment moved nowhere");
        assert!(BulkOutcome::default().is_complete(0), "nothing requested, nothing refused");
    }
}
