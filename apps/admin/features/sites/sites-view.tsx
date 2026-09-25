"use client";

/** Sites screen: every site the signed-in account may see. */
import { RefreshCw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";

/** The sites list. */
export function SitesView() {
  const { sites, selectedSite, status, error, reload } = useSites();

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Sites</h2>
          <span className="text-[12px] text-muted">
            {status === "ready" ? `${sites.length} total` : "Loading…"}
          </span>
        </div>
        <button
          type="button"
          onClick={reload}
          aria-label="Reload sites"
          className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
        </button>
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
      ) : status !== "ready" ? (
        <LoadingTable columns={4} />
      ) : sites.length === 0 ? (
        <EmptyState
          title="No sites yet"
          hint="Create the first site through POST /api/v1/sites; it shows up here and in the switcher."
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                <th scope="col" className="px-4 py-2.5">
                  Site
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Key
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Status
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Created
                </th>
              </tr>
            </thead>
            <tbody>
              {sites.map((site) => (
                <tr key={site.id} className="border-t border-line transition hover:bg-canvas/60">
                  <td className="px-4 py-3.5">
                    <span className="flex min-w-0 items-center gap-2">
                      <span className="truncate font-medium">{site.name}</span>
                      {site.id === selectedSite?.id ? (
                        <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[11px] font-medium text-accent-strong">
                          Selected
                        </span>
                      ) : null}
                    </span>
                  </td>
                  <td className="px-4 py-3.5 text-muted">{site.key}</td>
                  <td className="px-4 py-3.5">
                    <StatusBadge status={site.status} />
                  </td>
                  <td className="px-4 py-3.5 text-muted">{formatTimestamp(site.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
