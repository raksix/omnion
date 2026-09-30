//! The draft store: `ai_workflow_drafts` as rows (docs/requests/REQ-046, slice 1).
//!
//! Three invariants are kept here rather than in the routes, because every write path has to
//! honour them and a route that repeats a rule is a route that can forget it:
//!
//! * **A status is never written that the row's check would refuse.** The check is the
//!   authority (`ai_workflow_drafts_status_check`); this store answers the same question
//!   first so a bad value arrives as a `400` naming the value rather than as a `500` from a
//!   constraint violation nobody can act on.
//! * **A `rejected` draft carries a reason.** "Rejected" with nothing next to it is a
//!   decision no reviewer can learn from, and the review screen would render an empty
//!   sentence rather than a gap.
//! * **A decision is written with its actor and its time, or not at all.** `decided_by` and
//!   `decided_at` move together, so a row can never say "rejected" with nobody's name on it.

use sqlx::{PgPool, QueryBuilder};
use uuid::Uuid;

use crate::error::{AiWorkflowError, Result};
use crate::model::{
    AiWorkflowDraft, DraftFilter, NewDraft, STATUSES, draft_status_is_known,
};

/// Columns read back from `ai_workflow_drafts`.
const COLUMNS: &str = "id, organization_id, site_id, title, prompt, rationale, definition, \
     status, workflow_id, model_key, tokens_input, tokens_output, error, revision_note, \
     revision_count, created_by, decided_by, decision_reason, created_at, updated_at, decided_at";

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Write the row a generation starts from.
///
/// The row is written **before** the provider is called, so a generation that dies mid-flight
/// leaves a `failed` row rather than nothing at all: "we tried this and here is why it did
/// not work" is the difference between a support ticket that can be answered and one that
/// asks the customer to try again.
pub async fn insert_draft(pool: &PgPool, new: NewDraft) -> Result<AiWorkflowDraft> {
    require_known_status(&new.status)?;
    let prompt = require_prompt(&new.prompt)?;
    let title = require_title(&new.title)?;

    let sql = format!(
        "insert into ai_workflow_drafts \
         (organization_id, site_id, title, prompt, rationale, definition, status, model_key, \
          tokens_input, tokens_output, error, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning {COLUMNS}"
    );
    let draft: AiWorkflowDraft = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(&title)
        .bind(&prompt)
        .bind(new.rationale.as_deref())
        .bind(new.definition.as_ref())
        .bind(&new.status)
        .bind(new.model_key.as_deref())
        .bind(new.tokens_input)
        .bind(new.tokens_output)
        .bind(new.error.as_deref())
        .bind(new.created_by)
        .fetch_one(pool)
        .await?;
    Ok(draft)
}

