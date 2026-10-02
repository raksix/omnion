/**
 * The typed client for the GraphQL surface (REQ-130, slice 2's admin screens).
 *
 * ## Why its own module rather than more of `api.ts`
 *
 * For the same reason `deployment-api.ts` exists, and one more: **every payload here has a
 * contract about honesty.** `blocked_reason` exists so the manager never shows a control beside a
 * note explaining why the control cannot work; `persisted_only` is echoed from the same row the
 * endpoint enforces rather than re-declared client-side; `contributors` is the list the cost meter
 * has to name or the meter has nothing to explain. Those read as one contract when they sit next
 * to each other and as four unrelated fields when they are 5,000 lines apart.
 *
 * ## Nothing here can carry a credential
 *
 * There is no field in any type below that a secret can arrive in — a document is query text, a
 * hash, a name. The persisted-document *secret* travels on a redemption path REQ-125 owns; this
 * surface never sees one, so the guarantee is a property of the types rather than of a scan.
 *
 * ## The cost meter predicts, it does not re-price
 *
 * `measure()` is the endpoint's own decision layer (`crates/graphql`), not a JavaScript
 * re-implementation of it. A meter written here would agree with the endpoint until one of them
 * changed a weight, and the disagreement would be invisible in review — it is exactly the drift
 * the request forbids ("cost accounting … stay per-request").
 */

import { request, ApiError } from "./api";

export { ApiError };

// -------------------------------------------------------------------------------------------
// Persisted documents
// -------------------------------------------------------------------------------------------

/** One operation inside a document, with the numbers the meter shows. */
export type GraphqlOperation = {
  name: string | null;
  kind: string;
  cost: number;
  depth: number;
};

/** One row of the persisted-document registry, as the manager lists it. */
export type GraphqlDocument = {
  id: string;
  name: string;
  hash: string;
  /** The first 32 hex characters — what a client sends, and what the screen copies. */
  short_hash: string;
  kind: string;
  status: string;
  required_for_callers: boolean;
  hits: number;
  operations: GraphqlOperation[];
  /**
   * `null` when the row executes. **Never render a control next to a row that carries one** —
   * the string names the code the client will receive, so the manager can explain a refusal it
   * caused.
   */
  blocked_reason: string | null;
  last_used_at: string | null;
};

/** `GET /api/v1/graphql/documents`. */
export type GraphqlDocumentList = {
  documents: GraphqlDocument[];
  total: number;
  /** Read from the row the endpoint enforces, so the banner cannot disagree with it. */
  persisted_only: boolean;
};

/** `GET /api/v1/graphql/documents`. */
export function fetchGraphqlDocuments(): Promise<GraphqlDocumentList> {
  return request<GraphqlDocumentList>("/api/v1/graphql/documents");
}

/** `GET /api/v1/graphql/documents/{id}` — the row plus its text and the revoke warning's count. */
export type GraphqlDocumentDetail = GraphqlDocument & {
  text: string;
  /** Executions in the last day: the number the revoke dialog must quote, not "unknown callers". */
  recent_hits: number;
};

export function fetchGraphqlDocument(id: string): Promise<GraphqlDocumentDetail> {
  return request<GraphqlDocumentDetail>(`/api/v1/graphql/documents/${encodeURIComponent(id)}`);
}

/** `POST /api/v1/graphql/documents`. */
export type RegisterDocumentInput = {
  name: string;
  document: string;
  active: boolean;
  required_for_callers: boolean;
};

/** `POST /api/v1/graphql/documents`. */
export type RegisterDocumentResult = {
  document: GraphqlDocument;
  /** False when the same hash was already registered and only the name moved. */
  created: boolean;
  duplicate_of: string | null;
};

export function registerGraphqlDocument(
  input: RegisterDocumentInput,
): Promise<RegisterDocumentResult> {
  return request<RegisterDocumentResult>("/api/v1/graphql/documents", {
    method: "POST",
    body: JSON.stringify(input),
  });
}

/** `PUT /api/v1/graphql/documents/{id}` — revoke or re-activate. */
export function setGraphqlDocumentStatus(
  id: string,
  status: "revoked" | "active",
): Promise<GraphqlDocument> {
  return request<GraphqlDocument>(`/api/v1/graphql/documents/${encodeURIComponent(id)}`, {
    method: "PUT",
    body: JSON.stringify({ status }),
  });
}

/** `GET /api/v1/graphql/documents/prunable` — rows nobody has called. */
export type PrunableDocument = {
  id: string;
  name: string;
  short_hash: string;
  kind: string;
  status: string;
  hits: number;
  cost: number | null;
  depth: number | null;
};

export type PrunableResponse = {
  documents: PrunableDocument[];
  idle_hits: number;
};

export function fetchPrunableDocuments(): Promise<PrunableResponse> {
  return request<PrunableResponse>("/api/v1/graphql/documents/prunable");
}

// -------------------------------------------------------------------------------------------
// Settings
// -------------------------------------------------------------------------------------------

/**
 * The endpoint's limits. **Every field here is READ by the endpoint**, which is why this is a
 * type and not a set of defaults in the screen: `Settings::default()` in the endpoint is the
 * defect tick 68's walk found (the screen wrote a row nobody read), and a screen that keeps its
 * own copy of the defaults would hide the next one.
 */
