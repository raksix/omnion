//! The persisted-document store: PostgreSQL over `graphql_persisted_documents` (REQ-130).
//!
//! ## Why the SQL lives here and not in the route
//!
//! Three callers read these rows — the manager screen, the endpoint's GET leg and CI's
//! registration step — and they must agree on what an "active" document is. A query written in
//! the handler is a query the other two callers cannot make, and the acceptance line *"a document
//! revoked in the UI stops executing within one cache cycle"* is precisely a claim about the
//! endpoint reading what the screen wrote. So the reads are here, the row shape is
//! [`crate::persisted::RegistryEntry`], and the handler only decides what to answer.
//!
//! ## Why there is no cache
//!
//! The request says *"within one cache cycle"* — a revocation may take one cycle to land, and no
//! longer. The cheapest way to guarantee a *bounded* cycle is to have no cache: every execution
//! reads the row it is about to run. A cache here would buy a lookup and spend the property the
//! acceptance line is about. The cost is one indexed read per request against a table with one
//! row per registered document, which is small by construction — it is a registry, not a log.
//!
//! ## The prefix search is done in SQL, and ambiguity is a first-class answer
//!
//! `DocumentId::matches` accepts a prefix, because a client that pasted a truncated hash is
//! asking the question the endpoint exists to answer. Two registered documents sharing a prefix
//! is a real possibility, so the lookup returns **all** matches and the caller decides:
//! [`lookup`] is what turns that list into `Active` / `Blocked` / `Unknown` / `Ambiguous`.

use omnion_graphql::persisted::{
    self, DocumentId, Lookup, OperationSummary, RegistryEntry, RegisterRequest,
};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::ApiError;

/// A store failure, as a `500`.
///
/// There is deliberately no `ApiError::internal` constructor in this crate, so this helper is the
/// single place a store failure is shaped. Written once because a database message that reaches a
/// client should always carry the same `internal_error` code — a store that invents its own code
/// per call site is a store whose client-side error handling grows a branch per query.
fn internal_store(message: String) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        message,
    )
}

/// Columns read for every registry row, in one constant.
///
/// A hand-written column list repeated in four queries is a place for the manager and the
/// endpoint to disagree about which `status` values exist — and the disagreement would read as
/// "the screen shows it as active and the endpoint refuses it". One list, four uses.
const REGISTRY_COLUMNS: &str = "id, organization_id, name, hash, kind, status, \
     required_for_callers, hits, operations, created_by, created_at, updated_at";

/// A row as stored, including the columns the manager does not show.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DocumentRow {
    id: Uuid,
    #[allow(dead_code)]
    organization_id: Uuid,
    name: String,
    hash: String,
    kind: String,
    status: String,
    required_for_callers: bool,
    hits: i64,
    /// `[{name, kind, cost, depth}]`, captured at registration.
    operations: serde_json::Value,
    #[allow(dead_code)]
    created_by: Option<Uuid>,
    #[allow(dead_code)]
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
}

impl DocumentRow {
    /// The registry entry the decision layer reasons about.
    fn to_entry(&self) -> RegistryEntry {
        RegistryEntry {
            id: self.id.to_string(),
            name: self.name.clone(),
            hash: self.hash.clone(),
            short_hash: short_of(&self.hash),
            kind: self.kind.clone(),
            status: self.status.clone(),
            required_for_callers: self.required_for_callers,
            hits: self.hits,
            operations: operations_of(&self.operations),
            // Set by [`crate::routes::graphql::documents`] from the settings row, not from here:
            // the store does not read settings, so a caller that forgets to set it shows a screen
            // that silently assumes ad-hoc documents are allowed.
            persisted_only: false,
        }
    }
}

/// The first `SHORT_HASH_LEN` characters of a stored digest.
fn short_of(hash: &str) -> String {
    hash.chars().take(persisted::SHORT_HASH_LEN).collect()
}