/// Record a validated answer on the row that was already there.
///
/// The update is **conditional on the row still being `generating`** and the clause is in the
/// `where`, not in Rust: two generations racing on one draft would otherwise both read
/// `generating`, both write, and the second answer would silently overwrite the first — with
/// the audit trail showing one draft created and two answers stored for it. A lost race
/// returns `None` and the caller treats the row as somebody else's now.
pub async fn apply_answer(
    pool: &PgPool,
    id: Uuid,
    title: &str,
    rationale: Option<&str>,
    definition: &serde_json::Value,
    model_key: &str,
    tokens_input: Option<i32>,
    tokens_output: Option<i32>,
) -> Result<Option<AiWorkflowDraft>> {
    let title = require_title(title)?;
    let sql = format!(
        "update ai_workflow_drafts set title = $2, rationale = $3, definition = $4, \
         model_key = $5, tokens_input = $6, tokens_output = $7, status = 'draft', error = null, \
         updated_at = now() \
         where id = $1 and status = 'generating' returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(&title)
        .bind(rationale)
        .bind(definition)
        .bind(model_key)
        .bind(tokens_input)
        .bind(tokens_output)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// Record that generation ran out of answers.
///
/// The reason is stored on the row rather than in a log line: the review screen's error state
/// reads this column, and a failure only visible in a log is a failure the operator cannot
/// see from where they hit it.
pub async fn apply_failure(pool: &PgPool, id: Uuid, error: &str) -> Result<Option<AiWorkflowDraft>> {
    let sql = format!(
        "update ai_workflow_drafts set status = 'failed', error = $2, updated_at = now() \
         where id = $1 and status = 'generating' returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(error)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// Record a revision prompt against a draft that is not decided.
///
/// `revision_note` and `revision_count` move in the same statement as the status reset to
/// `generating`: a revision that changed the prompt but left the counter at zero would make
/// the spend line claim one answer where two were paid for.
pub async fn apply_revision(
    pool: &PgPool,
    id: Uuid,
    note: &str,
    model_key: &str,
) -> Result<Option<AiWorkflowDraft>> {
    let note = note.trim();
    if note.is_empty() {
        return Err(AiWorkflowError::invalid(
            "invalid_revision_note",
            "asking for changes needs a note saying what to change",
        ));
    }
    let sql = format!(
        "update ai_workflow_drafts set revision_note = $2, revision_count = revision_count + 1, \
         status = 'generating', decided_by = null, decision_reason = null, decided_at = null, \
         model_key = $3, updated_at = now() \
         where id = $1 and status in ('draft', 'failed') returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(note)
        .bind(model_key)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// Record a decision on a draft that has not been decided yet.
///
/// `status` is `approved` or `rejected` — checked here rather than left to the row, because
/// this is the one write where an unknown value would be *accepted* by the constraint check
/// (both are in the status list) and only fail later, when the workflow it claims to have
/// materialised is looked up.
pub async fn apply_decision(
    pool: &PgPool,
    id: Uuid,
    status: &str,
    decided_by: Uuid,
    reason: Option<&str>,
) -> Result<Option<AiWorkflowDraft>> {
    let reason = check_decision(status, reason)?;

    let sql = format!(
        "update ai_workflow_drafts set status = $2, decided_by = $3, decision_reason = $4, \
         decided_at = now(), updated_at = now() \
         where id = $1 and status in ('draft', 'failed') returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(status)
        .bind(decided_by)
        .bind(reason)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// What a decision is allowed to be, checked before a row is touched.
///
/// Pure, so the rule is testable without a database — and it has to be testable, because this
/// is the one write where a wrong value is **accepted by the row**: `draft` is in
/// `ai_workflow_drafts_status_check`, so an unprobed `apply_decision(_, "draft", …)` would
/// store a status that means "the model answered, nobody has decided" while claiming somebody
/// had. The failure would surface a click later, as a draft with no definition that says it
/// was approved.
///
/// The rejection reason rule lives here for the same reason: `decision_reason` is `null` for
/// an approval by design (there is nothing to say), but an approval that someone typed a
/// comment into keeps it, because the store does not own what an operator wrote.
fn check_decision<'a>(status: &str, reason: Option<&'a str>) -> Result<Option<&'a str>> {
    match status {
        "approved" | "rejected" => {}
        other => {
            return Err(AiWorkflowError::invalid(
                "invalid_decision_status",
                format!(
                    "a decision is `approved` or `rejected`, not `{other}`; a draft is one of: {}",
                    STATUSES.join(", ")
                ),
            ));
        }
    }
    let reason = reason.map(str::trim).filter(|value| !value.is_empty());
    if status == "rejected" && reason.is_none() {
        return Err(AiWorkflowError::invalid(
            "rejection_reason_required",
            "rejecting a draft needs a reason the person who asked for it can read",
        ));
    }
    Ok(reason)
}

/// Point an approved draft at the workflow it materialised.
///
/// One write that carries the id and moves the status to `activated`, so a draft cannot read
/// "approved, with a workflow" while its status still says `approved` because the second
/// statement failed. The `where` names `workflow_id is null` for the same reason the answer
/// write names a status: the caller must not attach a second workflow to a draft that
/// already has one.
pub async fn attach_workflow(pool: &PgPool, id: Uuid, workflow_id: Uuid) -> Result<Option<AiWorkflowDraft>> {
    let sql = format!(
        "update ai_workflow_drafts set workflow_id = $2, status = 'activated', updated_at = now() \
         where id = $1 and workflow_id is null and status in ('approved', 'activated') \
         returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(workflow_id)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// Replace the definition of a draft an operator edited.
///
/// Revalidated by the caller against the engine (this store does not know the definition
/// shape), and conditional on `status = 'draft'`: an approved or rejected draft whose
/// definition changed after the decision would leave the decision describing a different
/// rule than the one stored beside it.
pub async fn replace_definition(
    pool: &PgPool,
    id: Uuid,
    definition: &serde_json::Value,
) -> Result<Option<AiWorkflowDraft>> {
    let sql = format!(
        "update ai_workflow_drafts set definition = $2, updated_at = now() \
         where id = $1 and status = 'draft' returning {COLUMNS}"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(definition)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// Delete a draft.
///
/// Deleting the row never touches the workflow it produced: the two are separate objects with
/// separate permissions, and a draft that could switch a running rule off would be a delete
/// with a second, invisible effect.
pub async fn delete_draft(pool: &PgPool, id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from ai_workflow_drafts where id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// One draft by id.
pub async fn find_draft(pool: &PgPool, id: Uuid) -> Result<Option<AiWorkflowDraft>> {
    let sql = format!("select {COLUMNS} from ai_workflow_drafts where id = $1");
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql).bind(id).fetch_optional(pool).await?;
    Ok(draft)
}

/// One draft of one organization, or `None`.
///
/// The organization is in the `where` and not applied afterwards, so a draft of another
/// tenant is *absent* rather than found-and-refused. A handler that distinguishes the two
/// answers would let a caller learn that an id exists anywhere in the platform, which is the
/// question a scoped read must not answer.
pub async fn find_draft_in(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<AiWorkflowDraft>> {
    let sql = format!(
        "select {COLUMNS} from ai_workflow_drafts where id = $1 and organization_id = $2"
    );
    let draft: Option<AiWorkflowDraft> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(draft)
}

/// One page of one organization's drafts, newest first.
///
/// The `where` clause is built **once** by [`push_filter`] and used by both the count and the
/// page. That is not tidiness: the count answers "12 drafts" above a list showing one row when
/// a filter is applied, and the first thing an operator concludes is that the filter lost
/// something rather than that it worked.
pub async fn list_drafts(
    pool: &PgPool,
    organization_id: Uuid,
    filter: &DraftFilter,
) -> Result<crate::model::DraftPage> {
    let mut count = QueryBuilder::new("select count(*) from ai_workflow_drafts where 1 = 1");
    let mut rows = QueryBuilder::new(format!("select {COLUMNS} from ai_workflow_drafts where 1 = 1"));
    push_filter(&mut count, organization_id, filter);
    push_filter(&mut rows, organization_id, filter);

    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;

    rows.push(" order by created_at desc, id desc");
    rows.push(" limit ").push_bind(filter.limit.max(0));
    rows.push(" offset ").push_bind(filter.offset.max(0));

    let drafts: Vec<AiWorkflowDraft> = rows.build_query_as().fetch_all(pool).await?;
    Ok(crate::model::DraftPage { drafts, total })
}

/// Append one organization's filtered rows to a `where` already true.
///
/// Every predicate is a bind, never an interpolated value: the search text reaches Postgres as
/// a parameter, so a prompt containing a quote is a search term rather than a syntax error.
/// The status list is the reason this is a helper — it is the one filter with a variable
/// number of values, and building it into a `QueryBuilder` that was then *not* extended (as
/// the first version of this function did) is a filter that silently does nothing while
/// looking exactly like the others.
fn push_filter(
    query: &mut QueryBuilder<'_, sqlx::Postgres>,
    organization_id: Uuid,
    filter: &DraftFilter,
) {
    query.push(" and organization_id = ").push_bind(organization_id);

    let known: Vec<String> = filter
        .statuses
        .iter()
        .filter(|status| crate::model::draft_status_is_known(status))
        .cloned()
        .collect();
    if !known.is_empty() {
        // An unknown status is dropped rather than bound: the row's check would refuse the
        // value anyway, and a filter chip that yields an empty list reads as "no drafts yet".
        // The values are collected into owned `String`s so the builder's lifetime is its own:
        // holding `&str` borrowed from the filter would tie the two together and make the
        // helper unusable from a caller that builds a filter on the fly.
        query.push(" and status in (");
        let mut separated = query.separated(", ");
        for status in known {
            separated.push_bind(status);
        }
        query.push(")");
    }

    if let Some(text) = filter
        .query
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        // Both columns because an operator remembers the words they typed far more often than
        // the name the model gave them — and a search that only reads the title answers "no
        // results" for the prompt that is still in their clipboard.
        let pattern = format!("%{}%", escape_like(text));
        query.push(" and (title ilike ").push_bind(pattern.clone());
        query.push(" or prompt ilike ").push_bind(pattern).push(")");
    }
    if let Some(created_by) = filter.created_by {
        query.push(" and created_by = ").push_bind(created_by);
    }
}

/// Every distinct author of this organization's drafts, with how many each has.
///
/// The console's "created by" filter is a select, and a select of names an operator cannot
/// predict is a select they cannot use — so the list comes from the rows rather than from a
/// fixed roster, and the count travels with it so an empty option can show "0 drafts".
pub async fn list_authors(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<(Uuid, i64)>> {
    let authors = sqlx::query_as::<_, (Uuid, i64)>(
        "select created_by, count(*) from ai_workflow_drafts \
         where organization_id = $1 and created_by is not null \
         group by created_by order by count(*) desc, created_by",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    Ok(authors)
}

// ---------------------------------------------------------------------------------------------
// Rules the store answers before the database does
// ---------------------------------------------------------------------------------------------

/// Refuse a status the row's constraint would refuse, with the whole list next to it.
///
/// The error names the value *and* the vocabulary, because the caller of this function is a
/// handler writing a status from a body field, and "invalid input value for enum" from the
/// database tells them nothing about what to send instead.
fn require_known_status(status: &str) -> Result<()> {
    if draft_status_is_known(status) {
        return Ok(());
    }
    Err(AiWorkflowError::invalid(
        "invalid_draft_status",
        format!(
            "`{status}` is not a draft status; a draft is one of: {}",
            STATUSES.join(", ")
        ),
    ))
}

/// A prompt is trimmed before it is stored, and a blank one is refused.
///
/// Trimming is what makes the length checks meaningful: `"   "` is three characters long and
/// carries no request, so a check on the raw length would store it and the generator would be
/// handed nothing while the row said it had a prompt.
fn require_prompt(prompt: &str) -> Result<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return Err(AiWorkflowError::invalid(
            "invalid_prompt",
            "describe the workflow you want — an empty prompt cannot be answered",
        ));
    }
    if trimmed.chars().count() > crate::model::MAX_PROMPT_LEN {
        return Err(AiWorkflowError::invalid(
            "invalid_prompt",
            format!(
                "a prompt is at most {} characters",
                crate::model::MAX_PROMPT_LEN
            ),
        ));
    }
    Ok(trimmed.to_owned())
}

/// The title is trimmed and bounded the same way.
fn require_title(title: &str) -> Result<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(AiWorkflowError::invalid(
            "invalid_title",
            "a draft needs a title an operator can recognise in a list",
        ));
    }
    if trimmed.chars().count() > crate::model::MAX_TITLE_LEN {
        return Err(AiWorkflowError::invalid(
            "invalid_title",
            format!(
                "a title is at most {} characters",
                crate::model::MAX_TITLE_LEN
            ),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Escape the wildcards a `ilike` pattern carries.
///
/// `DraftFilter::query` is compared with `ilike`, so a `%` or `_` in it would widen the match
/// to the whole table and the console's free-text search would return every draft. Escaping
/// here rather than in the route means no caller can forget it, because there is no other way
/// to write the clause.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_the_constraint_would_refuse_is_refused_here_first() {
        require_known_status("draft").expect("a real status is accepted");
        require_known_status(crate::model::NEW_DRAFT_STATUS).expect("the opening status is accepted");

        let error = require_known_status("pending").expect_err("a status nobody stores is refused");
        assert_eq!(error.code(), "invalid_draft_status");
        // The refusal has to name the vocabulary, or the caller is left guessing.
        assert!(error.to_string().contains("rejected"), "{error}");
    }

    #[test]
    fn a_blank_prompt_is_refused_and_a_padded_one_is_trimmed() {
        let error = require_prompt("   \n\t ").expect_err("whitespace is not a request");
        assert_eq!(error.code(), "invalid_prompt");

        assert_eq!(
            require_prompt("  email the customer  ").expect("a padded prompt is accepted"),
            "email the customer"
        );
    }

    #[test]
    fn the_prompt_bound_counts_characters_not_bytes() {
        let long = "ş".repeat(crate::model::MAX_PROMPT_LEN + 1);
        let error = require_prompt(&long).expect_err("one character over is refused");
        assert_eq!(error.code(), "invalid_prompt");

        let at_bound = "ş".repeat(crate::model::MAX_PROMPT_LEN);
        assert!(require_prompt(&at_bound).is_ok(), "the bound itself is accepted");
    }

    #[test]
    fn a_blank_title_is_refused() {
        assert_eq!(
            require_title(" ").expect_err("a title of whitespace is refused").code(),
            "invalid_title"
        );
        assert_eq!(require_title(" Invoice chase ").expect("accepted"), "Invoice chase");
    }

    #[test]
    fn a_like_pattern_cannot_widen_a_search() {
        // The escape is the whole reason `list_drafts` cannot be turned into "return every
        // draft" by typing a percent sign into the console's search box.
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
        // A literal backslash-first order matters: escaping the backslash last would
        // double-escape the backslashes this function itself inserted.
        assert_eq!(escape_like("%_\\"), "\\%\\_\\\\");
    }

    #[test]
    fn a_decision_is_approved_or_rejected_and_nothing_else() {
        // The case worth having is `draft`: the row's status check *accepts* it, so this is
        // the one wrong value a check against the vocabulary alone would let through.
        for status in ["draft", "generating", "failed", "activated", ""] {
            let error = check_decision(status, Some("because")).expect_err(status);
            assert_eq!(error.code(), "invalid_decision_status", "{status}");
        }
        assert!(check_decision("approved", None).is_ok());
        assert!(check_decision("rejected", Some("too broad")).is_ok());
    }

    #[test]
    fn a_rejection_without_a_reason_is_refused_before_the_row_is_touched() {
        let error = check_decision("rejected", None).expect_err("a bare rejection is refused");
        assert_eq!(error.code(), "rejection_reason_required");

        // Whitespace is not a reason either — this is the case a `!is_empty()` check written
        // against the raw string misses, and it renders as a review screen with an empty
        // sentence where the decision should be.
        let error = check_decision("rejected", Some("   \n ")).expect_err("so is blank prose");
        assert_eq!(error.code(), "rejection_reason_required");
    }

    #[test]
    fn an_approval_keeps_what_an_operator_typed_and_a_rejection_trims_it() {
        assert_eq!(check_decision("approved", None).expect("accepted"), None);
        assert_eq!(
            check_decision("rejected", Some("  two invoices, one address  ")).expect("accepted"),
            Some("two invoices, one address")
        );
    }
}
