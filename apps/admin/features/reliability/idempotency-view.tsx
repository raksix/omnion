"use client";

/**
 * `/settings/reliability/idempotency` — the keyed-write ledger (REQ-127, slice 2).
 *
 * This screen answers one operator question with three numbers and a table: *is anything stuck?*
 * A key left `in_progress` is a write whose client is still retrying into a `409` that will
 * never resolve, so the screen leads with the stuck count rather than with the total.
 *
 * Four things it refuses to blur, because each pair otherwise renders as the same chip:
 *
 * - **`completed` is not `has_response`.** A key can be `completed` with nothing stored inline
 *   — a body over the cap went to the object store — and a row that says "replays" when it
 *   holds a reference is a promise the platform cannot keep. The tile says `replays the stored
 *   response` only when there is one.
 * - **`in_progress` is not an error.** It is a claim held by a running request, and the correct
 *   response is usually to wait. It becomes a problem only at the execution deadline, and the
 *   release action is what an operator reaches for at that point.
 * - **A replayed key is not a duplicate write.** `replay_count` is the number of times the
 *   platform answered from the store instead of running the handler, which is the feature
 *   working, not a duplicate. Labelling it as an error would train an operator to release keys
 *   that are doing exactly their job.
 * - **The detail pane never shows a stored body.** The API does not return one (the store must
 *   not become a second request archive), and this screen does not offer a way to ask for it.
 *   What it shows instead is the size against the cap and the reference when the body went to
 *   object storage — which is what an operator needs to answer "where did the response go".
 *
 * Release is the one destructive action here, so it is behind a dialog that asks for a reason
 * (the API refuses an empty one) and names the key it will release.
 *
 * Keyboard: `/` filters, `r` releases the first stuck key, `Esc` closes the pane. Under `sm:`
 * the table becomes cards and the release button stays reachable without a hover.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  Clock,
  FileWarning,
  KeyRound,
  Loader2,
  RefreshCw,
  RotateCcw,
  Search,
  X,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchIdempotencyKey,
  fetchIdempotencyKeys,
  releaseIdempotencyKey,
  type IdempotencyKey,
  type IdempotencyKeyDetail,
  type IdempotencyKeys,
} from "@/lib/api";

/** The three states the vocabulary defines, in the order an operator reads them. */
const STATES = ["all", "in_progress", "completed", "failed"] as const;

/** What each state means, in a sentence rather than a name. */
const STATE_WORDS: Record<string, string> = {
  in_progress: "a claim held by a request that has not finished",
  completed: "the write finished and its response is stored",
  failed: "the attempt left nothing replayable, so the next one runs again",
};

