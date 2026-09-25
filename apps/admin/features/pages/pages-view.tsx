"use client";

/** Pages screen: the pages of the site the switcher is on, filterable by lifecycle state. */
import { useCallback, useEffect, useState } from "react";

import { RefreshCw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { ApiError, fetchPages } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import { pageTitle, type Page } from "@/lib/types";

const FILTERS = [
  { value: "", label: "All states" },
  { value: "draft", label: "Draft" },
  { value: "published", label: "Published" },
  { value: "archived", label: "Archived" },
] as const;

/** One row's second line: what is live and what is still waiting. */
function revisionNote(page: Page): string {
  const live = page.published ? `live v${page.published.revision_no}` : "not published";
  const pending = page.draft ? ` · draft v${page.draft.revision_no}` : "";
  return `${live}${pending}`;
}

/** The pages list. */
export function PagesView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [filter, setFilter] = useState("");
  const [pages, setPages] = useState<Page[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!selectedSite) {
      setPages(null);
      setError(null);
      return;
    }

    let cancelled = false;
    setPages(null);
    setError(null);
    fetchPages(selectedSite.id, filter || undefined)
      .then((rows) => {
        if (!cancelled) {
          setPages(rows);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "The pages could not be loaded.");
      });

    return () => {
      cancelled = true;
    };
  }, [selectedSite, filter, reloadToken]);

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The site list could not be loaded"
          hint="The pages screen follows the selected site, so it needs the sites first."
        />
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="No sites yet"
          hint="A site is where pages live. Create the first one through POST /api/v1/sites and it appears here."
        />
      </div>
    );
  }

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Pages</h2>
          <span className="text-[12px] text-muted">
            {selectedSite ? selectedSite.name : "Loading sites…"}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <label className="flex items-center gap-2">
            <span className="sr-only">Filter pages by state</span>
            <select
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] text-ink outline-none focus:border-accent"
            >
              {FILTERS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <button
            type="button"
            onClick={reload}
            aria-label="Reload pages"
            className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
          >
            <RefreshCw className="size-3.5" aria-hidden />
          </button>
        </div>
      </div>

      {error ? (
        <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : pages === null ? (
        <LoadingTable columns={4} />
      ) : pages.length === 0 ? (
        <EmptyState
          title={filter ? "No pages in this state" : "This site has no pages yet"}
          hint={
            filter
              ? "Clear the filter to see every page of the site."
              : "Create the first page through POST /api/v1/pages with this site's id."
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                <th scope="col" className="px-4 py-2.5">
                  Page
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Type
                </th>
                <th scope="col" className="px-4 py-2.5">
                  State
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Updated
                </th>
              </tr>
            </thead>
            <tbody>
              {pages.map((page) => (
                <tr key={page.id} className="border-t border-line transition hover:bg-canvas/60">
                  <td className="px-4 py-3.5">
                    <span className="flex min-w-0 flex-col leading-tight">
                      <span className="truncate font-medium">{pageTitle(page)}</span>
                      <span className="truncate text-[11.5px] text-muted">
                        /{page.slug} · {revisionNote(page)}
                      </span>
                    </span>
                  </td>
                  <td className="px-4 py-3.5 text-muted">{page.page_type}</td>
                  <td className="px-4 py-3.5">
                    <StatusBadge status={page.status} />
                  </td>
                  <td className="px-4 py-3.5 text-muted">{formatTimestamp(page.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
