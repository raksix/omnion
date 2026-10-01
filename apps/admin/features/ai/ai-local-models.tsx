"use client";

/**
 * `/ai/local/models` — what the local endpoints serve (REQ-106, slice 1).
 *
 * The endpoint list says *where* inference can happen; this says *what is there*. Three decisions
 * shape it, and each one comes from a way the naive table lies:
 *
 * 1. **A pull's progress is polled, because the server holds the connection.**
 *    Ollama answers `POST /api/pull` with a stream that stays open for the whole download, and the
 *    API claims the row in the database *before* it dials. So while a pull request is in flight
 *    this screen re-reads the list on a timer and renders `pull_progress` and the server's own
 *    `pull_message` on the row. Inventing a client-side percentage bar would have been a fake
 *    number agreeing with nothing.
 *
 * 2. **"Already there" is not an error.** The pull endpoint answers `200` with
 *    `already_available` / `already_pulling` precisely because nothing is in conflict. The screen
 *    says that in a neutral tone; a red banner here would teach operators to ignore real
 *    failures, which are the `error` rows that keep the server's own words.
 *
 * 3. **Two empty states, not one.** `is_empty` is computed server-side over the *unfiltered*
 *    list, so "this installation serves no local models" and "your search matched nothing" say
 *    different things and offer different next actions.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "next/navigation";

import { Download, Loader2, RefreshCw, Search, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  cancelLocalPull,
  fetchLocalModels,
  pullLocalModel,
  removeLocalModel,
  retryLocalPull,
  type LocalModel,
  type LocalModelList,
} from "@/lib/local-api";
import { formatBytes, formatTimestamp } from "@/lib/format";

const STATUS_TABS = [
  { key: "", label: "All" },
  { key: "available", label: "Available" },
  { key: "pulling", label: "Pulling" },
  { key: "missing", label: "Missing" },
  { key: "error", label: "Failed" },
] as const;

const STATUS_TONE: Record<string, string> = {
  available: "bg-positive-soft text-positive",
  pulling: "bg-accent-soft text-accent-strong",
  missing: "bg-quiet-soft text-muted",
  error: "bg-danger-soft text-danger",
};

/** How often the list is re-read while a pull is in flight. */
const POLL_MS = 1500;

/** The capabilities a row advertises, in a fixed order so two rows read the same way. */
function capabilityChips(model: LocalModel): string[] {
  const chips: string[] = [];
  if (model.supports_tools) chips.push("tools");
  if (model.supports_vision) chips.push("vision");
  if (model.supports_embeddings) chips.push("embeddings");
  if (model.supports_rerank) chips.push("rerank");
  return chips;
}

/** One row's size/capacity line, or an explicit "the server said nothing". */
function specLine(model: LocalModel): string {
  const parts = [
    model.parameter_count ? `${(model.parameter_count / 1e9).toFixed(1)}B parameters` : null,
    model.size_bytes ? formatBytes(model.size_bytes) : null,
    model.quantization,
    model.context_window ? `${model.context_window} ctx` : null,
    model.embedding_dimension ? `${model.embedding_dimension} dims` : null,
  ].filter(Boolean);
  return parts.join(" · ") || "The server reported no size details for this model";
}

/** A row is addressed by (endpoint, model key) — there is no path segment to hang them on. */
function rowId(model: LocalModel): string {
  return `${model.provider_id}:${model.model_key}`;
}