/** A state chip that is readable without colour, because the words carry the meaning. */
function StateChip({ state }: { state: string }) {
  const tone =
    state === "completed"
      ? "border-emerald-500/40 text-emerald-700 dark:text-emerald-400"
      : state === "in_progress"
        ? "border-amber-500/40 text-amber-700 dark:text-amber-400"
        : "border-danger/40 text-danger";
  return (
    <span
      data-testid={`state-${state}`}
      className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11.5px] ${tone}`}
    >
      {state === "completed" ? (
        <CheckCircle2 className="size-3" aria-hidden />
      ) : state === "in_progress" ? (
        <Clock className="size-3" aria-hidden />
      ) : (
        <FileWarning className="size-3" aria-hidden />
      )}
      {state}
    </span>
  );
}

/** A size rendered in the unit a person reads, never a raw byte count. */
function humanBytes(bytes: number | null): string {
  if (bytes === null) return "—";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** A timestamp in the operator's local zone, with the raw value as the tooltip. */
function when(value: string | null): string {
  if (!value) return "—";
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime())
    ? value
    : parsed.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/** The release dialog. The reason is required, and the API refuses an empty one. */
function ReleaseDialog({
  target,
  busy,
  onCancel,
  onConfirm,
}: {
  target: IdempotencyKey | null;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (reason: string) => void;
}) {
  const [reason, setReason] = useState("");
  useEffect(() => {
    if (target) setReason("");
  }, [target]);

  useEffect(() => {
    if (!target) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [target, onCancel]);

  if (!target) return null;
  const tooShort = reason.trim().length === 0;

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-4 sm:items-center"
      role="dialog"
      aria-modal="true"
      aria-label="Release idempotency key"
      data-testid="release-dialog"
    >
      <form
        className="w-full max-w-md space-y-4 rounded-lg border border-line bg-panel p-5 shadow-xl"
        onSubmit={(event) => {
          event.preventDefault();
          if (!tooShort) onConfirm(reason.trim());
        }}
      >
        <div className="flex items-start gap-2">
          <RotateCcw className="mt-0.5 size-4 shrink-0" aria-hidden />
          <div>
            <h2 className="text-[14px] font-medium">Release this key</h2>
            <p className="mt-1 text-[12.5px] text-muted">
              <span className="font-mono text-[12px]">{target.key}</span> on{" "}
              <span className="font-mono text-[12px]">{target.scope}</span> is held as{" "}
              <span className="font-medium">in_progress</span>. Releasing it frees the key so the
              next attempt of that write runs again — which means <em>the write may run twice</em>{" "}
              if the first attempt is still alive somewhere.
            </p>
          </div>
        </div>

        <div>
          <label htmlFor="release-reason" className="block text-[12px] font-medium text-muted">
            Why is it being released?
          </label>
          <textarea
            id="release-reason"
            value={reason}
            onChange={(event) => setReason(event.target.value)}
            rows={3}
            autoFocus
            data-testid="release-reason"
            aria-describedby="release-reason-help"
            className="mt-1 w-full rounded-md border border-line bg-elevated px-2.5 py-1.5 text-[13px] outline-none focus-visible:border-accent"
          />
          <p id="release-reason-help" className="mt-1 text-[12px] text-muted">
            Required. It is written to the audit trail and to the{" "}
            <span className="font-mono text-[11.5px]">idempotency.keys.released</span> event.
          </p>
        </div>

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={tooShort || busy}
            data-testid="release-confirm"
            className="inline-flex items-center gap-1.5 rounded-md bg-danger px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <RotateCcw className="size-3.5" aria-hidden />}
            Release the key
          </button>
        </div>
      </form>
    </div>
  );
}

/** One key's metadata, in a side panel rather than a route — the list stays scrolled. */
function KeyDetail({ detail }: { detail: IdempotencyKeyDetail }) {
  return (
    <div className="space-y-3" data-testid="key-detail">
      <div className="flex flex-wrap items-center gap-2">
        <StateChip state={detail.state} />
        <span className="font-mono text-[12.5px]">{detail.key}</span>
      </div>

      <dl className="grid grid-cols-1 gap-x-4 gap-y-2 text-[12.5px] sm:grid-cols-2">
        <div>
          <dt className="text-muted">First attempt</dt>
          <dd className="font-mono text-[12px]">
            {detail.method} {detail.path}
          </dd>
        </div>
        <div>
          <dt className="text-muted">Stored status</dt>
          <dd className="tabular-nums">{detail.response_status ?? "— not finished"}</dd>
        </div>
        <div>
          <dt className="text-muted">Replays served</dt>
          <dd className="tabular-nums" data-testid="detail-replays">
            {detail.replay_count}
          </dd>
        </div>
        <div>
          <dt className="text-muted">Expires</dt>
          <dd title={detail.created_expires_at}>{when(detail.created_expires_at)}</dd>
        </div>
        <div>
          <dt className="text-muted">Finished</dt>
          <dd title={detail.completed_at ?? undefined}>{when(detail.completed_at)}</dd>
        </div>
        <div>
          <dt className="text-muted">Original request</dt>
          <dd className="font-mono text-[11.5px]" data-testid="detail-original-request">
            {detail.original_request_id ?? "— not recorded"}
          </dd>
        </div>
      </dl>

      <div className="rounded-md border border-line p-3 text-[12.5px]">
        <p className="text-muted">Stored response</p>
        <p className="mt-1" data-testid="detail-body">
          {detail.stored_body
            ? `${humanBytes(detail.stored_body_bytes)} kept inline, against a ${humanBytes(detail.inline_cap_bytes)} cap.`
            : detail.response_body_ref
              ? `Too large to keep inline — it is at ${detail.response_body_ref}.`
              : "Nothing stored. A replay of this key cannot be answered from the store."}
        </p>
        <p className="mt-1 text-muted">
          The body itself is never returned by this screen: the store must not become a second
          request archive.
        </p>
      </div>

      <p className="text-[12.5px]" data-testid="detail-next-attempt">
        The next attempt of this key{" "}
        <span className="font-medium">{detail.next_attempt}</span>.
      </p>
    </div>
  );
}

export function ReliabilityIdempotencyView() {
  const [document, setDocument] = useState<IdempotencyKeys | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ApiError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [state, setState] = useState<(typeof STATES)[number]>("all");
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<IdempotencyKeyDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [releasing, setReleasing] = useState<IdempotencyKey | null>(null);
  const [releaseBusy, setReleaseBusy] = useState(false);
  const searchRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setDocument(await fetchIdempotencyKeys({ state: state === "all" ? null : state }));
      setError(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setLoading(false);
    }
  }, [state]);

  useEffect(() => {
    void load();
  }, [load]);

  async function openDetail(key: string) {
    setSelected(key);
    setDetail(null);
    setDetailLoading(true);
    try {
      setDetail(await fetchIdempotencyKey(key));
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setDetailLoading(false);
    }
  }

  async function release(reason: string) {
    if (!releasing) return;
    setReleaseBusy(true);
    try {
      const answer = await releaseIdempotencyKey(releasing.key, reason);
      setNotice(answer.message);
      setReleasing(null);
      if (selected === releasing.key) void openDetail(releasing.key);
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught : new ApiError(0, "request_failed", String(caught)));
    } finally {
      setReleaseBusy(false);
    }
  }

  const rows = useMemo(() => {
    const all = document?.keys ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter((row) =>
      [row.key, row.scope, row.state].join(" ").toLowerCase().includes(needle),
    );
  }, [document, filter]);

  const stuck = useMemo(
    () => (document?.keys ?? []).filter((row) => row.state === "in_progress"),
    [document],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const element = event.target as HTMLElement | null;
      const typing = element && ["INPUT", "TEXTAREA", "SELECT"].includes(element.tagName);
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "Escape") {
        setSelected(null);
        setDetail(null);
      } else if (event.key === "r" && !typing && stuck.length > 0) {
        setReleasing(stuck[0]);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [stuck]);

  if (loading && !document) {
    return (
      <div className="rounded-lg border border-line">
        <LoadingTable columns={5} />
      </div>
    );
  }

  if (error && !document) {
    return (
      <div className="rounded-lg border border-danger/40 p-6 text-center" data-testid="load-error">
        <XCircle className="mx-auto size-6 text-danger" aria-hidden />
        <p className="mt-2 text-[13.5px] font-medium">The keyed-write ledger could not be read</p>
        <p className="mt-1 text-[12.5px] text-muted">
          {error.message}
          {typeof error.details?.request_id === "string" ? ` · request ${error.details.request_id}` : ""}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-5">
      {error ? (
        <p
          className="flex items-start gap-1.5 rounded-md border border-danger/40 px-3 py-2 text-[12.5px] text-danger"
          data-testid="inline-error"
        >
          <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {error.message}
            {typeof error.details?.request_id === "string" ? ` · request ${error.details.request_id}` : ""}
          </span>
        </p>
      ) : null}
      {notice ? (
        <p
          className="flex items-start gap-1.5 rounded-md border border-emerald-500/40 px-3 py-2 text-[12.5px]"
          data-testid="notice"
        >
          <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>{notice}</span>
        </p>
      ) : null}

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Stuck keys</p>
          <p
            className={`mt-1 text-2xl font-semibold tabular-nums ${
              (document?.in_progress ?? 0) > 0 ? "text-amber-600 dark:text-amber-400" : ""
            }`}
            data-testid="stuck-count"
          >
            {document?.in_progress ?? 0}
          </p>
          <p className="text-[12px] text-muted">
            {(document?.in_progress ?? 0) > 0
              ? "held by a request that has not finished"
              : "nothing is waiting on a running attempt"}
          </p>
        </div>
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Keys held</p>
          <p className="mt-1 text-2xl font-semibold tabular-nums" data-testid="key-total">
            {document?.total ?? 0}
          </p>
          <p className="text-[12px] text-muted">by this account, in every state</p>
        </div>
        <div className="rounded-lg border border-line p-4">
          <p className="text-[12px] text-muted">Replays served</p>
          <p
            className="mt-1 text-2xl font-semibold tabular-nums"
            data-testid="replay-total"
          >
            {(document?.keys ?? []).reduce((sum, row) => sum + row.replay_count, 0)}
          </p>
          <p className="text-[12px] text-muted">
            answers from the store instead of running the handler again
          </p>
        </div>
      </div>

      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <KeyRound className="size-4" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Recent keys</h2>
          <div className="ml-auto flex flex-wrap items-center gap-2">
            <label htmlFor="state-filter" className="sr-only">
              Filter by state
            </label>
            <select
              id="state-filter"
              value={state}
              onChange={(event) => setState(event.target.value as (typeof STATES)[number])}
              data-testid="state-filter"
              className="rounded-md border border-line bg-elevated px-2 py-1.5 text-[12.5px] outline-none focus-visible:border-accent"
            >
              {STATES.map((value) => (
                <option key={value} value={value}>
                  {value === "all" ? "All states" : value}
                </option>
              ))}
            </select>
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden
              />
              <input
                ref={searchRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="Filter  ( / )"
                aria-label="Filter keys"
                data-testid="key-filter"
                className="rounded-md border border-line bg-elevated py-1.5 pl-7 pr-2 text-[12.5px] outline-none focus-visible:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={() => void load()}
              aria-label="Reload"
              className="rounded-md border border-line p-1.5"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
          </div>
        </header>

        {state !== "all" ? (
          <p className="border-b border-line px-4 py-2 text-[12.5px] text-muted" data-testid="state-explanation">
            Showing <span className="font-medium">{state}</span> —{" "}
            {STATE_WORDS[state] ?? "every key in this state"}.
          </p>
        ) : null}

        {rows.length === 0 ? (
          <EmptyState
            title={filter ? "No key matches that filter" : "No keyed writes yet"}
            hint={
              filter
                ? "Clear the filter to see every key this account holds."
                : "A keyed write appears here as soon as a client sends an Idempotency-Key header on a mutating endpoint. Nothing is keyed by default, and that is the safe default."
            }
            action={
              filter ? (
                <button
                  type="button"
                  onClick={() => setFilter("")}
                  className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
                >
                  Clear the filter
                </button>
              ) : undefined
            }
          />
        ) : (
          <>
            {/* The table is `hidden` under `sm` rather than removed, so the mobile cards and the
                desktop rows are the same data rendered twice — never two different queries. */}
            <table className="hidden w-full text-left text-[12.5px] sm:table" data-testid="key-table">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-4 py-2 font-medium">Key</th>
                  <th className="px-4 py-2 font-medium">Endpoint family</th>
                  <th className="px-4 py-2 font-medium">State</th>
                  <th className="px-4 py-2 text-right font-medium">Replays</th>
                  <th className="px-4 py-2 font-medium">Expires</th>
                  <th className="px-4 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr key={`${row.scope}|${row.key}`} className="border-b border-line/60 last:border-0">
                    <td className="px-4 py-2 font-mono text-[12px]">{row.key}</td>
                    <td className="px-4 py-2 font-mono text-[12px] text-muted">{row.scope}</td>
                    <td className="px-4 py-2">
                      <StateChip state={row.state} />
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums">{row.replay_count}</td>
                    <td className="px-4 py-2 text-muted" title={row.expires_at}>
                      {when(row.expires_at)}
                    </td>
                    <td className="px-4 py-2 text-right">
                      <span className="inline-flex items-center gap-1">
                        <button
                          type="button"
                          onClick={() => void openDetail(row.key)}
                          data-testid={`detail-open-${row.key}`}
                          className="rounded-md border border-line px-2 py-1 text-[12px]"
                        >
                          Details
                        </button>
                        {row.state === "in_progress" ? (
                          <button
                            type="button"
                            onClick={() => setReleasing(row)}
                            data-testid={`release-${row.key}`}
                            className="inline-flex items-center gap-1 rounded-md border border-danger/50 px-2 py-1 text-[12px] text-danger"
                          >
                            <RotateCcw className="size-3" aria-hidden />
                            Release
                          </button>
                        ) : null}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            <ul className="divide-y divide-line/60 sm:hidden" data-testid="key-cards">
              {rows.map((row) => (
                <li key={`${row.scope}|${row.key}`} className="space-y-2 p-4">
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate font-mono text-[12px]">{row.key}</span>
                    <StateChip state={row.state} />
                  </div>
                  <p className="truncate font-mono text-[11.5px] text-muted">{row.scope}</p>
                  <p className="text-[12px] text-muted">
                    {row.replay_count} replays · expires {when(row.expires_at)}
                  </p>
                  <div className="flex gap-2">
                    <button
                      type="button"
                      onClick={() => void openDetail(row.key)}
                      className="rounded-md border border-line px-2 py-1 text-[12px]"
                    >
                      Details
                    </button>
                    {row.state === "in_progress" ? (
                      <button
                        type="button"
                        onClick={() => setReleasing(row)}
                        data-testid={`release-card-${row.key}`}
                        className="inline-flex items-center gap-1 rounded-md border border-danger/50 px-2 py-1 text-[12px] text-danger"
                      >
                        <RotateCcw className="size-3" aria-hidden />
                        Release
                      </button>
                    ) : null}
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {selected ? (
        <section className="rounded-lg border border-line p-4" aria-label="Key detail">
          <header className="mb-3 flex items-center gap-2">
            <h2 className="text-[13.5px] font-medium">Key detail</h2>
            <button
              type="button"
              onClick={() => {
                setSelected(null);
                setDetail(null);
              }}
              aria-label="Close the key detail"
              data-testid="detail-close"
              className="ml-auto rounded-md border border-line p-1.5"
            >
              <X className="size-3.5" aria-hidden />
            </button>
          </header>
          {detailLoading ? (
            <p className="flex items-center gap-2 text-[12.5px] text-muted">
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
              Reading the stored response metadata…
            </p>
          ) : detail ? (
            <KeyDetail detail={detail} />
          ) : (
            <p className="text-[12.5px] text-muted">That key could not be read.</p>
          )}
        </section>
      ) : null}

      {stuck.length > 0 ? (
        <p
          className="flex items-start gap-1.5 rounded-md border border-amber-500/40 px-3 py-2 text-[12.5px]"
          data-testid="stuck-hint"
        >
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {stuck.length} key{stuck.length === 1 ? " is" : "s are"} held by a running attempt.
            A client retrying one of them is refused with <span className="font-mono text-[11.5px]">409</span>{" "}
            until it finishes. Release one only when the attempt is known to be dead —{" "}
            <button
              type="button"
              onClick={() => setReleasing(stuck[0])}
              className="underline underline-offset-2"
            >
              release it now
            </button>
            .
          </span>
        </p>
      ) : null}

      <ReleaseDialog
        target={releasing}
        busy={releaseBusy}
        onCancel={() => setReleasing(null)}
        onConfirm={(reason) => void release(reason)}
      />
    </div>
  );
}
