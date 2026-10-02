//! `POST|GET /api/v1/graphql` — the endpoint (REQ-130, slice 1).
//!
//! ## What this handler is responsible for, and what it deliberately is not
//!
//! The decisions — parse, price, measure, compose, validate — all live in `crates/graphql` and are
//! pure. The data access lives in `crates/graphql-resolvers`, which calls the same service
//! functions the REST handlers call. This file is the **seam**: it authenticates the caller,
//! resolves their permissions once, runs the refusals in the order the request specifies, writes
//! one query-log row, and assembles the envelope. It contains no query and no pricing rule, which
//! is what makes the playground's cost meter able to predict it exactly rather than agree with it.
//!
//! ## The envelope is assembled in one place, on both outcomes
//!
//! The request: *"Execution returns `200` with a GraphQL envelope even when it contains errors;
//! transport-level failures (`401`, `403` on the endpoint itself, `429`) keep HTTP statuses."*
//! Those are two different contracts and confusing them is the most common GraphQL integration
//! bug, so the split is explicit and [`execute_request`] returns the envelope — never an
//! `ApiError` — for anything that reached execution. A `403` on the endpoint *itself* (the caller
//! may not use the endpoint) is a transport failure and does keep its status.
//!
//! ## The guard the endpoint carries
//!
//! The endpoint itself is guarded by a permission, and it is a **real catalogue key**. The first
//! draft of the surface had a route whose guard named `developer.graphql.execute`, which this
//! repository does not ship — and an uncatalogued key resolves to no permission, so the route
//! answers `403` for every caller *including the instance owner*. So the guard is
//! `content.pages.read`: the endpoint reads the same surface it exposes, and a caller who may read
//! content may run queries. A machine key is accepted too, because a service account integrating
//! with the platform has no session and the request says so explicitly ("session or API key,
//! sandbox keys included").

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use omnion_graphql::document::MAX_DOCUMENT_BYTES;
use omnion_graphql::parity::{ALL as KNOWN_PERMISSIONS, PermissionSet};
use omnion_graphql::schema::{CacheKey, SchemaCatalogue, compose};
use omnion_graphql::{Code, Error, extensions, parse};
use omnion_graphql_resolvers::{Caller, execute};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::ApiCaller;
use crate::error::ApiError;
use crate::state::AppState;

/// The request body, per the GraphQL over HTTP convention.
#[derive(Debug, Default, Deserialize)]
pub struct GraphqlRequest {
    /// The document text.
    pub query: String,
    /// Which operation to run, when the document defines more than one.
    #[serde(default)]
    pub operation_name: Option<String>,
    /// Variable values. **Never logged** — see [`crate::routes::graphql::record_query`].
    #[serde(default)]
    pub variables: Value,
}

/// The response body. Always `200` once execution began.
#[derive(Debug, Serialize)]
pub struct GraphqlEnvelope {
    /// `data` when at least one field resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// `errors` when anything was refused. Absent otherwise — a client checking `if (!body.errors)`
    /// must not have to distinguish an absent key from an empty array, and it must be absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<GraphqlError>>,
    /// Depth, cost, duration and request id — on both outcomes.
    pub extensions: Value,
}

/// One error, in the shape a client branches on.
#[derive(Debug, Serialize)]
pub struct GraphqlError {
    /// For a human.
    pub message: String,
    /// The machine-readable code: `DEPTH_LIMIT`, `FORBIDDEN`, `TYPE_NOT_VISIBLE`, …
    pub extensions: Value,
}

