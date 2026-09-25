"use client";

/** The site switcher: which site the panel is looking at. */
import { Globe } from "lucide-react";

import { useSites } from "@/lib/sites";

/** A compact select over the sites the signed-in account may see. */
export function SiteSwitcher() {
  const { sites, selectedSite, status, selectSite } = useSites();

  if (status === "loading" || status === "idle") {
    return <span aria-hidden className="h-9 w-36 animate-pulse rounded-lg bg-quiet-soft" />;
  }

  if (status === "error" || sites.length === 0) {
    return (
      <span className="hidden items-center gap-2 rounded-lg border border-dashed border-line px-2.5 py-2 text-[12px] text-muted sm:flex">
        <Globe className="size-3.5" aria-hidden />
        No sites yet
      </span>
    );
  }

  return (
    <label className="flex items-center gap-2 rounded-lg border border-line bg-surface px-2.5 py-1.5">
      <Globe className="size-3.5 text-muted" aria-hidden />
      <span className="sr-only">Current site</span>
      <select
        value={selectedSite?.id ?? ""}
        onChange={(event) => selectSite(event.target.value)}
        className="max-w-44 bg-transparent text-[12.5px] font-medium text-ink outline-none"
      >
        {sites.map((site) => (
          <option key={site.id} value={site.id}>
            {site.name}
          </option>
        ))}
      </select>
    </label>
  );
}
