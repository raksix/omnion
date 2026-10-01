/**
 * The local-endpoint client (docs/requests/REQ-106, slice 1).
 *
 * A module of its own for the same reason `guard-api.ts` is one: these are the only places the
 * local endpoint wire shapes are written down on the admin side, and the two screens that read
 * them sit next to it.
 *
 * **Locality is never re-derived here.** `locality` and `host_kind` arrive on the row and are
 * rendered as they arrive. A screen that re-ran the host test in TypeScript would own a second
 * copy of the rule the air-gap check runs server-side, and the two answers could disagree in
 * exactly the case that matters — a provider the panel calls "local" and the check calls
 * "remote".
 */
import { request } from "./api";

/**
 * One provider row, with the locality the platform verified at save time.
 *
 * A local endpoint *is* a provider row, not a parallel table — that is why `host` and
 * `local_count` come precomputed rather than parsed out of `base_url` in the browser.
 */
export type LocalEndpoint = {
  id: string;
  name: string;
  /** Normalized base URL, version segment included. */
  base_url: string;
  /** The host part, precomputed by the API. */
  host: string;
  /** `local` or `remote`. */
  locality: string;
  /**
   * Why the platform believes it — `loopback`, `private`, `allowlisted`, or `null`.
   *
   * `null` on a `local` row means the stored answer is older than the check; the screen shows
   * that as "unknown", never as a green badge.
   */
  host_kind: string | null;
  protocol: string;
  enabled: boolean;
  /** When a probe last reached this endpoint. `null` is "never probed", not "down". */
  last_seen_at: string | null;
  model_count: number;
  available_count: number;
};

/** `GET /api/v1/ai/local/endpoints`. */
export type LocalEndpointList = {
  endpoints: LocalEndpoint[];
  /** Verified-local endpoints — the stat tile's number. */
  local_count: number;
  /** Endpoints whose traffic can leave this machine. */
  remote_count: number;
  /**
   * `true` when no endpoint has ever been registered.
   *
   * Distinct from "loaded and found none in the current filter": an installation with no
   * provider at all gets the empty state, while a filtered one gets "no match".
   */
  is_empty: boolean;
};

/** A model row an endpoint serves. */
export type LocalModel = {
  id: string;
  provider_id: string;
  /** Endpoint name, joined server-side for the table column. */
  endpoint_name: string;
  /** The key the endpoint answers to — what a pull and a remove name. */
  model_key: string;
  display_name: string | null;
  size_bytes: number | null;
  parameter_count: number | null;
  quantization: string | null;
  context_window: number | null;
  supports_tools: boolean;
  supports_vision: boolean;
  supports_embeddings: boolean;
  supports_rerank: boolean;
  embedding_dimension: number | null;
  /** `available`, `pulling`, `missing` or `error`. */
  status: string;
  /** 0–100 while pulling. */
  pull_progress: number;
  /** The server's own progress or error line, verbatim and clipped. */
  pull_message: string | null;
  /** The server holds the weights in memory. */
  resident: boolean;
  last_used_at: string | null;
  updated_at: string;
};

/** `GET /api/v1/ai/local/models`. */
export type LocalModelList = {
  models: LocalModel[];
  /**
   * Totals computed over the **unfiltered** list.
   *
   * That is deliberate: a stat tile that counted the visible page would change its number every
   * time a search box was typed into.
   */
  total_models: number;
  available: number;
  resident: number;
  pulling: number;
  /** Computed on the unfiltered list too — see `is_empty` on {@link LocalEndpointList}. */
  is_empty: boolean;
};

/** The filter every `GET /ai/local/models` call sends; all fields optional. */
export type LocalModelFilter = {
  endpoint?: string | null;
  status?: string | null;
  /** `tools`, `vision`, `embeddings` or `rerank`. */
  capability?: string | null;
  q?: string | null;
};

/**
 * What a pull answered.
 *
 * `already_available` and `already_pulling` are **not errors** and must not be rendered as one:
 * the server returns `200` precisely because nothing is in conflict — the model is simply
 * already there, or already coming. A screen that showed a red banner here would train
 * operators to ignore real failures.
 */
export type PullResult = {
  outcome: "started" | "already_available" | "already_pulling" | "available";
  model: LocalModel;
};

/** Every endpoint with its verified locality. */
export function fetchLocalEndpoints(): Promise<LocalEndpointList> {
  return request<LocalEndpointList>("/api/v1/ai/local/endpoints");
}

/**
 * Register a local endpoint.
 *
 * The host is verified server-side; a public address is refused with `invalid_provider` naming
 * the host. That refusal is the screen's whole reason for saying "loopback, a private address or
 * an allow-listed host" next to the field.
 */
export function createLocalEndpoint(body: {
  name: string;
  base_url: string;
  protocol?: string;
  api_key?: string | null;
  /** `false` stores a row as remote. It cannot make a public address local. */
  local?: boolean;
}): Promise<{ endpoint: LocalEndpoint }> {
  return request<{ endpoint: LocalEndpoint }>("/api/v1/ai/local/endpoints", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** What the endpoints serve, filtered server-side. */
export function fetchLocalModels(filter: LocalModelFilter = {}): Promise<LocalModelList> {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(filter)) {
    if (value) params.set(key, value);
  }
  const query = params.toString();
  return request<LocalModelList>(`/api/v1/ai/local/models${query ? `?${query}` : ""}`);
}

/** Ask an endpoint to pull a model. Idempotent: a second click explains itself. */
export function pullLocalModel(body: { endpoint: string; model_key: string }): Promise<PullResult> {
  return request<PullResult>("/api/v1/ai/local/models/pull", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Cancel a pull in flight. The row returns to `missing` rather than claiming to pull forever. */
export function cancelLocalPull(body: { endpoint: string; model_key: string }): Promise<{ model: LocalModel }> {
  return request<{ model: LocalModel }>("/api/v1/ai/local/models/cancel", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** Clear an `error` row and pull it again. */
export function retryLocalPull(body: { endpoint: string; model_key: string }): Promise<PullResult> {
  return request<PullResult>("/api/v1/ai/local/models/retry", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/**
 * Remove a model from its endpoint. Refused while the model is pulling.
 *
 * `DELETE` carries a body because the row is addressed by (endpoint, model key) and there is no
 * path segment to hang them on — the API defines it this way, so the client matches it rather
 * than inventing a second URL for the same operation.
 */
export function removeLocalModel(body: { endpoint: string; model_key: string }): Promise<{
  removed: string;
  endpoint: string;
}> {
  return request<{ removed: string; endpoint: string }>("/api/v1/ai/local/models", {
    method: "DELETE",
    body: JSON.stringify(body),
  });
}

/**
 * Ask an endpoint what it serves and record the answer.
 *
 * A `502 local_endpoint_unreachable` and a `422` are different faults — "your Ollama is down" and
 * "your Ollama speaks something else" — so the screen prints the server's own message rather
 * than collapsing both into "connection failed".
 */
export function scanLocalEndpoint(id: string): Promise<{
  endpoint: string;
  served: number;
  written: number;
}> {
  return request<{ endpoint: string; served: number; written: number }>("/api/v1/ai/local/scan", {
    method: "POST",
    body: JSON.stringify(id),
  });
}