export type GraphqlSettings = {
  max_depth: number;
  cost_budget: number;
  max_aliases: number;
  max_fragments: number;
  max_page_size: number;
  timeout_ms: number;
  persisted_only: boolean;
  playground_enabled: boolean;
};

/** `GET /api/v1/graphql/settings`. */
export function fetchGraphqlSettings(): Promise<GraphqlSettings> {
  return request<GraphqlSettings>("/api/v1/graphql/settings");
}

/** `PUT /api/v1/graphql/settings`. */
export function saveGraphqlSettings(settings: GraphqlSettings): Promise<GraphqlSettings> {
  return request<GraphqlSettings>("/api/v1/graphql/settings", {
    method: "PUT",
    body: JSON.stringify(settings),
  });
}

// -------------------------------------------------------------------------------------------
// The schema explorer
// -------------------------------------------------------------------------------------------

/** One field the caller can see, and the permission it requires. */
export type VisibleField = {
  name: string;
  /** `null` when the field needs only its type's read permission. */
  requires: string | null;
  is_mutation: boolean;
  /** The type a selection beneath it is checked against. */
  returns: string | null;
};

export type VisibleType = {
  name: string;
  fields: VisibleField[];
};

export type ComposedSchema = {
  types: VisibleType[];
  /**
   * Types this caller cannot read. **Never rendered as SDL** — an administrator needs to know a
   * type is missing; a caller must not be able to read its name out of a comment.
   */
  withheld: string[];
};

/** One role the caller's organization can compose a schema for. */
export type RoleOption = {
  id: string;
  key: string;
  name: string;
  /** How many of the GraphQL-visible permissions the role grants. */
  permission_count: number;
};

/** `GET /api/v1/graphql/schema`. */
export type SchemaResponse = {
  schema: ComposedSchema;
  /** The caller's own SDL — already filtered, so nothing withheld appears in it. */
  sdl: string;
  /** The cache key this composition is stored under (module set · capabilities · permissions · version). */
  cache_key: string;
  /** The caller's resolved permissions, so the explorer can say what it composed from. */
  permissions: string[];
  /** Roles the caller's organization may compare against. */
  roles: RoleOption[];
  /** `content.pages.read` — the guard on this route, echoed so a refusal is explainable. */
  read_permission: string;
};

/** `GET /api/v1/graphql/schema`. */
export function fetchSchema(): Promise<SchemaResponse> {
  return request<SchemaResponse>("/api/v1/graphql/schema");
}

/** One line of the role diff. */
export type SchemaDiffLine = {
  /** `Type` or `Type.field`. */
  path: string;
  /** `only in <label>` or `only in <label>` for a field. */
  direction: "mine" | "theirs";
  /** The permission the other side lacks. */
  requires: string | null;
};

/** `GET /api/v1/graphql/schema/diff?roleId=…`. */
export type SchemaDiffResponse = {
  /** The caller's own label in the comparison. */
  mine: string;
  /** The role compared against. */
  theirs: string;
  /** What the caller gains that the role lacks, with the permission on each line. */
  only_mine: SchemaDiffLine[];
  /** What the role has that the caller lacks — with the permission, which is the answer. */
  only_theirs: SchemaDiffLine[];
  /** True when the two resolve to the same GraphQL surface. */
  identical: boolean;
};

export function fetchSchemaDiff(roleId: string): Promise<SchemaDiffResponse> {
  return request<SchemaDiffResponse>(
    `/api/v1/graphql/schema/diff?roleId=${encodeURIComponent(roleId)}`,
  );
}

// -------------------------------------------------------------------------------------------
// The playground
// -------------------------------------------------------------------------------------------

/** One execution's outcome. `data` is absent on a refusal, which is a GraphQL contract. */
export type PlaygroundEnvelope = {
  data?: Record<string, unknown> | null;
  errors?: {
    message: string;
    extensions: { code: string; limit?: number; actual?: number; contributors?: { field: string; weight: number }[] };
  }[];
  extensions: {
    depth: number;
    cost: number;
    durationMs: number;
    requestId: string;
    aliases?: number;
    pageSize?: number;
    persisted?: boolean;
  };
};

/**
 * `POST /api/v1/graphql` — run one ad-hoc document.
 *
 * Returns the envelope on **every** outcome that reached execution, refusals included, because
 * that is the endpoint's contract: a `200` with an `errors` array is a GraphQL answer and a
 * client that throws on it loses the `extensions` block the meter needs.
 */
export function runGraphql(body: {
  query: string;
  operationName?: string | null;
  variables?: Record<string, unknown> | null;
}): Promise<PlaygroundEnvelope> {
  return request<PlaygroundEnvelope>("/api/v1/graphql", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** `GET /api/v1/graphql?documentId=…&variables=…` — run a registered document. */
export function runPersistedDocument(
  documentId: string,
  variables: Record<string, unknown> | null,
): Promise<PlaygroundEnvelope> {
  const query = new URLSearchParams({ documentId });
  if (variables && Object.keys(variables).length > 0) {
    query.set("variables", JSON.stringify(variables));
  }
  return request<PlaygroundEnvelope>(`/api/v1/graphql?${query.toString()}`);
}