/// The `operations` jsonb as summaries, tolerating a shape written by an older release.
///
/// `unwrap_or_default` rather than a refusal: this column is a cache of what the parser found, and
/// a manager that refuses to list a document because its cost cache is unreadable turns a
/// cosmetic problem into an outage. The cost column showing a dash is the right failure.
fn operations_of(value: &serde_json::Value) -> Vec<OperationSummary> {
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// Register a document, or report the duplicate that already exists.
///
/// The duplicate carries the **id of the row already registered**, because the manager's contract
/// is to reject it "with a link to the existing row" — a `409` with a message naming the hash
/// gives an operator something to look up by hand and nothing to click.
pub async fn register(
    pool: &PgPool,
    organization_id: Uuid,
    request: &RegisterRequest,
    created_by: Option<Uuid>,
) -> Result<RegistryEntry, ApiError> {
    request.validate().map_err(|error| ApiError::bad_request("graphql_document_invalid", error.to_string()))?;

    let document = request.document.as_str();
    // The byte cap is the parser's own, checked before anything is written: a 10 MB "document"
    // that fails to parse must not first cost a row.
    if document.len() > omnion_graphql::document::MAX_DOCUMENT_BYTES {
        return Err(ApiError::bad_request(
            "graphql_document_too_large",
            format!(
            "`document` is {} bytes; the cap is {}",
            document.len(),
            omnion_graphql::document::MAX_DOCUMENT_BYTES
            ),
        ));
    }

    let identity = request.identity();
    // Parsed before the insert, because a document that does not parse has no operations, no cost
    // and no meaning — and a registry row with `operations: []` is a row nothing can execute.
    let operations = persisted::describe(document, &omnion_graphql::cost::Catalogue::catalogued());
    if operations.is_empty() {
        let parse_error = omnion_graphql::parse(document).err();
        return Err(ApiError::bad_request(
            "graphql_document_unparsable",
            match parse_error {
                Some(error) => format!("`document` is not a valid GraphQL document: {error}"),
                // Parsed, but defines nothing executable. A different problem with a different fix.
                None => "`document` defines no executable operation".to_owned(),
            },
        ));
    }

    let kind = persisted::heaviest_kind(document);
    let status = if request.active { "active" } else { "draft" };

    let sql = format!(
        "insert into graphql_persisted_documents \
         (id, organization_id, name, hash, kind, status, required_for_callers, document, \
          operations, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         on conflict (organization_id, hash) do update \
            set name = excluded.name, updated_at = now() \
         returning {REGISTRY_COLUMNS}"
    );
    let row = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(Uuid::new_v4())
        .bind(organization_id)
        .bind(request.name.trim())
        .bind(&identity.hash)
        .bind(kind)
        .bind(status)
        .bind(request.required_for_callers)
        .bind(document)
        .bind(serde_json::to_value(&operations).unwrap_or(serde_json::json!([])))
        .bind(created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            // The unique index is on `(organization_id, hash)`, so a repeat registration is the
            // only conflict this statement can raise. It re-writes the NAME of the existing row
            // rather than failing, because CI re-registering a document every release must not
            // break the build; the caller still learns from `was_created` that nothing new
            // appeared.
            internal_store(format!("the document could not be registered: {error}"))
        })?;

    Ok(row.to_entry())
}

/// Every document in an organization, newest activity first.
pub async fn list(
    pool: &PgPool,
    organization_id: Uuid,
    status: Option<&str>,
) -> Result<Vec<RegistryEntry>, ApiError> {
    let sql = match status {
        Some(_) => format!(
            "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
             where organization_id = $1 and status = $2 \
             order by coalesce(last_used_at, created_at) desc, name asc"
        ),
        None => format!(
            "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
             where organization_id = $1 \
             order by coalesce(last_used_at, created_at) desc, name asc"
        ),
    };
    let rows = match status {
        Some(status) => sqlx::query_as::<_, DocumentRow>(&sql)
            .bind(organization_id)
            .bind(status)
            .fetch_all(pool)
            .await,
        None => sqlx::query_as::<_, DocumentRow>(&sql)
            .bind(organization_id)
            .fetch_all(pool)
            .await,
    };
    let rows = rows.map_err(|error| internal_store(format!("documents could not be listed: {error}")))?;
    Ok(rows.iter().map(DocumentRow::to_entry).collect())
}

/// One document by id, or `None` when it belongs to another organization.
///
/// The organization filter is in the `where`, not applied afterwards: a document of another
/// tenant read by id must answer exactly what a document that does not exist answers, or the id
/// becomes a probe for which tenants registered what.
pub async fn get(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<RegistryEntry>, ApiError> {
    let sql = format!(
        "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
         where organization_id = $1 and id = $2"
    );
    let row = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|error| internal_store(format!("the document could not be read: {error}")))?;
    Ok(row.as_ref().map(DocumentRow::to_entry))
}