/// The refusals that happen BEFORE execution, in the order the request specifies.
///
/// Order matters and it is the order the decision layer documents: a document that is both too
/// deep and too expensive is reported as too deep, because that is the one a client can fix by
/// flattening a selection. And every one of these runs before a resolver is called, which is what
/// makes *"a refused mutation … changes nothing in the store"* structural rather than a matter of
/// ordering discipline inside the resolvers.
async fn run_pre_execution(
    caller: &Caller,
    request: &GraphqlRequest,
    settings: &omnion_graphql::Settings,
    registered: bool,
) -> Result<Prepared, Error> {
    // 1. The document size. Checked on the RAW bytes before parsing, because a parser is the last
    //    thing that should see a 10 MB string.
    if request.query.len() > MAX_DOCUMENT_BYTES {
        return Err(Error::Limit {
            code: Code::PageSizeLimit,
            message: format!(
                "the document is {} bytes, the cap is {MAX_DOCUMENT_BYTES}",
                request.query.len()
            ),
            limit: MAX_DOCUMENT_BYTES as u64,
            actual: request.query.len() as u64,
        });
    }

    let document = parse(&request.query)?;

    // 2. The installation's limits and cost budget — READ, not defaulted.
    //
    // This line was `Settings::default()` and that made the whole settings screen decorative: an
    // operator who turned `persisted_only` on, or lowered `max_depth` to 4, watched the endpoint
    // keep enforcing the shipped defaults, and the walk proved it — the settings test set
    // `persisted_only: true`, got `200` and a normal `data` envelope, and the flag had changed
    // nothing about execution. A settings screen whose values are not read is the "documented but
    // unreachable" shape this request has produced four times already, and here it was a single
    // line wearing the costume of a default.
    let limits = omnion_graphql::Limits::from_settings(settings);

    // 2b. Persisted-only mode, BEFORE a single byte of the document is priced or resolved.
    //
    // Checked here rather than after the measurement because a refused ad-hoc document should not
    // cost the platform a parse, a price walk and a schema composition: the whole point of the
    // flag is that a leaked read scope cannot run arbitrary queries, and the cheapest query is the
    // one never looked at.
    if settings.persisted_only && !registered {
        // The GET leg executes a REGISTERED document and arrives here with no document reference
        // attached, so only an ad-hoc POST is refused here. A registered document goes through
        // `execute_persisted`, which resolves the registry first.
        return Err(Error::Simple {
            code: Code::PersistedQueryNotFound,
            message: "persisted-only mode is on: send a registered document by id or hash instead \
                      of query text"
                .into(),
        });
    }
    let measurement = match request.operation_name.as_deref() {
        Some(name) => {
            let operation = document.select(Some(name))?;
            omnion_graphql::measure_operation(&document, operation, &limits)?
        }
        None => omnion_graphql::check(&document, &limits)?,
    };

    // 3. The caller's composed schema, and validation against it.
    let catalogue = SchemaCatalogue::catalogued();
    let schema = compose(&catalogue, &caller.known);

    Ok(Prepared {
        document,
        measurement,
        schema,
    })
}

/// Everything execution needs, all of it decided before a resolver is called.
struct Prepared {
    document: omnion_graphql::Document,
    measurement: omnion_graphql::Measurement,
    schema: omnion_graphql::schema::ComposedSchema,
}

/// Execute one request and assemble its envelope. **Never returns an error** — everything that
/// reaches this point is a GraphQL outcome.
async fn execute_request(
    state: &AppState,
    caller: &Caller,
    request: &GraphqlRequest,
    document_id: Option<Uuid>,
) -> GraphqlEnvelope {
    // `document_id` is `Some` exactly when the caller presented a REGISTERED document (the GET
    // leg resolves the registry before it gets here). Persisted-only mode refuses ad-hoc text, so
    // the refusal below is applied only when there is no registered document behind the request —
    // otherwise the GET leg would refuse the very documents the flag exists to allow.
    let request_id = Uuid::new_v4().to_string();
    let started = std::time::Instant::now();

    // Read ONCE per request and hand the same value to the refusal path and the measurement, so
    // a settings save landing between the two cannot produce an envelope priced with one policy
    // and refused under another.
    let settings = crate::routes::graphql_settings::load(state.db().pool())
        .await
        .unwrap_or_default();
    let outcome = run_pre_execution(caller, request, &settings, document_id.is_some()).await;
    let (data, errors, measurement) = match outcome {
        Err(error) => {
            // A refusal has no measurement of its own — the document never got that far — so the
            // `extensions` block reports what the refusal ITSELF measured and zeroes elsewhere. A
            // client reading `extensions.depth` on a DEPTH_LIMIT must see the depth that broke
            // it, not zero, or the two refusals become indistinguishable from the outside.
            (None, Some(vec![to_error(&error)]), error_measurement(&error))
        }
        Ok(prepared) => {
            let measurement = prepared.measurement;
            let resolved = execute(
                caller,
                state.db().pool(),
                &prepared.document,
                request.operation_name.as_deref(),
                &prepared.schema,
            )
            .await;
            match resolved {
                Ok(data) => (Some(data), None, measurement),
                Err(error) => (None, Some(vec![to_error(&error)]), measurement),
            }
        }
    };

    let elapsed = started.elapsed().as_millis() as u64;
    let envelope_extensions = extensions(&measurement, elapsed, &request_id);

    record_query(state, caller, request, document_id, &measurement, &errors, &request_id).await;

    GraphqlEnvelope {
        data,
        errors,
        extensions: envelope_extensions,
    }
}

