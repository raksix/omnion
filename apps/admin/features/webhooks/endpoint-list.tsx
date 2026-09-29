"use client";

/**
 * The endpoint list: where an organization sends its events, and whether the receiver is
 * still listening.
 *
 * The screen answers two questions that a table of endpoints cannot, and both are the reason
 * it is a screen rather than a list:
 *
 * 1. **Is this receiver working?** A success rate and a last-delivery state per row, so the
 *    answer is in the list rather than two clicks away. The rate is the API's number and is
 *    never recomputed here: it counts settled *traffic* only, and a panel that divided by
 *    everything it received would show a different, worse figure than the API's own.
 * 2. **What happens when I press something here?** A `disabled` endpoint still has a history
 *    worth reading and a receiver worth testing, so Disable is not Delete and the confirm
 *    says which one it is.
 *
 * Two claims this screen makes, each one a way a list lies:
 *
 * - **The filters live in the query string.** A filtered list that cannot be pasted to a
 *   colleague is one that has to be rebuilt by hand, and the rebuild is where mistakes
 *   happen.
 * - **An endpoint with no deliveries is not an endpoint with a 0% success rate.** The first
 *   has never been asked to do anything; the second has failed at it. The column says which,
 *   because the difference decides whether anybody goes looking at the receiver.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Ban,
  CircleCheck,
  CircleSlash,
  ExternalLink,
  Play,
  Plus,
  RefreshCw,
  Search,
  Trash2,
  TriangleAlert,
  Webhook,
  X,
} from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import {
  ApiError,
  deleteWebhookEndpoint,
  fetchWebhookEndpoints,
  fetchWebhookStats,
  testWebhookEndpoint,
  updateWebhookEndpoint,
} from "@/lib/api";
import type { WebhookEndpoint } from "@/lib/types";

/** The list's own page size. The API clamps to 1..200; the list is not paginated. */
const STATUSES = [
  { value: "", label: "Any status" },
  { value: "enabled", label: "Enabled" },
  { value: "disabled", label: "Disabled" },
] as const;

/** What one row knows about its receiver, as of the last read. */
type RowHealth = {
  successRate: number | null;
  pending: number;
  failed: number;
};

/** The host part of a URL, for the column that is narrow enough to read. */
function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** `page.*` first, then the rest: a stored subscription list is alphabetical, and a
 *  group wildcard is the reason the list is long. */
function subscriptionChips(endpoint: WebhookEndpoint): { groups: string[]; names: string[] } {
  const groups: string[] = [];
  const names: string[] = [];
  for (const entry of endpoint.events) {
    if (entry.endsWith(".*")) groups.push(entry);
    else names.push(entry);
  }
  return { groups, names };
}

