"use client";

/**
 * `/environments` — the list screen (REQ-017, slice 2).
 *
 * The screen answers one question: *what copies of this tenant's content exist right now, and
 * which of them is broken.* Three decisions are not obvious, and each of them is a place where the
 * straightforward implementation lies to the operator:
 *
 * - **A clone that is counting is not a clone that is stuck, and the screen has to say which.**
 *   The API refuses to report a percentage until it knows the total (`percent` is 0 while
 *   `items_total` is 0), and that zero is a *different state* from zero progress on a known
 *   total. Rendering both as "0%" makes a counting job look like a hung one, and the operator's
 *   next click is a cancel on a job that was seconds from done. So the bar renders an indeterminate
 *   state whenever `items_total` is 0 and the job is open.
 * - **The re-clone button appears only where the server would accept the request.** The API ships
 *   `reclonable` per row instead of letting the panel derive it; a button that appears on a row
 *   whose request would be refused is a dead button with a worse name than "Clone".
 * - **Filters live in the URL's query string.** A filter that exists only in component state is a
 *   filter a reload loses and a shared link cannot carry, and "the rows on screen match the
 *   server's total" stops being checkable when the total is computed from the visible rows.
 *
 * Keyboard: `/` focuses the filter, `n` opens the wizard, `Esc` closes it. Mobile: the table
 * becomes stacked cards carrying the same `data-*` hooks as the rows, so a depth pass measures the
 * same screen either way.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { Layers, Plus, RefreshCw, Search, X } from "lucide-react";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { ApiError, fetchEnvironments } from "@/lib/api";
import type {
  Environment,
  EnvironmentAreaOption,
  EnvironmentCloneJob,
  EnvironmentFilters,
} from "@/lib/types";

import { EnvironmentCreateWizard } from "./environment-create-wizard";

/** The types the filter offers. Production is not cloneable, so the split is explicit. */
const TYPES = [
  { value: "", label: "Every type" },
  { value: "staging", label: "Staging" },
  { value: "production", label: "Production" },
] as const;

/** The statuses the filter offers. */
const STATUSES = [
  { value: "", label: "Every status" },
  { value: "active", label: "Active" },
  { value: "cloning", label: "Cloning" },
  { value: "error", label: "Error" },
  { value: "archived", label: "Archived" },
] as const;

/** How many rows a page shows. */
const PAGE_SIZE = 50;

/** `true` while a job is counting but has not yet learned its total. */
function isCounting(job: EnvironmentCloneJob | null | undefined): boolean {
  return (
    job !== null &&
    job !== undefined &&
    (job.status === "pending" || job.status === "running") &&
    job.items_total === 0
  );
}

/** The one line under an environment's name: what kind it is and where it is served. */
function subtitle(environment: Environment): string {
  const parts = [environment.type === "production" ? "production" : "staging"];
  if (environment.staging_host) {
    parts.push(environment.staging_host);
  }
  if (environment.type === "production") {
    parts.push("live content");
  }
  return parts.join(" · ");
}

/** The content cell: pages first, then the rest, with a real zero rather than a dash. */
function contentSummary(environment: Environment): string {
  const { pages, translations, workflows, settings, revisions } = environment.content;
  const parts = [`${pages} page${pages === 1 ? "" : "s"}`];
  if (translations > 0) {
    parts.push(`${translations} translation${translations === 1 ? "" : "s"}`);
  }
  if (workflows > 0) {
    parts.push(`${workflows} workflow${workflows === 1 ? "" : "s"}`);
  }
  if (settings > 0) {
    parts.push(`${settings} setting${settings === 1 ? "" : "s"}`);
  }
  if (revisions > 0) {
    parts.push(`${revisions} revision${revisions === 1 ? "" : "s"}`);
  }
  return parts.join(" · ");
}

/**
 * The progress cell.
 *
 * Two states that look identical in a number and mean opposite things: *counting* (no total yet,
 * so no percentage can be honest) and *zero of many* (a real 0%). The first gets an indeterminate
 * bar and the word "counting"; the second gets an empty bar and the counts.
 */
function CloneProgress({ job }: { job: EnvironmentCloneJob }) {
  if (isCounting(job)) {
    return (
      <div className="flex flex-col gap-1" data-env-clone-progress data-env-clone-counting="true">
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-canvas">
          <div className="h-full w-1/3 animate-pulse rounded-full bg-caution" />
        </div>
        <span className="text-[11.5px] text-muted">
          counting — the total is not known yet
        </span>
      </div>
    );
  }
  if (job.status === "failed" || job.status === "cancelled") {
    return (
      <div className="flex flex-col gap-1" data-env-clone-progress data-env-clone-failed="true">
        <span className="text-[11.5px] text-accent-strong">
          {`${job.items_done} of ${job.items_total} copied, then ${job.status}`}
        </span>
        {job.error ? <span className="text-[11px] text-muted">{job.error}</span> : null}
      </div>
    );
  }
  return (
    <div className="flex flex-col gap-1" data-env-clone-progress data-env-clone-percent={job.percent}>
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-canvas"
        role="progressbar"
        aria-valuenow={job.percent}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label="Clone progress"
      >
        <div
          className={`h-full rounded-full ${job.percent === 100 ? "bg-positive" : "bg-accent"}`}
          style={{ width: `${Math.max(job.percent, 2)}%` }}
        />
      </div>
      <span className="text-[11.5px] text-muted">{job.summary}</span>
    </div>
  );
}