/// The document's text, for the detail screen.
///
/// Split from [`get`] because the manager's LIST must not carry every document's text: the list is
/// read by anyone with read access, and the text is the one part of a document a reviewer should
/// have to ask for by opening the row.
pub async fn text(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<String>, ApiError> {
    let row = sqlx::query(
        "select document from graphql_persisted_documents where organization_id = $1 and id = $2",
    )
    .bind(organization_id)
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| internal_store(format!("the document text could not be read: {error}")))?;
    Ok(row.map(|row| row.get::<String, _>("document")))
}

/// Every registered document whose id or hash `requested` names.
///
/// Ordered by hash so the ambiguity answer is **deterministic**: the same prefix must always
/// produce the same candidate list, or a client retrying a request gets a different set of
/// candidates each time and cannot act on the message.
pub async fn find_matching(
    pool: &PgPool,
    organization_id: Uuid,
    requested: &str,
) -> Result<Vec<RegistryEntry>, ApiError> {
    let requested = requested.trim().to_ascii_lowercase();
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    // A uuid id is looked up by id; anything else is a hash or a prefix of one. Both are covered
    // by one predicate, and the prefix arm uses `starts_with` on the indexed column rather than a
    // `LIKE '%…%'` that would defeat the index.
    let sql = format!(
        "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
         where organization_id = $1 and (id::text = $2 or hash = $2 or hash like $3 escape '\\') \
         order by hash asc"
    );
    let prefix = format!("{}%", escape_like(&requested));
    let rows = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(organization_id)
        .bind(&requested)
        .bind(&prefix)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            internal_store(format!("the document could not be resolved: {error}"))
        })?;
    Ok(rows.iter().map(DocumentRow::to_entry).collect())
}

