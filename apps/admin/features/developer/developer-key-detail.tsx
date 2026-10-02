"use client";

/**
 * One key in full: its scopes, its usage, and its own request history (REQ-022, slice 2).
 *
 * **The usage chart is a per-day rollup the API already computed.** The panel draws it rather
 * than summing the log on the client: the rollup is written by the same recorder that writes
 * the rows, and a client-side sum over a page of rows would answer a different question than the
 * one the log screen shows for the same key.
 *
 * **An empty usage window says which of the two empties it is.** A key that has never been
 * called and a key whose 30-day window has rolled over are the same zero, and the screen says
 * so rather than drawing a flat line that reads as "no traffic" on a key that is being called
 * today.
 */
import { useCallback, useEffect, useState } from "react";

import { ArrowLeft, RefreshCw, TriangleAlert } from "lucide-react";
import Link from "next/link";

import { ApiError, fetchDeveloperKey, fetchDeveloperLogs } from "@/lib/api";
import type { DeveloperKeyDetail, DeveloperLogRow } from "@/lib/developer";
import { formatTimestamp } from "@/lib/format";

type State =
  | { status: "loading" }
  | { status: "ready"; detail: DeveloperKeyDetail; rows: DeveloperLogRow[] }
  | { status: "error"; message: string };

export function DeveloperKeyDetailScreen({ id }: { id: string }) {
  const [state, setState] = useState<State>({ status: "loading" });

  const load = useCallback(async () => {
    setState({ status: "loading" });
    try {
      const [detail, page] = await Promise.all([
        fetchDeveloperKey(id),
        // The key's own history, filtered by the API rather than in the browser: the filter
        // takes an id, and filtering a page of every organization's traffic client-side would
        // need every page of it to be correct.
        fetchDeveloperLogs({ api_key_id: id, window_days: 30 }),
      ]);
      setState({ status: "ready", detail, rows: page.rows });
    } catch (cause: unknown) {
      setState({
        status: "error",
        message:
          cause instanceof ApiError ? cause.message : "This API key could not be loaded.",
      });
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  if (state.status === "loading") {
    return (
      <div className="flex flex-col gap-4" aria-busy="true">
        <div className="h-6 w-48 animate-pulse rounded bg-quiet-soft" />
        <div className="h-32 animate-pulse rounded-xl bg-quiet-soft" />
      </div>
    );
  }

  if (state.status === "error") {
    return (
      <div role="alert" className="rounded-xl border border-line bg-surface px-4 py-6">
        <p className="flex items-center gap-2 text-[13px] text-caution">
          <TriangleAlert className="size-4" aria-hidden />
          {state.message}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const { detail, rows } = state;
  const { key, usage } = detail;
  const totalRequests = usage.reduce((sum, point) => sum + point.requests, 0);
  const totalErrors = usage.reduce((sum, point) => sum + point.errors, 0);
  const peak = usage.reduce((max, point) => Math.max(max, point.requests), 0);
  const predecessor = key.rotated_from;

  return (
    <div className="flex flex-col gap-5" data-developer-key-detail={key.id}>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <Link
            href="/developer/api-keys"
            className="inline-flex items-center gap-1 text-[12px] text-muted hover:text-ink"
          >
            <ArrowLeft className="size-3.5" aria-hidden />
            All API keys
          </Link>
          <h2 className="mt-1 flex items-center gap-2 text-[17px] font-semibold">
            {key.name}
            <span
              className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                key.status === "active"
                  ? "bg-positive-soft text-positive"
                  : "bg-quiet-soft text-muted"
              }`}
              data-key-status={key.status}
            >
              {key.status}
            </span>
          </h2>
          <p className="font-mono text-[12px] text-muted">
            {key.key_prefix} · {key.environment} · created {formatTimestamp(key.created_at)}
          </p>
        </div>
        <Link
          href={`/developer/logs?api_key_id=${key.id}`}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
        >
          See every request
        </Link>
      </div>

      {predecessor ? (
        <p className="rounded-xl border border-line bg-surface px-4 py-2.5 text-[12.5px] text-muted">
          This key replaced an earlier one. The predecessor is revoked and its request history is
          kept under its own id, so a request made before the rotation still names the key that
          made it.
        </p>
      ) : null}

      <section
        aria-labelledby="key-usage"
        className="rounded-xl border border-line bg-surface px-4 py-4"
      >
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h3 id="key-usage" className="text-[13.5px] font-medium">
            Usage · last 30 days
          </h3>
          <span className="text-[12px] text-muted">
            {totalRequests} requests · {totalErrors} refusals
          </span>
        </div>

        {usage.length === 0 ? (
          // The two empties, named separately. A flat zero-width chart would answer neither.
          <p className="mt-3 text-[12.5px] text-muted">
            {key.last_used_at
              ? "No requests in the last 30 days — this key authenticated before the window opened."
              : "This key has never authenticated a request."}
          </p>
        ) : (
          <div className="mt-3 flex h-24 items-end gap-1" data-key-usage-chart>
            {usage.map((point) => (
              <div
                key={point.day}
                title={`${point.day}: ${point.requests} requests, ${point.errors} refusals`}
                className="flex-1"
              >
                <div
                  className={`w-full rounded-t ${point.errors > 0 ? "bg-caution" : "bg-accent"}`}
                  // A zero-request day still gets a hairline, so the window's *width* is the
                  // 30 days the API promised rather than "the days that happened".
                  style={{
                    height: `${Math.max(2, peak === 0 ? 2 : (point.requests / peak) * 96)}px`,
                  }}
                />
              </div>
            ))}
          </div>
        )}
      </section>

      <section
        aria-labelledby="key-scopes"
        className="rounded-xl border border-line bg-surface px-4 py-4"
      >
        <h3 id="key-scopes" className="text-[13.5px] font-medium">
          Scopes
        </h3>
        <p className="mt-1 text-[12px] text-muted">
          A key may only carry a permission the account that issued it already held. Rotating with
          a scope added is how a key grows; it never grows on its own.
        </p>
        <ul className="mt-2 grid grid-cols-1 gap-1 sm:grid-cols-2">
          {key.scopes.map((scope) => (
            <li key={scope} className="font-mono text-[12px]">
              {scope}
            </li>
          ))}
        </ul>
      </section>

      <section
        aria-labelledby="key-requests"
        className="overflow-hidden rounded-xl border border-line bg-surface"
      >
        <h3 id="key-requests" className="border-b border-line px-4 py-3 text-[13.5px] font-medium">
          Recent requests
        </h3>
        {rows.length === 0 ? (
          <p className="px-4 py-8 text-center text-[12.5px] text-muted">
            This key has made no requests in the window the log keeps.
          </p>
        ) : (
          <table className="w-full border-collapse text-left text-[12.5px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th scope="col" className="px-4 py-2 font-medium">Time</th>
                <th scope="col" className="px-4 py-2 font-medium">Call</th>
                <th scope="col" className="px-4 py-2 font-medium">Status</th>
                <th scope="col" className="px-4 py-2 font-medium">Duration</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id} className="border-b border-line last:border-0">
                  <td className="px-4 py-2 whitespace-nowrap text-muted">
                    {formatTimestamp(row.created_at)}
                  </td>
                  <td className="px-4 py-2 font-mono">
                    {row.method} {row.path}
                  </td>
                  <td className="px-4 py-2 tabular-nums">{row.status}</td>
                  <td className="px-4 py-2 tabular-nums text-muted">{row.duration_ms} ms</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}
