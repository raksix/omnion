"use client";

/**
 * `/cdn` — the CDN overview (REQ-011, slice 1's front door).
 *
 * The overview's whole job is to answer "is this site's cache doing anything, and is it
 * healthy?" in one screen, so the claims here are narrow and all of them are about *honesty*
 * rather than decoration:
 *
 * - **A site with no rule says so plainly.** Without a rule every public response is
 *   `private, no-store`, which is a safe default and a slow one. An overview that showed
 *   four zeros and no explanation reads as "the CDN is broken" rather than "nothing is
 *   cached yet", and the fix for each is completely different.
 * - **The numbers come from the API, and the ones that do not exist yet are not rendered as
 *   zero.** Purge counts belong to slice 2's tables, which do not exist. Inventing a `0` for
 *   a counter that has never been written is a small lie that makes the next real zero
 *   indistinguishable from it, so the counters that need slice 2 are simply absent and the
 *   screen says which slice they are waiting on.
 * - **The provider line names what is configured and what is not.** `origin` is a real
 *   answer — it means the rule engine runs and no external cache is contacted — and it is
 *   not the same answer as "nothing is configured".
 */
import { useCallback, useEffect, useState } from "react";

import { Activity, ArrowRight, Gauge, ListOrdered, Settings2, TriangleAlert } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchCdnRules, fetchCdnSettings, fetchCdnStatus } from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { CdnPurge, CdnSettings, CdnStatus } from "@/lib/types";

/** One number on a card. */
function Stat({
  label,
  value,
  hint,
}: {
  label: string;
  value: string;
  hint: string;
}) {
  return (
    <div className="flex flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3.5">
      <span className="flex items-center gap-1.5 text-[11px] font-medium tracking-wide text-muted uppercase">
        {label}
      </span>
      <span className="text-[20px] leading-tight font-medium">{value}</span>
      <span className="text-[11.5px] text-muted">{hint}</span>
    </div>
  );
}