/** "3 minutes ago" — the value an operator compares against now. */
function relativeTime(value: string | null): string {
  if (!value) return "—";
  const then = new Date(value).getTime();
  if (Number.isNaN(then)) return "—";
  const seconds = Math.round((Date.now() - then) / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  return `${days} d ago`;
}

export function WebhookListScreen() {
  const router = useRouter();
  const params = useSearchParams();
  const search = params?.toString() ?? "";
  const query = new URLSearchParams(search).get("q") ?? "";
  const status = new URLSearchParams(search).get("status") ?? "";

  const [endpoints, setEndpoints] = useState<WebhookEndpoint[]>([]);
  const [health, setHealth] = useState<Record<string, RowHealth>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [cursorIndex, setCursorIndex] = useState(-1);
  const searchInput = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const list = await fetchWebhookEndpoints();
      setEndpoints(list.webhooks);

      // One stats read per endpoint, in parallel. The alternative — a list endpoint that
      // carried its own success rate — would put a per-row aggregate inside the list query,
      // and an endpoint with a large history would make the list slow for every operator on
      // every page load. The requests are independent, so they do not need to be serial.
      const reads = await Promise.all(
        list.webhooks.map(async (endpoint) => {
          try {
            const stats = await fetchWebhookStats(endpoint.id);
            return [
              endpoint.id,
              {
                successRate: stats.success_rate,
                pending: stats.pending,
                failed: stats.failed,
              },
            ] as const;
          } catch {
            // One endpoint's stats failing must not blank the whole list: the row still
            // renders, and its health column says nothing rather than claiming a number.
            return [endpoint.id, null] as const;
          }
        }),
      );
      setHealth(Object.fromEntries(reads.filter((row) => row[1] !== null)) as Record<string, RowHealth>);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const write = useCallback(
    (mutate: (next: URLSearchParams) => void) => {
      const next = new URLSearchParams(search);
      mutate(next);
      const value = next.toString();
      router.replace(value ? `/webhooks?${value}` : "/webhooks");
    },
    [router, search],
  );

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return endpoints.filter((endpoint) => {
      if (status === "enabled" && !endpoint.enabled) return false;
      if (status === "disabled" && endpoint.enabled) return false;
      if (!needle) return true;
      return (
        endpoint.name.toLowerCase().includes(needle) ||
        endpoint.url.toLowerCase().includes(needle) ||
        endpoint.events.some((entry) => entry.toLowerCase().includes(needle))
      );
    });
  }, [endpoints, query, status]);

  const activeFilters = (query ? 1 : 0) + (status ? 1 : 0);

  const setEnabled = async (endpoint: WebhookEndpoint, enabled: boolean) => {
    setBusy(endpoint.id);
    setError(null);
    try {
      await updateWebhookEndpoint(endpoint.id, { enabled });
      setNotice(
        enabled
          ? `${endpoint.name} delivers again.`
          : `${endpoint.name} stops delivering. Its history stays readable.`,
      );
      await load();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(null);
    }
  };

  const sendTest = async (endpoint: WebhookEndpoint) => {
    setBusy(endpoint.id);
    setError(null);
    try {
      const report = await testWebhookEndpoint(endpoint.id);
      setNotice(
        report.deliveries > 0
          ? `Test delivery queued for ${endpoint.name}. Open it to watch the attempt.`
          : `The test delivery to ${endpoint.name} was queued but has no endpoint left to reach.`,
      );
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(null);
    }
  };

  const remove = async (endpoint: WebhookEndpoint) => {
    setBusy(endpoint.id);
    setError(null);
    try {
      await deleteWebhookEndpoint(endpoint.id);
      setConfirmDelete(null);
      setNotice(`${endpoint.name} and its queue history are gone.`);
      await load();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(null);
    }
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLTableSectionElement>) => {
    if (event.key === "/" && searchInput.current) {
      event.preventDefault();
      searchInput.current.focus();
      return;
    }
    if (event.key === "n") {
      event.preventDefault();
      router.push("/webhooks/new");
      return;
    }
    if (event.key === "Escape") {
      if (confirmDelete) {
        event.preventDefault();
        setConfirmDelete(null);
        return;
      }
      if (activeFilters > 0) {
        event.preventDefault();
        router.replace("/webhooks");
      }
      return;
    }
    if (event.key === "j" || event.key === "ArrowDown") {
      event.preventDefault();
      setCursorIndex((index) => Math.min(index + 1, visible.length - 1));
      return;
    }
    if (event.key === "k" || event.key === "ArrowUp") {
      event.preventDefault();
      setCursorIndex((index) => Math.max(index - 1, 0));
      return;
    }
    const row = visible[cursorIndex];
    if (row && event.key === "Enter") {
      event.preventDefault();
      router.push(`/webhooks/${row.id}`);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <section
        aria-label="Endpoint filters"
        data-webhook-filters
        className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface p-3"
      >
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Search</span>
          <input
            ref={searchInput}
            id="webhook-search"
            type="search"
            value={query}
            placeholder="Name, URL or event"
            onChange={(event) =>
              write((next) => {
                if (event.target.value) next.set("q", event.target.value);
                else next.delete("q");
              })
            }
            className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          />
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span className="font-medium">Status</span>
          <select
            id="webhook-status"
            value={status}
            onChange={(event) =>
              write((next) => {
                if (event.target.value) next.set("status", event.target.value);
                else next.delete("status");
              })
            }
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            {STATUSES.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>

        <button
          type="button"
          onClick={() => void load()}
          data-webhook-refresh
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Refresh
        </button>

        {activeFilters > 0 ? (
          <button
            type="button"
            onClick={() => router.replace("/webhooks")}
            data-webhook-reset
            className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
          >
            Reset filters
          </button>
        ) : null}

        <Link
          href="/webhooks/new"
          data-webhook-new
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          New endpoint
        </Link>
      </section>

      {notice ? (
        <p
          data-webhook-notice
          role="status"
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] text-muted"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          data-webhook-error
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load()} className="text-accent-strong hover:underline">
            Retry
          </button>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {loading && endpoints.length === 0 ? (
          <div aria-busy="true" data-webhook-skeleton className="flex flex-col gap-2 p-4">
            {Array.from({ length: 5 }, (_, index) => (
              <div key={index} className="h-9 animate-pulse rounded bg-quiet-soft" />
            ))}
          </div>
        ) : endpoints.length === 0 ? (
          <div
            data-webhook-empty
            className="flex flex-col items-center gap-3 px-4 py-16 text-center"
          >
            <Webhook className="size-6 text-muted" aria-hidden />
            <div>
              <p className="text-[13.5px] font-medium">No endpoint is connected yet.</p>
              <p className="max-w-sm text-[12.5px] text-muted">
                An endpoint is a URL that receives one signed POST per subscribed event. The
                secret that signs it is shown once, when it is created.
              </p>
            </div>
            <Link
              href="/webhooks/new"
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white hover:bg-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              Connect your first endpoint
            </Link>
          </div>
        ) : visible.length === 0 ? (
          <div data-webhook-filtered-empty className="px-4 py-12 text-center">
            <p className="text-[13px] font-medium">No endpoint matches these filters.</p>
            <p className="text-[12.5px] text-muted">
              {endpoints.length} endpoint{endpoints.length === 1 ? "" : "s"} exist outside them.
            </p>
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table data-webhook-table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    Name
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    Receiver
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    Events
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    Status
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    24 h
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    Created
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody onKeyDown={onKeyDown} tabIndex={0} aria-label="Webhook endpoints">
                {visible.map((endpoint, index) => {
                  const isCursor = index === cursorIndex;
                  const row = health[endpoint.id];
                  const { groups, names } = subscriptionChips(endpoint);
                  return (
                    <tr
                      key={endpoint.id}
                      data-webhook-row={endpoint.id}
                      data-cursor={isCursor ? "true" : undefined}
                      className={`border-b border-line/60 transition hover:bg-quiet-soft ${
                        isCursor ? "bg-accent-soft" : ""
                      }`}
                    >
                      <td className="px-3 py-2.5">
                        <Link
                          href={`/webhooks/${endpoint.id}`}
                          data-webhook-open={endpoint.id}
                          className="font-medium text-ink hover:underline"
                        >
                          {endpoint.name}
                        </Link>
                      </td>
                      <td className="px-3 py-2.5">
                        {/* The host is the readable column and the full URL is one hover away:
                            a 2048-character URL in a table is a column nobody can scan. */}
                        <span className="block max-w-[16rem] truncate text-muted" title={endpoint.url}>
                          {hostOf(endpoint.url)}
                        </span>
                      </td>
                      <td className="px-3 py-2.5">
                        <span className="flex flex-wrap items-center gap-1">
                          {groups.map((group) => (
                            <span
                              key={group}
                              className="rounded-full bg-accent-soft px-1.5 py-0.5 text-[11px] text-accent-strong"
                            >
                              {group}
                            </span>
                          ))}
                          {names.slice(0, 2).map((name) => (
                            <span
                              key={name}
                              className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                            >
                              {name}
                            </span>
                          ))}
                          {names.length > 2 ? (
                            <span className="text-[11px] text-muted">+{names.length - 2}</span>
                          ) : null}
                          <span className="sr-only">
                            {endpoint.events.length} events subscribed
                          </span>
                        </span>
                      </td>
                      <td className="px-3 py-2.5">
                        {endpoint.enabled ? (
                          <span className="inline-flex items-center gap-1.5 text-[12px] text-positive">
                            <CircleCheck className="size-3.5" aria-hidden />
                            Enabled
                          </span>
                        ) : (
                          <span className="inline-flex items-center gap-1.5 text-[12px] text-muted">
                            <CircleSlash className="size-3.5" aria-hidden />
                            Disabled
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2.5 tabular-nums">
                        {/* No deliveries is not a 0%: the first says the endpoint was never
                            asked to do anything, the second says it failed. */}
                        {row ? (
                          <span
                            data-webhook-rate={endpoint.id}
                            className={
                              row.failed > 0 && (row.successRate ?? 1) < 0.9
                                ? "text-caution"
                                : "text-muted"
                            }
                          >
                            {row.successRate === null
                              ? "no traffic"
                              : `${Math.round(row.successRate * 100)}%`}
                            {row.pending > 0 ? (
                              <span className="text-muted"> · {row.pending} waiting</span>
                            ) : null}
                          </span>
                        ) : (
                          <span className="text-muted">—</span>
                        )}
                      </td>
                      <td className="px-3 py-2.5 text-muted">{relativeTime(endpoint.created_at)}</td>
                      <td className="px-3 py-2.5">
                        <div className="flex items-center justify-end gap-1">
                          <button
                            type="button"
                            title="Send a test delivery"
                            aria-label={`Send a test delivery to ${endpoint.name}`}
                            data-webhook-test={endpoint.id}
                            disabled={busy === endpoint.id}
                            onClick={() => void sendTest(endpoint)}
                            className="rounded p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink disabled:opacity-50"
                          >
                            <Play className="size-3.5" aria-hidden />
                          </button>
                          <button
                            type="button"
                            title={endpoint.enabled ? "Disable deliveries" : "Enable deliveries"}
                            aria-label={
                              endpoint.enabled
                                ? `Disable ${endpoint.name}`
                                : `Enable ${endpoint.name}`
                            }
                            data-webhook-toggle={endpoint.id}
                            disabled={busy === endpoint.id}
                            onClick={() => void setEnabled(endpoint, !endpoint.enabled)}
                            className="rounded p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink disabled:opacity-50"
                          >
                            {endpoint.enabled ? (
                              <Ban className="size-3.5" aria-hidden />
                            ) : (
                              <ExternalLink className="size-3.5" aria-hidden />
                            )}
                          </button>
                          <button
                            type="button"
                            title="Remove the endpoint"
                            aria-label={`Remove ${endpoint.name}`}
                            data-webhook-delete={endpoint.id}
                            disabled={busy === endpoint.id}
                            onClick={() => setConfirmDelete(endpoint.id)}
                            className="rounded p-1.5 text-muted transition hover:bg-quiet-soft hover:text-red-600 disabled:opacity-50"
                          >
                            <Trash2 className="size-3.5" aria-hidden />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Delete is confirmed by name, not by a generic "are you sure". The two actions an
          operator confuses most — Disable and Remove — end in opposite states, and the
          sentence is what tells them apart before they press it. */}
      {confirmDelete ? (
        <div
          data-webhook-confirm
          role="alertdialog"
          aria-label="Remove the endpoint"
          className="flex flex-wrap items-center gap-3 rounded-xl border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="size-4 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">
            Remove <strong>{endpoints.find((row) => row.id === confirmDelete)?.name}</strong> and
            its delivery history? Subscribers that only this endpoint served stop being told.
          </span>
          <button
            type="button"
            onClick={() => setConfirmDelete(null)}
            className="rounded-lg border border-line px-2.5 py-1.5 text-muted hover:text-ink"
          >
            <X className="size-3.5" aria-hidden />
            Keep it
          </button>
          <button
            type="button"
            data-webhook-confirm-remove
            disabled={busy === confirmDelete}
            onClick={() => {
              const target = endpoints.find((row) => row.id === confirmDelete);
              if (target) void remove(target);
            }}
            className="rounded-lg bg-red-600 px-2.5 py-1.5 font-medium text-white hover:bg-red-700 disabled:opacity-50"
          >
            Remove it
          </button>
        </div>
      ) : null}

      <p className="text-[11.5px] text-muted">
        Press <kbd className="rounded border border-line px-1">/</kbd> to search,{" "}
        <kbd className="rounded border border-line px-1">n</kbd> for a new endpoint,{" "}
        <kbd className="rounded border border-line px-1">j</kbd>/
        <kbd className="rounded border border-line px-1">k</kbd> to move,{" "}
        <kbd className="rounded border border-line px-1">Enter</kbd> to open.
      </p>
    </div>
  );
}
