//! Comments on a content revision: what a person — or an automation — noted about a version.
//!
//! A revision is immutable (docs/05-VERSIONING.md §4): editing it would break the history the
//! compare/restore flow is built on. What a reviewer needs instead is a place to *say* something
//! about a version, and that is a separate row: the comment points at the revision and carries
//! its own author and time. `source` says where it came from, so the panel can tell a colleague's
//! note apart from one an automation left behind (the `comment_revision` action, P13).

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};

/// Longest comment body the platform accepts.
pub const MAX_COMMENT_BODY: usize = 2000;

/// Columns of `page_revision_comments`, in the order [`RevisionComment`] expects.
pub const COMMENT_COLUMNS: &str =
    "id, organization_id, revision_id, author_user_id, source, body, created_at";

/// Who wrote a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSource {
    /// A signed-in account wrote it.
    User,
    /// An automation wrote it while running a workflow.
    Automation,
}

impl CommentSource {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Automation => "automation",
        }
    }

    /// Parse a stored value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "automation" => Some(Self::Automation),
            _ => None,
        }
    }
}

/// One stored comment.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct RevisionComment {
    /// Comment id.
    pub id: Uuid,
    /// Organization the revision's site belongs to.
    pub organization_id: Uuid,
    /// Revision the comment is about.
    pub revision_id: Uuid,
    /// Account that wrote it; `None` for an automation.
    pub author_user_id: Option<Uuid>,
    /// `user` or `automation`.
    pub source: String,
    /// The note itself.
    pub body: String,
    /// When it was written.
    pub created_at: OffsetDateTime,
}

impl RevisionComment {
    /// The parsed source.
    #[must_use]
    pub fn source(&self) -> Option<CommentSource> {
        CommentSource::parse(&self.source)
    }
}

/// A comment to be written.
#[derive(Debug, Clone)]
pub struct NewRevisionComment {
    /// Organization the revision belongs to (carried on the row so audits need no join).
    pub organization_id: Uuid,
    /// Revision the comment is about.
    pub revision_id: Uuid,
    /// Account that writes it; must be `None` for [`CommentSource::Automation`].
    pub author_user_id: Option<Uuid>,
    /// Who writes it.
    pub source: CommentSource,
    /// The note itself.
    pub body: String,
}

/// Write one comment on a revision.
///
/// The revision must exist — a comment on nothing is refused with `revision_not_found` rather
/// than surfacing a foreign-key error from the database.
pub async fn add(pool: &PgPool, new: NewRevisionComment) -> Result<RevisionComment> {
    let body = validate_body(&new.body)?;
    validate_author(new.source, new.author_user_id)?;

    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from page_revisions where id = $1)")
            .bind(new.revision_id)
            .fetch_one(pool)
            .await?;
    if !exists {
        return Err(ContentError::RevisionNotFound);
    }

    let sql = format!(
        "insert into page_revision_comments \
         (organization_id, revision_id, author_user_id, source, body) \
         values ($1, $2, $3, $4, $5) returning {COMMENT_COLUMNS}"
    );

    let comment: RevisionComment = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.revision_id)
        .bind(new.author_user_id)
        .bind(new.source.as_str())
        .bind(body)
        .fetch_one(pool)
        .await?;

    Ok(comment)
}

/// Comments of one revision, oldest first.
pub async fn list_for_revision(
    pool: &PgPool,
    revision_id: Uuid,
    limit: i64,
) -> Result<Vec<RevisionComment>> {
    let sql = format!(
        "select {COMMENT_COLUMNS} from page_revision_comments \
         where revision_id = $1 order by created_at asc, id limit $2"
    );

    let comments: Vec<RevisionComment> = sqlx::query_as(&sql)
        .bind(revision_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    Ok(comments)
}

/// Trim and bound a comment body.
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

/// Check "who wrote this" against the shape the schema holds.
///
/// An automation has no account behind it and a person always does; the database has the same
/// rule as a constraint, so a mismatch is caught here with a readable message instead of a
/// constraint violation.
pub fn validate_author(source: CommentSource, author_user_id: Option<Uuid>) -> Result<()> {
    match (source, author_user_id) {
        (CommentSource::Automation, None) | (CommentSource::User, Some(_)) => Ok(()),
        (CommentSource::Automation, Some(_)) => Err(ContentError::InvalidComment(
            "a comment written by an automation carries no author account".to_owned(),
        )),
        (CommentSource::User, None) => Err(ContentError::InvalidComment(
            "a comment written by an account needs the author's account id".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_round_trips() {
        for source in [CommentSource::User, CommentSource::Automation] {
            assert_eq!(CommentSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(CommentSource::parse("robot"), None);
    }

    #[test]
    fn a_body_is_trimmed_and_bounded() {
        assert_eq!(
            validate_body("  release notes  ").expect("valid"),
            "release notes"
        );
        assert_eq!(
            validate_body("   ").expect_err("blank is refused").code(),
            "invalid_comment"
        );

        let long = "a".repeat(MAX_COMMENT_BODY + 1);
        assert_eq!(
            validate_body(&long)
                .expect_err("too long is refused")
                .code(),
            "invalid_comment"
        );
        // The cap counts characters, not bytes: a body of multibyte characters of legal length
        // must pass.
        let wide = "ç".repeat(MAX_COMMENT_BODY);
        assert!(validate_body(&wide).is_ok());
    }

    #[test]
    fn the_author_shape_matches_the_source() {
        let account = Uuid::new_v4();
        assert!(validate_author(CommentSource::User, Some(account)).is_ok());
        assert!(validate_author(CommentSource::Automation, None).is_ok());
        assert!(validate_author(CommentSource::Automation, Some(account)).is_err());
        assert!(validate_author(CommentSource::User, None).is_err());
    }
}