/// The measurement a refusal reports, reconstructed from the error's own numbers.
///
/// A limit refusal carries the actual depth/aliases/page size it measured; a cost refusal carries
/// the cost. Everything else reports zero, which is honest: nothing ran.
fn error_measurement(error: &Error) -> omnion_graphql::Measurement {
    let mut measurement = omnion_graphql::Measurement {
        depth: 0,
        cost: 0,
        aliases: 0,
        fragments: 0,
        contributors: Vec::new(),
        page_size: 0,
    };
    if let Error::Limit { code, limit: _, actual, .. } = error {
        match code {
            Code::DepthLimit => measurement.depth = *actual as u32,
            Code::AliasLimit => measurement.aliases = *actual as usize,
            Code::PageSizeLimit => measurement.page_size = *actual as u32,
            _ => {}
        }
    }
    if let Error::Cost { cost, contributors, .. } = error {
        measurement.cost = *cost;
        measurement.contributors = contributors.clone();
    }
    measurement
}

fn to_error(error: &Error) -> GraphqlError {
    GraphqlError {
        message: error.to_string(),
        extensions: error.extensions(),
    }
}

/// Write the one query-log row every request produces.
///
/// **No variable values, ever.** The row records the NAMES the caller sent and nothing about what
/// they were — the column that would hold a value does not exist, so this is not a promise about
/// this function. The point of logging every request *including refusals* is the `status`
/// column: an operator asking "why does my depth limit keep firing" reads `rejected` rows, and a
/// log that only kept successes could never answer it.
async fn record_query(
    state: &AppState,
    caller: &Caller,
    request: &GraphqlRequest,
    document_id: Option<Uuid>,
    measurement: &omnion_graphql::Measurement,
    errors: &Option<Vec<GraphqlError>>,
    request_id: &str,
) {
    let refused = errors.as_ref().is_some_and(|errors| !errors.is_empty());
    let error_code = refused
        .then(|| {
            errors
                .as_ref()
                .and_then(|errors| errors.first())
                .and_then(|error| error.extensions.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten();

    let variable_names: Vec<String> = request
        .variables
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default();

    // A log write must never fail a query the platform already answered: the caller is not waiting
    // on a tracing table. So the error is logged and dropped, and the next tick's retention job
    // has one hole rather than the request being retried by a client that thinks it failed.
    let outcome = sqlx::query(
        "insert into graphql_query_logs \
         (organization_id, api_key_id, actor_user_id, document_id, operation_name, hash, depth, \
          cost, aliases, duration_ms, status, error_code, ad_hoc, variable_names) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(caller.organization_id)
    .bind(caller.api_key_id)
    .bind(caller.user_id)
    .bind(document_id)
    .bind(request.operation_name.clone())
    .bind(document_hash(&request.query))
    .bind(i32::try_from(measurement.depth).unwrap_or(i32::MAX))
    .bind(f64::from(measurement.cost))
    .bind(i32::try_from(measurement.aliases).unwrap_or(i32::MAX))
    .bind(
        i32::try_from(measurement.contributors.len() * 1000).unwrap_or(i32::MAX),
    )
    .bind(if refused { "rejected" } else { "ok" })
    .bind(error_code)
    .bind(&variable_names)
    .execute(state.db().pool())
    .await;

    if let Err(error) = outcome {
        tracing::debug!(request_id, error = %error, "the query log row could not be written");
    }
}

/// A short, stable digest of the document text.
///
/// Not a security primitive and not claimed to be: it is a **log key**. A full SHA-256 of a
/// document would put the document's length and shape in a table anyone with log access can read,
/// and the request says payloads carry "hashes and codes, never query text". Sixteen hex
/// characters of an FNV-1a is enough to correlate repeated queries and impossible to invert into
/// text.
fn document_hash(query: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in query.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `POST /api/v1/graphql` — execute a document.
pub async fn execute_graphql(
    State(state): State<AppState>,
    caller: ApiCaller,
    Json(request): Json<GraphqlRequest>,
) -> Result<Json<GraphqlEnvelope>, ApiError> {
    let resolved = resolve_caller(&state, &caller).await?;
    Ok(Json(execute_request(&state, &resolved, &request, None).await))
}

/// `GET /api/v1/graphql?documentId=…` — execute a **registered** document by id or hash.
///
/// The caller is resolved FIRST, so an anonymous GET is a `401` on the transport rather than a
/// `PERSISTED_QUERY_NOT_FOUND` that reads as "your document is not registered" — which would be a
/// wrong answer about an unrelated question.
///
/// ## What this leg reads
///
/// The registry, on **every** call, with no cache between the read and the revocation. That is the
/// whole of the acceptance line *"A document revoked in the UI stops executing within one cache
/// cycle"*: the bound is one call, and it holds because there is nothing to expire. The cost is
/// one indexed read of a table with one row per registered document.
///
/// The document's TEXT is read too, and only here — a lookup that answered the query would make
/// the id meaningless, since the point of a registered document is that the client sends an id
/// instead of text.
pub async fn execute_persisted(
    State(state): State<AppState>,
    caller: ApiCaller,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<GraphqlEnvelope>, ApiError> {
    let resolved = resolve_caller(&state, &caller).await?;

    let requested = params
        .get("documentId")
        .or_else(|| params.get("document_id"))
        .or_else(|| params.get("hash"))
        .map(String::as_str)
        .unwrap_or_default();

    let settings = crate::routes::graphql_settings::load(state.db().pool()).await?;

    // An installation that runs persisted-only refuses a request that names no document at all,
    // before the registry is consulted — "your document is not registered" is a true statement
    // about a document the caller did send, and a wrong one about a request that sent none.
    if settings.persisted_only && requested.is_empty() {
        return Ok(Json(refusal_envelope(
            "persisted-only mode is on and this request named no document; send documentId or \
             hash",
        )));
    }

    let Some(organization_id) = resolved.organization_id else {
        // A global caller has no registry: documents are registered per organization because the
        // allowlist is a tenant decision, and there is no tenant here to have registered one.
        return Ok(Json(refusal_envelope(
            "persisted documents belong to an organization; this caller has none",
        )));
    };

    let found =
        crate::routes::graphql_documents::lookup(state.db().pool(), organization_id, requested)
            .await?;

    let (document_id, text) = match &found {
        omnion_graphql::persisted::Lookup::Active(entry) => {
            let id = Uuid::parse_str(&entry.id).map_err(|_| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("the stored document id `{}` is not a uuid", entry.id),
                )
            })?;
            let text = crate::routes::graphql_documents::text(
                state.db().pool(),
                organization_id,
                id,
            )
            .await?
            .ok_or_else(|| {
                // The row exists and its text does not. That is a corrupted row, not a missing
                // document, and answering PERSISTED_QUERY_NOT_FOUND here would tell the client its
                // document is not registered — which is false and sends the caller to register a
                // duplicate of a document that exists.
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    format!("document {id} is registered but carries no text"),
                )
            })?;
            (Some(id), text)
        }
        other => return Ok(Json(refusal_envelope(&other.message()))),
    };

    // The variables arrive as query parameters, which is what a persisted-document client sends.
    // They are parsed rather than ignored: a `variables` parameter that is not JSON is a client
    // bug, and executing the query with no variables would answer a different question than the
    // one asked.
    let variables: serde_json::Value = match params.get("variables") {
        Some(raw) => serde_json::from_str(raw).map_err(|error| {
            ApiError::bad_request(
                "graphql_variables_invalid",
                format!("`variables` must be a JSON object: {error}"),
            )
        })?,
        None => serde_json::Value::Null,
    };

    let request = GraphqlRequest {
        query: text,
        operation_name: params.get("operationName").cloned(),
        variables,
    };

    let envelope = execute_request(&state, &resolved, &request, document_id).await;

    // The hit counter is written AFTER the query has been answered, and a failure to write it is
    // logged rather than surfaced: the manager's "who hit this document" number must never turn a
    // successful query into an error.
    if let Some(id) = document_id {
        if let Err(error) =
            crate::routes::graphql_documents::record_use(state.db().pool(), id).await
        {
            tracing::debug!(%id, error = %error, "the document hit could not be recorded");
        }
    }
    Ok(Json(envelope))
}

/// A `200` envelope carrying one refusal.
///
/// The request's split again: this reached the endpoint, so it is a GraphQL outcome and not an
/// HTTP status. `persistedOnly` rides in `extensions` so a client can tell "your document is not
/// registered" from "your query was too deep" without matching on the message.
fn refusal_envelope(message: &str) -> GraphqlEnvelope {
    GraphqlEnvelope {
        data: None,
        errors: Some(vec![GraphqlError {
            message: message.to_owned(),
            extensions: json!({ "code": Code::PersistedQueryNotFound.as_str() }),
        }]),
        extensions: json!({
            "depth": 0,
            "cost": 0,
            "durationMs": 0,
            "requestId": Uuid::new_v4().to_string(),
            "persisted": true,
        }),
    }
}

/// Resolve the caller's permissions once, into the shape the resolvers take.
async fn resolve_caller(state: &AppState, caller: &ApiCaller) -> Result<Caller, ApiError> {
    let (subject, organization_id, user_id, api_key_id) = match caller {
        ApiCaller::Session(session) => (
            omnion_permissions::model::Subject::User(session.user.id),
            session.user.organization_id,
            Some(session.user.id),
            None,
        ),
        ApiCaller::Machine(machine) => (
            machine.account,
            Some(machine.organization_id),
            None,
            Some(machine.key_id),
        ),
    };

    let scope = match organization_id {
        Some(organization_id) => omnion_permissions::model::Scope::Organization { organization_id },
        None => omnion_permissions::model::Scope::Global,
    };
    let effective = omnion_permissions::effective_permissions_for(
        state.db().pool(),
        subject.clone(),
        &omnion_permissions::model::ResourceContext::from_scope(scope),
    )
    .await?;

    // Derived by NAME from the resolved set, never written by hand: a hand-written list here is
    // exactly the defect the parity gate was written for, in a new place.
    let known = PermissionSet::from_known(
        KNOWN_PERMISSIONS
            .iter()
            .copied()
            .filter(|known| effective.allows(known.as_str())),
    );

    Ok(Caller {
        user_id,
        api_key_id,
        organization_id,
        subject,
        permissions: effective,
        known,
    })
}

/// The cache key an installation's composed schemas are stored under.
///
/// Exposed so the settings screen and the endpoint agree on ONE key shape — a second definition
/// here would be the caching bug the request warns about (one role's schema served to another),
/// with the same cause: two code paths computing the same key differently.
#[must_use]
pub fn schema_cache_key(caller: &Caller, modules: &[&str], capabilities: &[&str], version: u32) -> String {
    CacheKey::new(modules, capabilities, &caller.known, version).fingerprint()
}

/// The endpoint's own status answer, for a transport-level refusal.
#[must_use]
pub fn refusal_status(code: Code) -> StatusCode {
    match code {
        // The request keeps transport-level failures as HTTP statuses. A refused QUERY is a 200
        // envelope; a caller who may not use the endpoint at all is a 403 on the route.
        Code::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        Code::Forbidden => StatusCode::FORBIDDEN,
        _ => StatusCode::OK,
    }
}

/// The envelope, as a response — so the route can return it without re-wrapping.
impl IntoResponse for GraphqlEnvelope {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_measurement_reports_the_number_the_refusal_named() {
        let depth = error_measurement(&Error::Limit {
            code: Code::DepthLimit,
            message: "too deep".into(),
            limit: 10,
            actual: 14,
        });
        assert_eq!(depth.depth, 14, "a depth refusal must report the depth it measured");
        assert_eq!(depth.cost, 0);

        let page = error_measurement(&Error::Limit {
            code: Code::PageSizeLimit,
            message: "too big".into(),
            limit: 100,
            actual: 99999,
        });
        assert_eq!(page.page_size, 99999);
        assert_eq!(page.depth, 0, "a page-size refusal did not measure a depth");

        let aliases = error_measurement(&Error::Limit {
            code: Code::AliasLimit,
            message: "too many".into(),
            limit: 15,
            actual: 40,
        });
        assert_eq!(aliases.aliases, 40);
    }

    #[test]
    fn a_cost_refusal_measurement_carries_the_cost_and_the_contributors() {
        let error = Error::Cost {
            message: "over budget".into(),
            limit: 1000,
            cost: 1400,
            contributors: vec![("pages".into(), 1200)],
        };
        let measurement = error_measurement(&error);
        assert_eq!(measurement.cost, 1400);
        assert_eq!(measurement.contributors, vec![("pages".to_owned(), 1200)]);
    }

    #[test]
    fn a_validation_refusal_measures_nothing_because_nothing_ran() {
        let error = Error::Simple {
            code: Code::GraphqlValidationFailed,
            message: "not GraphQL".into(),
        };
        let measurement = error_measurement(&error);
        assert_eq!(measurement.depth, 0);
        assert_eq!(measurement.cost, 0);
        assert!(measurement.contributors.is_empty());
    }

    #[test]
    fn a_document_hash_is_stable_short_and_carries_no_text() {
        let a = document_hash("{ organizations { id } }");
        let b = document_hash("{ organizations { id } }");
        assert_eq!(a, b, "the same document must hash the same");
        assert_eq!(a.len(), 16, "a log key is not a digest to publish");
        let c = document_hash("{ pages { id } }");
        assert_ne!(a, c, "two different documents must not collide");
        // The hash must not be the text, in any encoding a reader could mistake for one.
        assert!(!a.contains("organization"), "{a}");
    }

    #[test]
    fn a_refused_query_is_still_a_two_hundred_and_only_a_refused_ENDPOINT_is_three_oh_three() {
        assert_eq!(refusal_status(Code::RateLimited), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refusal_status(Code::Forbidden), StatusCode::FORBIDDEN);
        // The point of the whole split: a query the caller may not run is an error INSIDE a 200.
        assert_eq!(refusal_status(Code::DepthLimit), StatusCode::OK);
        assert_eq!(refusal_status(Code::CostLimit), StatusCode::OK);
    }

    #[test]
    fn an_envelope_without_errors_omits_the_key_entirely() {
        let envelope = GraphqlEnvelope {
            data: Some(json!({ "me": { "id": "1" } })),
            errors: None,
            extensions: json!({ "depth": 1 }),
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert!(
            value.get("errors").is_none(),
            "an `errors` key that is present-but-null forces every client to null-check: {value}"
        );
        assert!(value.get("data").is_some());
    }

    #[test]
    fn a_refused_envelope_omits_data_rather_than_answering_null() {
        let envelope = GraphqlEnvelope {
            data: None,
            errors: Some(vec![GraphqlError {
                message: "nope".into(),
                extensions: json!({ "code": "FORBIDDEN" }),
            }]),
            extensions: json!({ "depth": 0 }),
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert!(
            value.get("data").is_none(),
            "a refused operation must not answer `data: null` — that reads as \"the field does not \
             exist\", which is a different and wrong statement: {value}"
        );
        assert_eq!(value["errors"][0]["extensions"]["code"], "FORBIDDEN");
    }
}