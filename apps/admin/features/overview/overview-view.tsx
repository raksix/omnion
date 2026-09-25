"use client";

/** Overview screen: the workspace at a glance plus the account behind the session. */
import { useEffect, useState, type ReactNode } from "react";

import { Check, FileText, Globe } from "lucide-react";
import Link from "next/link";

import { StatusBadge } from "@/components/status-badge";
import { ApiError, fetchPages } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSession } from "@/lib/session";
import { useSites } from "@/lib/sites";
import type { Page } from "@/lib/types";

function StatCard({
  icon,
  label,
  value,
  hint,
}: {
  icon: ReactNode;
  label: string;
  value: string;
  hint: string;
}) {
  return (
    <article className="rounded-xl border border-line bg-surface p-4">
      <div className="flex items-center gap-2 text-muted">
        <span aria-hidden className="flex size-7 items-center justify-center rounded-lg bg-canvas">
          {icon}
        </span>
        <span className="text-[12px] font-medium tracking-wide uppercase">{label}</span>
      </div>
      <p className="mt-3 text-[26px] leading-none font-semibold">{value}</p>
      <p className="mt-1.5 text-[12px] text-muted">{hint}</p>
    </article>
  );
}

/** The panel's landing screen. */
export function OverviewView() {
  const { user } = useSession();
  const { sites, selectedSite, status: sitesStatus } = useSites();
  const [pages, setPages] = useState<Page[] | null>(null);
  const [pagesError, setPagesError] = useState<string | null>(null);

  useEffect(() => {
    if (!selectedSite) {
      setPages(null);
      setPagesError(null);
      return;
    }
    let cancelled = false;
    setPages(null);
    setPagesError(null);
    fetchPages(selectedSite.id)
      .then((rows) => {
        if (!cancelled) {
          setPages(rows);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setPagesError(cause instanceof ApiError ? cause.message : "The pages could not be loaded.");
      });
    return () => {
      cancelled = true;
    };
  }, [selectedSite]);

  const publishedCount = pages?.filter((page) => page.status === "published").length ?? null;

  return (
    <div className="flex flex-col gap-6">
      <section aria-label="Workspace totals" className="grid gap-4 sm:grid-cols-3">
        <StatCard
          icon={<Globe className="size-3.5" aria-hidden />}
          label="Sites"
          value={sitesStatus === "ready" ? String(sites.length) : "—"}
          hint="Sites the account may see"
        />
        <StatCard
          icon={<FileText className="size-3.5" aria-hidden />}
          label="Pages"
          value={pages ? String(pages.length) : "—"}
          hint={selectedSite ? `In ${selectedSite.name}` : "Select a site first"}
        />
        <StatCard
          icon={<Check className="size-3.5" aria-hidden />}
          label="Published"
          value={publishedCount !== null ? String(publishedCount) : "—"}
          hint="Pages visitors can see"
        />
      </section>

      {pagesError ? (
        <p className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong">
          {pagesError}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="rounded-xl border border-line bg-surface p-5">
          <h2 className="text-[13.5px] font-medium">Your account</h2>
          <dl className="mt-4 flex flex-col gap-3 text-[13px]">
            <div className="flex items-center justify-between gap-4">
              <dt className="text-muted">Email</dt>
              <dd className="truncate font-medium">{user?.email ?? "—"}</dd>
            </div>
            <div className="flex items-center justify-between gap-4">
              <dt className="text-muted">Display name</dt>
              <dd className="truncate">{user?.display_name ?? "—"}</dd>
            </div>
            <div className="flex items-center justify-between gap-4">
              <dt className="text-muted">Account status</dt>
              <dd>{user ? <StatusBadge status={user.status} /> : "—"}</dd>
            </div>
            <div className="flex items-center justify-between gap-4">
              <dt className="text-muted">Scope</dt>
              <dd className="truncate">
                {user?.organization_id ? "Organization account" : "Platform-level account"}
              </dd>
            </div>
            <div className="flex items-center justify-between gap-4">
              <dt className="text-muted">Member since</dt>
              <dd>{user ? formatTimestamp(user.created_at) : "—"}</dd>
            </div>
          </dl>
        </section>

        <section className="rounded-xl border border-line bg-surface p-5">
          <h2 className="text-[13.5px] font-medium">Where to go next</h2>
          <p className="mt-1.5 text-[12.5px] text-muted">
            Content and sites are the two halves of a CMS installation. Both screens read the
            same API the panel is built on.
          </p>
          <div className="mt-4 flex flex-col gap-2">
            <Link
              href="/pages"
              className="flex items-center justify-between gap-3 rounded-lg border border-line px-3.5 py-2.5 text-[13px] transition hover:bg-canvas"
            >
              <span className="flex items-center gap-2.5">
                <FileText className="size-4 text-muted" aria-hidden />
                Pages of the selected site
              </span>
              <span className="text-muted">{selectedSite ? selectedSite.name : "—"}</span>
            </Link>
            <Link
              href="/sites"
              className="flex items-center justify-between gap-3 rounded-lg border border-line px-3.5 py-2.5 text-[13px] transition hover:bg-canvas"
            >
              <span className="flex items-center gap-2.5">
                <Globe className="size-4 text-muted" aria-hidden />
                Sites in this installation
              </span>
              <span className="text-muted">{sitesStatus === "ready" ? sites.length : "—"}</span>
            </Link>
          </div>
        </section>
      </div>
    </div>
  );
}