export function AiLocalModelsView() {
  const params = useSearchParams();
  const endpointFilter = params?.get("endpoint") ?? null;

  const [status, setStatus] = useState<string>("");
  const [q, setQ] = useState("");
  const [search, setSearch] = useState("");
  const [data, setData] = useState<LocalModelList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ id: string; tone: string; text: string } | null>(null);

  /**
   * Ids with a pull request in flight, plus whether any exists.
   *
   * A plain boolean would be wrong the moment two rows are pulled at once: the second completion
   * would clear the first row's poll. A ref count per id keeps polling until the *last* request
   * naming that row settles, and the ref keeps the interval from restarting on every render.
   */
  const inFlight = useRef(new Map<string, number>());
  const [polling, setPolling] = useState(false);

  const load = useCallback(
    (quiet = false) => {
      if (!quiet) setBusy(true);
      setError(null);
      fetchLocalModels({ endpoint: endpointFilter, status: status || null, q: search || null })
        .then(setData)
        .catch((cause: unknown) => {
          // A poll that fails does not wipe a table the operator is already reading; a failed
          // first load does, because there is nothing to show instead of it.
          if (!quiet) setData(null);
          setError(cause instanceof ApiError ? cause.message : "The models could not be loaded.");
        })
        .finally(() => {
          if (!quiet) setBusy(false);
        });
    },
    [endpointFilter, status, search],
  );

  useEffect(() => load(), [load]);

  useEffect(() => {
    const timer = setTimeout(() => setSearch(q.trim()), 300);
    return () => clearTimeout(timer);
  }, [q]);

  // While any pull is in flight, keep re-reading so `pull_progress` and the server's own line
  // move under the operator instead of the row sitting frozen until the request returns.
  useEffect(() => {
    if (!polling) return;
    const timer = setInterval(() => load(true), POLL_MS);
    return () => clearInterval(timer);
  }, [polling, load]);

  const models = useMemo(() => data?.models ?? [], [data]);

  const track = useCallback((id: string, delta: number) => {
    const counts = inFlight.current;
    const next = (counts.get(id) ?? 0) + delta;
    if (next <= 0) {
      counts.delete(id);
    } else {
      counts.set(id, next);
    }
    setPolling(counts.size > 0);
  }, []);

  /** A pull rendered through its real outcome: the "already" answers are 200s, not failures. */
  const pull = useCallback(
    async (model: LocalModel, action: "pull" | "retry") => {
      const id = rowId(model);
      track(id, 1);
      try {
        const answer =
          action === "pull"
            ? await pullLocalModel({ endpoint: model.provider_id, model_key: model.model_key })
            : await retryLocalPull({ endpoint: model.provider_id, model_key: model.model_key });
        const done = answer.outcome === "available" || answer.outcome === "started";
        setNotice({
          id,
          tone: done ? "bg-positive-soft text-positive" : "bg-quiet-soft text-muted",
          text:
            answer.outcome === "available"
              ? `${model.model_key} is now available.`
              : answer.outcome === "started"
                ? `Pulling ${model.model_key}…`
                : answer.outcome === "already_available"
                  ? `${model.model_key} is already on ${model.endpoint_name}; nothing to download.`
                  : `A pull of ${model.model_key} is already running.`,
        });
        load();
      } catch (cause: unknown) {
        setNotice({
          id,
          tone: "bg-danger-soft text-danger",
          text: cause instanceof ApiError ? cause.message : "The pull failed.",
        });
        load();
      } finally {
        track(id, -1);
      }
    },
    [load, track],
  );

  const cancel = useCallback(
    async (model: LocalModel) => {
      const id = rowId(model);
      try {
        await cancelLocalPull({ endpoint: model.provider_id, model_key: model.model_key });
        setNotice({
          id,
          tone: "bg-quiet-soft text-muted",
          text: `Cancelled the pull of ${model.model_key}; it is back to missing.`,
        });
        load();
      } catch (cause: unknown) {
        setNotice({
          id,
          tone: "bg-danger-soft text-danger",
          text: cause instanceof ApiError ? cause.message : "The pull could not be cancelled.",
        });
      }
    },
    [load],
  );

  const remove = useCallback(
    async (model: LocalModel) => {
      const id = rowId(model);
      try {
        await removeLocalModel({ endpoint: model.provider_id, model_key: model.model_key });
        setNotice({
          id,
          tone: "bg-quiet-soft text-muted",
          text: `Removed ${model.model_key} from ${model.endpoint_name}.`,
        });
        load();
      } catch (cause: unknown) {
        setNotice({
          id,
          tone: "bg-danger-soft text-danger",
          text: cause instanceof ApiError ? cause.message : "The model could not be removed.",
        });
      }
    },
    [load],
  );

  if (error && !data) {
    return (
      <div data-local-models-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={() => load()}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!data) return <LoadingTable columns={6} rows={4} />;

  return (
    <div data-local-models className="flex flex-col gap-4">
      <div className="grid gap-3 sm:grid-cols-4">
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Models</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.total_models}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Available</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.available}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Resident in memory</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.resident}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface p-4">
          <p className="text-[12px] text-muted">Pulling now</p>
          <p className="mt-1 text-2xl font-semibold text-ink">{data.pulling}</p>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <nav aria-label="Model status" className="flex flex-wrap gap-1">
          {STATUS_TABS.map((tab) => (
            <button
              key={tab.key || "all"}
              type="button"
              aria-pressed={status === tab.key}
              onClick={() => setStatus(tab.key)}
              className={`rounded-full px-3 py-1 text-[12px] transition ${
                status === tab.key
                  ? "bg-accent-soft text-accent-strong"
                  : "text-muted hover:bg-canvas"
              }`}
            >
              {tab.label}
            </button>
          ))}
        </nav>
        <div className="flex items-center gap-2">
          <div className="relative w-full sm:w-64">
            <Search
              aria-hidden
              className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted"
              size={15}
            />
            <input
              type="search"
              value={q}
              onChange={(event) => setQ(event.target.value)}
              placeholder="Search by key or name"
              aria-label="Search local models"
              className="w-full rounded-lg border border-line bg-surface py-2 pl-9 pr-3 text-[13px]"
            />
          </div>
          <button
            type="button"
            onClick={() => load()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
          >
            {busy ? <Loader2 aria-hidden size={14} className="animate-spin" /> : <RefreshCw aria-hidden size={14} />}
            Refresh
          </button>
        </div>
      </div>

      {error ? (
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger">{error}</p>
      ) : null}

      {data.is_empty ? (
        <EmptyState
          title="No local models registered"
          hint="Scan an endpoint from the local overview to record what it serves, or pull a model by key."
          action={
            <a
              href="/ai/local"
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
            >
              Open the local overview
            </a>
          }
        />
      ) : models.length === 0 ? (
        <EmptyState
          title="Nothing matches this filter"
          hint="Clear the search or pick another status to see the rest of the table."
          action={
            <button
              type="button"
              onClick={() => {
                setQ("");
                setStatus("");
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
            >
              Clear filters
            </button>
          }
        />
      ) : (
        <ul className="flex flex-col gap-2">
          {models.map((model) => {
            const id = rowId(model);
            const chips = capabilityChips(model);
            return (
              <li
                key={model.id}
                data-local-model-row
                className="flex flex-col gap-2.5 rounded-xl border border-line bg-surface p-4"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-[14px] font-medium text-ink">{model.model_key}</span>
                  <span
                    data-local-model-status
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
                      STATUS_TONE[model.status] ?? "bg-quiet-soft text-muted"
                    }`}
                  >
                    {model.status}
                  </span>
                  {model.resident ? (
                    <span className="rounded-full bg-positive-soft px-2 py-0.5 text-[11px] font-medium text-positive">
                      Resident
                    </span>
                  ) : null}
                </div>
                <p className="text-[12px] text-muted">
                  {model.endpoint_name}
                  {model.display_name ? ` · ${model.display_name}` : ""}
                </p>
                <p className="text-[12px] text-muted">{specLine(model)}</p>
                {chips.length > 0 ? (
                  <p className="flex flex-wrap gap-1">
                    {chips.map((chip) => (
                      <span
                        key={chip}
                        className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                      >
                        {chip}
                      </span>
                    ))}
                  </p>
                ) : null}

                {model.status === "pulling" ? (
                  <div data-local-model-progress className="flex flex-col gap-1.5">
                    {/* A block element in its own row, not an overlay: the request asks that
                        progress not overlap the model key column, and this cannot. */}
                    <progress
                      className="h-1.5 w-full"
                      value={model.pull_progress}
                      max={100}
                      aria-label={`Pull progress for ${model.model_key}`}
                    />
                    <p className="text-[11px] text-muted">
                      {model.pull_progress}% ·{" "}
                      {model.pull_message ?? "the server has not reported progress yet"}
                    </p>
                  </div>
                ) : null}

                {model.status === "error" && model.pull_message ? (
                  <p
                    data-local-model-error
                    className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
                  >
                    {model.pull_message}
                  </p>
                ) : null}

                <p className="text-[11px] text-muted">
                  Updated {formatTimestamp(model.updated_at)}
                  {model.last_used_at
                    ? ` · last served a request ${formatTimestamp(model.last_used_at)}`
                    : ""}
                </p>

                {notice?.id === id ? (
                  <p
                    data-local-model-row-notice
                    className={`rounded-lg px-3 py-2 text-[12px] ${notice.tone}`}
                  >
                    {notice.text}
                  </p>
                ) : null}

                <div className="flex flex-wrap items-center gap-2">
                  {model.status === "missing" ? (
                    <button
                      type="button"
                      onClick={() => void pull(model, "pull")}
                      data-local-model-action="pull"
                      className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
                    >
                      <Download aria-hidden size={14} />
                      Pull
                    </button>
                  ) : null}
                  {model.status === "pulling" ? (
                    <button
                      type="button"
                      onClick={() => void cancel(model)}
                      data-local-model-action="cancel"
                      className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                    >
                      <X aria-hidden size={14} />
                      Cancel
                    </button>
                  ) : null}
                  {model.status === "error" ? (
                    <button
                      type="button"
                      onClick={() => void pull(model, "retry")}
                      data-local-model-action="retry"
                      className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
                    >
                      <RefreshCw aria-hidden size={14} />
                      Retry
                    </button>
                  ) : null}
                  {model.status === "pulling" ? (
                    // Not a disabled control: a pull in flight cannot be removed, and saying so
                    // beats a greyed-out button nobody can interpret.
                    <span className="text-[12px] text-muted">
                      A model being pulled cannot be removed
                    </span>
                  ) : (
                    <button
                      type="button"
                      onClick={() => void remove(model)}
                      data-local-model-action="remove"
                      className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                    >
                      Remove
                    </button>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