/** The list screen. */
export function EnvironmentsView() {
  const router = useRouter();
  const params = useSearchParams();

  // Read the filter out of the URL rather than out of state, and write every change back to it.
  // Two sources of truth for a filter is how "3 of 40 rows" happens.
  const filters = useMemo<EnvironmentFilters>(() => {
    const type = params.get("type") ?? "";
    const status = params.get("status") ?? "";
    const search = params.get("search") ?? "";
    return {
      type: type === "production" || type === "staging" ? type : "",
      status,
      search,
    };
  }, [params]);

  const [environments, setEnvironments] = useState<Environment[]>([]);
  const [areas, setAreas] = useState<EnvironmentAreaOption[]>([]);
  const [sourceKey, setSourceKey] = useState("");
  const [total, setTotal] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [reloadToken, setReloadToken] = useState(0);

  // The wizard's open state lives in the URL, for the same reason the filters do: a modal that
  // only exists in component state cannot be linked to, cannot survive the browser's Back button,
  // and — the reason this tick found it — cannot be measured at a phone width by anything that
  // drives the panel through a URL. `?wizard=1` is the deep link, and every close path clears it,
  // so the address bar never claims a dialog that is not on screen.
  const wizard = params.get("wizard") === "1";
  const setWizard = useCallback(
    (open: boolean) => {
      const query = new URLSearchParams(params.toString());
      if (open) {
        query.set("wizard", "1");
      } else {
        query.delete("wizard");
      }
      const suffix = query.toString();
      router.replace(suffix ? `/environments?${suffix}` : "/environments");
    },
    [params, router],
  );

  const writeFilter = useCallback(
    (next: EnvironmentFilters) => {
      const query = new URLSearchParams();
      if (next.type) {
        query.set("type", next.type);
      }
      if (next.status) {
        query.set("status", next.status);
      }
      if (next.search) {
        query.set("search", next.search);
      }
      // `wizard` is carried across a filter write. Both live in the one address bar now, and a
      // filter change that silently dropped it would close the dialog out from under a person
      // typing in the list behind it — which is the exact shape "two sources of truth" takes when
      // one of them can rewrite the other's parameter.
      if (params.get("wizard") === "1") {
        query.set("wizard", "1");
      }
      const suffix = query.toString();
      router.replace(suffix ? `/environments?${suffix}` : "/environments");
    },
    [params, router],
  );

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    fetchEnvironments({ ...filters, limit: PAGE_SIZE })
      .then((page) => {
        if (cancelled) {
          return;
        }
        setEnvironments(page.environments);
        setAreas(page.areas);
        setSourceKey(page.source_key);
        setTotal(page.total);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof ApiError
              ? cause.message
              : "The environments could not be read.",
          );
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [filters, reloadToken]);

  // Poll only while something is actually copying. A clone on screen is live state; a list that
  // never refreshes is a screenshot of the past.
  const cloning = useMemo(
    () => environments.some((environment) => environment.clone?.status === "running"),
    [environments],
  );
  useEffect(() => {
    if (!cloning) {
      return;
    }
    const timer = setInterval(() => setReloadToken((token) => token + 1), 3000);
    return () => clearInterval(timer);
  }, [cloning]);

  const filtered = Boolean(filters.type || filters.status || filters.search);

  const searchRef = useRef<HTMLInputElement>(null);

  // Keyboard: `/` focuses the filter, `n` opens the wizard, `Esc` closes it. Skipped while the
  // operator is typing, so `/` inside a field types a slash instead of stealing the focus.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (typing) {
        return;
      }
      if (event.key === "/" && !wizard) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "n" && !wizard) {
        event.preventDefault();
        setWizard(true);
        return;
      }
      if (event.key === "Escape" && wizard) {
        setWizard(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [wizard]);

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-col gap-1">
          <h1 className="text-[15px] font-medium">Environments</h1>
          <p className="text-[12px] text-muted">
            {loading
              ? "Reading the environments of this organization…"
              : total === 0
                ? "No environment matches"
                : `Showing ${environments.length} of ${total} · cloning from ${sourceKey}`}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => setReloadToken((token) => token + 1)}
            data-env-reload
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            onClick={() => setWizard(true)}
            data-env-new
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            New staging environment
          </button>
        </div>
      </header>

      {error ? (
        <div className="flex flex-col items-start gap-3 rounded-xl border border-accent/30 bg-accent-soft px-4 py-3">
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
          <button
            type="button"
            onClick={() => setReloadToken((token) => token + 1)}
            className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-[220px] flex-1">
          <Search
            className="pointer-events-none absolute top-1/2 left-3 size-3.5 -translate-y-1/2 text-muted"
            aria-hidden
          />
          <input
            ref={searchRef}
            type="search"
            defaultValue={filters.search ?? ""}
            onChange={(event) => writeFilter({ ...filters, search: event.target.value })}
            placeholder="Search by name or key — press / to focus"
            aria-label="Search the environments"
            data-env-search
            className="w-full rounded-lg border border-line bg-surface py-2 pr-3 pl-8 text-[12.5px]"
          />
        </div>
        <select
          value={filters.type ?? ""}
          onChange={(event) =>
            writeFilter({
              ...filters,
              type: (event.target.value || undefined) as EnvironmentFilters["type"],
            })
          }
          aria-label="Filter by type"
          data-env-type-filter
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          {TYPES.map((type) => (
            <option key={type.value} value={type.value}>
              {type.label}
            </option>
          ))}
        </select>
        <select
          value={filters.status ?? ""}
          onChange={(event) =>
            writeFilter({ ...filters, status: event.target.value || undefined })
          }
          aria-label="Filter by status"
          data-env-status-filter
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          {STATUSES.map((status) => (
            <option key={status.value} value={status.value}>
              {status.label}
            </option>
          ))}
        </select>
        {filtered ? (
          <button
            type="button"
            onClick={() => router.replace("/environments")}
            data-env-filter-clear
            className="inline-flex items-center gap-1 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
          >
            <X className="size-3.5" aria-hidden />
            Clear
          </button>
        ) : null}
      </div>

      {loading ? (
        <LoadingTable columns={5} />
      ) : environments.length === 0 ? (
        filtered ? (
          <EmptyState
            title="Nothing matches that filter"
            hint="No environment of this organization has that type, status or name. Clearing the filter shows them all."
          />
        ) : (
          <EmptyState
            title="No staging environment yet"
            hint={`A staging environment is a copy of ${sourceKey || "production"} you can break safely. Create one, choose what it carries, and the platform copies it in the background.`}
            action={
              <button
                type="button"
                onClick={() => setWizard(true)}
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
              >
                <Plus className="size-3.5" aria-hidden />
                New staging environment
              </button>
            }
          />
        )
      ) : (
        <>
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
            <table className="w-full text-left text-[12.5px]">
              <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                <tr>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Environment
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Status
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Content
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Last clone
                  </th>
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    Open
                  </th>
                </tr>
              </thead>
              <tbody>
                {environments.map((environment) => (
                  <tr
                    key={environment.id}
                    data-env-row
                    data-env-type={environment.type}
                    data-env-status={environment.status}
                  >
                    <td className="px-4 py-3">
                      <a
                        href={`/environments/${environment.id}`}
                        data-env-open={environment.id}
                        className="font-medium underline-offset-2 hover:underline"
                      >
                        {environment.name}
                      </a>
                      <p className="font-mono text-[11.5px] text-muted">
                        {subtitle(environment)}
                      </p>
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={environment.status} />
                    </td>
                    <td className="px-4 py-3 text-muted">{contentSummary(environment)}</td>
                    <td className="px-4 py-3">
                      {environment.clone ? (
                        <CloneProgress job={environment.clone} />
                      ) : environment.cloned_at ? (
                        <span className="text-[11.5px] text-muted">
                          {new Date(environment.cloned_at).toLocaleString()}
                        </span>
                      ) : (
                        <span className="text-[11.5px] text-muted">never cloned</span>
                      )}
                    </td>
                    <td className="px-4 py-3 text-right">
                      {environment.reclonable ? (
                        <a
                          href={`/environments/${environment.id}`}
                          data-env-reclone={environment.id}
                          className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
                        >
                          Re-clone
                        </a>
                      ) : (
                        <span className="text-[11.5px] text-muted">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* The same rows as cards, with the same hooks. A hook present in only one rendering
              halves what a depth pass can drive, and the mobile measurement is then of a layout
              no interaction has reached. */}
          <ul className="flex flex-col gap-2 md:hidden">
            {environments.map((environment) => (
              <li key={environment.id}>
                <a
                  href={`/environments/${environment.id}`}
                  data-env-row
                  data-env-open={environment.id}
                  data-env-type={environment.type}
                  data-env-status={environment.status}
                  className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface px-4 py-3"
                >
                  <span className="flex items-center justify-between gap-2">
                    <span className="truncate font-medium">{environment.name}</span>
                    <StatusBadge status={environment.status} />
                  </span>
                  <span className="font-mono text-[11.5px] text-muted">{subtitle(environment)}</span>
                  <span className="text-[11.5px] text-muted">{contentSummary(environment)}</span>
                  {environment.clone ? <CloneProgress job={environment.clone} /> : null}
                </a>
              </li>
            ))}
          </ul>
        </>
      )}

      {wizard ? (
        <EnvironmentCreateWizard
          areas={areas}
          sourceKey={sourceKey}
          onClose={() => setWizard(false)}
          onCreated={() => {
            setWizard(false);
            setReloadToken((token) => token + 1);
          }}
        />
      ) : null}
    </div>
  );
}