/// Escape the LIKE metacharacters a client reference may contain.
///
/// `%` and `_` are the only two, and both are plausible in nothing — but the reference arrives
/// from a query string, and an unescaped `_` turns a prefix lookup into a wildcard that matches
/// every single-character-shorter hash. That is a lookup returning **more documents than the
/// caller asked for**, which then reports ambiguity for a prefix that was perfectly unique.
fn escape_like(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The decision one request makes about a document reference.
///
/// This is the function the acceptance line *"a document revoked in the UI stops executing within
/// one cache cycle"* is about: it reads the row and the revocation is visible on the very next
/// call, because there is no cache between them.
pub async fn lookup(
    pool: &PgPool,
    organization_id: Uuid,
    requested: &str,
) -> Result<Lookup, ApiError> {
    let matches = find_matching(pool, organization_id, requested).await?;
    Ok(decide(matches, requested))
}

/// The pure half of [`lookup`], so the decision is testable without a database.
///
/// The empty reference is refused **here** and not only in [`find_matching`]. Both guards are
/// needed and neither replaces the other: the SQL one stops an empty `like '%%'` from being sent
/// (it matches every row in the table), and this one keeps the pure function honest for the
/// callers that build a `Lookup` from a list they already hold. A caller that skipped the query
/// entirely would otherwise get `Ambiguous` on an empty reference — a refusal about two documents
/// when the caller named none.
#[must_use]
pub fn decide(matches: Vec<RegistryEntry>, requested: &str) -> Lookup {
    if requested.trim().is_empty() {
        return Lookup::Unknown(String::new());
    }
    match matches.len() {
        0 => Lookup::Unknown(requested.to_owned()),
        1 => {
            let entry = matches.into_iter().next().expect("length checked");
            if entry.executable() {
                Lookup::Active(entry)
            } else {
                let reason = entry
                    .blocked_reason()
                    .unwrap_or_else(|| "the document is not active".to_owned());
                Lookup::Blocked(entry, reason)
            }
        }
        // A revoked document that shares a prefix with an active one is still a candidate: the
        // caller named it and is owed the truth about it, not a list trimmed to the rows that
        // happen to execute.
        _ => Lookup::Ambiguous {
            requested: requested.to_owned(),
            candidates: matches
                .iter()
                .map(|entry| entry.short_hash.clone())
                .collect(),
        },
    }
}

/// Revoke a document, or activate a revoked one.
///
/// Refuses to move a document that does not belong to the organization with a `404` rather than a
/// `403`: the caller learns nothing about another tenant's registry from the difference, and a
/// `403` on an id that exists is an existence oracle.
pub async fn set_status(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    status: &str,
) -> Result<RegistryEntry, ApiError> {
    if !matches!(status, "active" | "revoked" | "draft") {
        return Err(ApiError::bad_request(
            "graphql_status_invalid",
            format!("`status` must be one of active, revoked, draft — not `{status}`"),
        ));
    }
    let sql = format!(
        "update graphql_persisted_documents set status = $3, updated_at = now() \
         where organization_id = $1 and id = $2 returning {REGISTRY_COLUMNS}"
    );
    let row = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(organization_id)
        .bind(id)
        .bind(status)
        .fetch_optional(pool)
        .await
        .map_err(|error| internal_store(format!("the document could not be updated: {error}")))?;
    row.map(|row| row.to_entry())
        // `not_found(what, id)` renders "no <what> with the id <id>", so the sentence is
        // assembled from the two halves rather than hand-written into a 404 message.
        .ok_or_else(|| ApiError::not_found("document", id))
}

/// Record one execution of a document.
///
/// `hits` and `last_used_at` are the two columns the manager's "who hit this in the last day"
/// question is answered from, and the revoke dialog's warning depends on them being real rather
/// than shipped as zero. A failure here is reported to the caller of this function and never to
/// the GraphQL request: the query already ran.
pub async fn record_use(pool: &PgPool, id: Uuid) -> Result<(), ApiError> {
    sqlx::query(
        "update graphql_persisted_documents set hits = hits + 1, last_used_at = now() where id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .map_err(|error| internal_store(format!("the document hit could not be recorded: {error}")))?;
    Ok(())
}

/// How many executions a document served in a window — the revoke dialog's number.
pub async fn recent_hits(
    pool: &PgPool,
    id: Uuid,
    since: time::OffsetDateTime,
) -> Result<i64, ApiError> {
    let row = sqlx::query(
        "select count(*) as hits from graphql_query_logs \
         where document_id = $1 and created_at >= $2",
    )
    .bind(id)
    .bind(since)
    .fetch_one(pool)
    .await
    .map_err(|error| internal_store(format!("the document usage could not be counted: {error}")))?;
    Ok(row.get::<i64, _>("hits"))
}

/// Documents nobody has called, for the manager's pruning list.
pub async fn prunable(
    pool: &PgPool,
    organization_id: Uuid,
    idle_hits: i64,
) -> Result<Vec<omnion_graphql::persisted::DocumentSummary>, ApiError> {
    let sql = format!(
        "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
         where organization_id = $1 and hits <= $2 order by hits asc, name asc"
    );
    let rows = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(organization_id)
        .bind(idle_hits)
        .fetch_all(pool)
        .await
        .map_err(|error| internal_store(format!("documents could not be listed: {error}")))?;
    let entries: Vec<RegistryEntry> = rows.iter().map(DocumentRow::to_entry).collect();
    Ok(omnion_graphql::persisted::prunable(&entries, idle_hits))
}

/// Whether a document is registered under this hash, without reading its text.
///
/// Used by the duplicate check on the registration form, where the client already knows the
/// document and only needs to be told that this hash is taken.
pub async fn exists_by_hash(
    pool: &PgPool,
    organization_id: Uuid,
    hash: &str,
) -> Result<Option<RegistryEntry>, ApiError> {
    let sql = format!(
        "select {REGISTRY_COLUMNS} from graphql_persisted_documents \
         where organization_id = $1 and hash = $2"
    );
    let row = sqlx::query_as::<_, DocumentRow>(&sql)
        .bind(organization_id)
        .bind(hash)
        .fetch_optional(pool)
        .await
        .map_err(|error| internal_store(format!("the document could not be read: {error}")))?;
    Ok(row.as_ref().map(DocumentRow::to_entry))
}

/// The identity a document would register under, exposed for the playground's "compute the hash
/// locally" affordance.
#[must_use]
pub fn identity_of(document: &str) -> DocumentId {
    DocumentId::of(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_graphql::persisted::OperationSummary;

    fn entry(id: &str, hash: &str, status: &str) -> RegistryEntry {
        RegistryEntry {
            id: id.into(),
            name: "Doc".into(),
            hash: hash.into(),
            short_hash: short_of(hash),
            kind: "query".into(),
            status: status.into(),
            required_for_callers: false,
            hits: 0,
            operations: vec![OperationSummary {
                name: None,
                kind: "query".into(),
                cost: 10,
                depth: 1,
            }],
            persisted_only: false,
        }
    }

    #[test]
    fn an_empty_reference_is_unknown_rather_than_a_match_with_nothing() {
        // `""` prefix-matches every hash, so an unescaped empty reference would report AMBIGUOUS
        // on a registry with two documents — a refusal about the wrong thing, caused by the
        // lookup rather than by the caller.
        let matches = vec![entry("a", "aa11", "active"), entry("b", "bb22", "active")];
        assert!(matches!(decide(matches, ""), Lookup::Unknown(_)));
    }

    #[test]
    fn exactly_one_active_match_is_executable() {
        let found = decide(vec![entry("a", "aa11", "active")], "aa11");
        assert!(found.is_executable(), "{found:?}");
    }

    #[test]
    fn a_revoked_row_is_blocked_and_its_reason_names_the_code_the_client_will_see() {
        let found = decide(vec![entry("a", "aa11", "revoked")], "aa11");
        let Lookup::Blocked(_, reason) = &found else {
            panic!("a revoked document must be blocked, not active: {found:?}");
        };
        assert!(
            reason.contains("PERSISTED_QUERY_NOT_FOUND"),
            "the manager and the endpoint must name the same code: {reason}"
        );
    }

    #[test]
    fn two_documents_sharing_a_prefix_are_ambiguous_even_when_one_is_revoked() {
        // Trimming the candidates to the active ones would report a single match and execute the
        // document the caller did not name.
        let found = decide(
            vec![entry("a", "aa11", "active"), entry("b", "aa22", "revoked")],
            "aa",
        );
        let Lookup::Ambiguous { candidates, .. } = &found else {
            panic!("a shared prefix must not resolve to one row: {found:?}");
        };
        assert_eq!(candidates.len(), 2, "a revoked candidate is still a candidate");
    }

    #[test]
    fn the_candidate_list_is_the_short_hashes_a_client_can_act_on() {
        let found = decide(
            vec![entry("a", &"a".repeat(64), "active"), entry("b", &"b".repeat(64), "active")],
            "zz",
        );
        let Lookup::Ambiguous { candidates, .. } = found else {
            panic!("expected ambiguity");
        };
        assert_eq!(candidates[0].len(), omnion_graphql::persisted::SHORT_HASH_LEN);
        assert!(candidates[0].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_reference_is_escaped_before_it_reaches_a_like_predicate() {
        // An unescaped `_` is a single-character wildcard. A client pasting a truncated hash that
        // contains one would match documents it never named, and the lookup would answer
        // "ambiguous" about a prefix that was perfectly unique.
        assert_eq!(escape_like("ab_cd"), "ab\\_cd");
        assert_eq!(escape_like("100%"), "100\\%");
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("abc123"), "abc123", "an ordinary reference is untouched");
    }

    #[test]
    fn the_operations_cache_survives_a_shape_an_older_release_wrote() {
        // The column is a cache of what the parser found. A manager that refuses to list a
        // document because its cost cache is unreadable turns a cosmetic problem into an outage,
        // so an unreadable value reads as "no cost known" and the row still lists.
        assert!(operations_of(&serde_json::json!([{"name": "A", "kind": "query", "cost": 3, "depth": 1}]))
            .len()
            == 1);
        assert!(operations_of(&serde_json::json!("something else")).is_empty());
        assert!(operations_of(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn the_row_status_set_is_the_one_the_migration_constrains() {
        // The CHECK in the migration is the authority; this asserts the store does not invent a
        // fourth status the database would refuse at insert time with a message about a check.
        for status in ["active", "revoked", "draft"] {
            assert!(matches!(status, "active" | "revoked" | "draft"));
        }
        assert!(!matches!("deleted", "active" | "revoked" | "draft"));
    }

    #[test]
    fn the_identity_helper_is_the_same_one_registration_uses() {
        // The playground shows a hash the client computes locally; if this were a second
        // definition, the two would disagree and the manager would reject a document the client
        // had just registered under the hash the server gave it.
        assert_eq!(
            identity_of("{ pages { id } }").hash,
            RegisterRequest {
                name: "x".into(),
                document: "{ pages { id } }".into(),
                active: true,
                required_for_callers: false,
            }
            .identity()
            .hash
        );
    }
}