/** The CDN overview of the selected site. */
export function CdnOverviewView() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [settings, setSettings] = useState<CdnSettings | null>(null);
  const [ruleCount, setRuleCount] = useState<number | null>(null);
  const [liveRules, setLiveRules] = useState<number | null>(null);
  const [unreadable, setUnreadable] = useState(0);
  // Slice 2's numbers. `null` means "not read yet" and is rendered as an ellipsis, which
  // is the same treatment the rule counts get: a dash or a zero here would be a claim
  // about a counter nothing has written yet.
  const [status, setStatus] = useState<CdnStatus | null>(null);
  const [recent, setRecent] = useState<CdnPurge[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    // Both reads are needed and neither implies the other: the settings row says which
    // adapter is running (the installation default for a site with no row of its own),
    // and the rule list says whether anything is being cached through it. Reading only one
    // is how an overview ends up describing a provider that caches nothing.
    Promise.all([
      fetchCdnSettings(siteId),
      fetchCdnRules(siteId ?? ""),
      // The status call is a third read rather than a second because the purge counters
      // and the recent list come from the same query: fetching them separately would let
      // the table show a purge the counter has not counted, which reads as a bug and is.
      siteId === null ? Promise.resolve(null) : fetchCdnStatus(siteId),
    ])
      .then(([row, rules, cdnStatus]) => {
        if (cancelled) {
          return;
        }
        setSettings(row);
        setRuleCount(rules.rules.length);
        setLiveRules(rules.rules.filter((rule) => rule.enabled).length);
        setUnreadable(rules.unreadable.length);
        if (cdnStatus) {
          setStatus(cdnStatus);
          setRecent(cdnStatus.recent);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "The CDN state could not be read.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  if (!siteId) {
    return (
      <EmptyState
        title="No site selected"
        hint="The CDN is configured per site. Pick one in the switcher above."
      />
    );
  }

  const noRules = ruleCount === 0;

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <div className="flex flex-col items-start gap-3 rounded-xl border border-accent/30 bg-accent-soft px-4 py-3">
          <p role="alert" className="text-[12.5px] text-accent-strong">
            {error}
          </p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Stat
          label="Provider"
          value={settings === null ? "…" : settings.provider}
          hint={
            settings === null
              ? "Reading the settings row"
              : settings.provider === "origin"
                ? "No external cache — the rule engine still decides the headers"
                : `Invalidation is sent to ${settings.endpoint_url ?? "the configured endpoint"}`
          }
        />
        <Stat
          label="Cache rules"
          value={ruleCount === null ? "…" : String(ruleCount)}
          hint={
            ruleCount === null
              ? "Loading"
              : liveRules === null
                ? ""
                : `${liveRules} live, ${ruleCount - liveRules} off`
          }
        />
        <Stat
          label="Unreadable rules"
          value={unreadable === 0 ? "none" : String(unreadable)}
          hint={
            unreadable === 0
              ? "Every stored pattern still compiles"
              : "These cannot match anything until they are fixed or deleted"
          }
        />
        <Stat
          label="Purges queued"
          value={status === null ? "…" : String(status.queue_depth)}
          hint={
            status === null
              ? "Loading"
              : status.queue_depth === 0
                ? "Nothing is waiting to be invalidated"
                : `${status.queue_depth} item${status.queue_depth === 1 ? "" : "s"} across ${status.open_purges} open purge${status.open_purges === 1 ? "" : "s"}`
          }
        />
        <Stat
          label="Purges · 24 h"
          value={status === null ? "…" : String(status.purges_24h)}
          hint={
            status === null
              ? "Loading"
              : status.purges_24h === 0
                ? "No invalidation has been requested today"
                : `${status.succeeded_24h} succeeded · ${status.failure_rate.toFixed(0)}% did not fully succeed`
          }
        />
      </div>

      {unreadable > 0 ? (
        <p className="flex items-start gap-2 rounded-xl border border-caution/30 bg-caution-soft px-4 py-3 text-[12.5px] text-caution">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {`${unreadable} rule${unreadable === 1 ? "" : "s"} cannot be matched. They still appear in the table, with the reason, because a rule that quietly stopped working is the failure you find days later from a stale page.`}
          </span>
        </p>
      ) : null}

      {noRules ? (
        <EmptyState
          title="Nothing is being cached yet"
          hint="Without a rule every public response is served as private, no-store. That is a safe default, not a fast one: a first rule for /blog/** is usually the whole difference."
          action={
            <Link
              href="/cdn/rules"
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              <ListOrdered className="size-3.5" aria-hidden />
              Write the first rule
            </Link>
          }
        />
      ) : null}

      {recent.length > 0 ? (
        <section
          data-cdn-overview-recent
          className="flex flex-col gap-2 rounded-xl border border-line bg-surface"
        >
          <header className="flex items-center justify-between gap-3 border-b border-line px-4 py-3">
            <h2 className="text-[13px] font-medium">Recent purges</h2>
            <Link
              href="/cdn/purges"
              data-cdn-overview-all-purges
              className="inline-flex items-center gap-1 text-[12px] text-muted transition hover:text-ink"
            >
              All history
              <ArrowRight className="size-3" aria-hidden />
            </Link>
          </header>
          <ul className="divide-y divide-line">
            {recent.slice(0, 8).map((purge) => (
              <li
                key={purge.id}
                data-cdn-recent-row
                data-cdn-purge-status={purge.status}
                className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5"
              >
                <span className="w-40 shrink-0 truncate font-mono text-[11.5px] text-muted">
                  {new Date(purge.requested_at).toLocaleString()}
                </span>
                <span className="w-12 shrink-0 text-[12px] text-muted">{purge.kind}</span>
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]">
                  {purge.targets.length === 1 ? purge.targets[0] : `${purge.targets.length} targets`}
                </span>
                <span className="shrink-0 text-[12px] text-muted">
                  {purge.failed_count > 0 ? `${purge.failed_count} failed` : `${purge.item_count} ok`}
                </span>
                <span
                  data-cdn-purge-badge
                  className="shrink-0 rounded-full border border-line px-2 py-0.5 text-[11px]"
                >
                  {purge.status}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2">
        <Link
          href="/cdn/rules"
          data-cdn-overview-rules
          className="flex items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-4 transition hover:border-accent/40"
        >
          <span className="flex min-w-0 flex-col gap-1">
            <span className="flex items-center gap-1.5 text-[13px] font-medium">
              <Gauge className="size-3.5 text-muted" aria-hidden />
              Cache rules
            </span>
            <span className="text-[12px] text-muted">
              Precedence, TTLs, what is keyed and what bypasses
            </span>
          </span>
          <ArrowRight className="size-3.5 shrink-0 text-muted" aria-hidden />
        </Link>
        <Link
          href="/cdn/settings"
          data-cdn-overview-settings
          className="flex items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-4 transition hover:border-accent/40"
        >
          <span className="flex min-w-0 flex-col gap-1">
            <span className="flex items-center gap-1.5 text-[13px] font-medium">
              <Settings2 className="size-3.5 text-muted" aria-hidden />
              Provider &amp; triggers
            </span>
            <span className="text-[12px] text-muted">
              Which adapter invalidates, and which events queue a purge on their own
            </span>
          </span>
          <ArrowRight className="size-3.5 shrink-0 text-muted" aria-hidden />
        </Link>
      </div>

      <div className="flex items-start gap-2 rounded-xl border border-line bg-canvas/60 px-4 py-3 text-[12px] text-muted">
        <Activity className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        <span>
          The purge console, the history drawer and the automatic invalidation that follows a
          publish all arrive with slice 2. Until then a change to a page is served the moment
          the request reaches the origin — which is correct, just not fast.
        </span>
      </div>

      {ruleCount === null ? <LoadingTable columns={4} /> : null}
    </div>
  );
}
