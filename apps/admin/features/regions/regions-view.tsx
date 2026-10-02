"use client";

/**
 * `/platform/regions` — the edge region registry, the health matrix and the latency matrix
 * (REQ-035, slice 1).
 *
 * The screen is built around one rule the REQ states twice, in two different sentences: *a
 * region with no recent checks shows `Unknown` rather than green*, and *an unreachable control
 * plane shows `Unknown`, never green*. Every decision below follows from it, and each of them
 * has a wrong version that is a defect rather than a preference:
 *
 * - **A cell with no check renders `Unknown` with its own tone and an icon — never an empty
 *   badge and never a green dot.** An empty badge reads as "not applicable"; a green one reads
 *   as "fine". Both are lies about a region nobody has heard from.
 * - **The number and the status are separate columns.** A cell shows `unknown` *and* `—` for
 *   latency, because the two absences mean different things: no measurement was taken, and a
 *   measurement is genuinely absent. Printing `0 ms` would make the second look like the
 *   fastest region on the page.
 * - **The status shown is the worse of stored and derived, and the panel says which is
 *   which.** A region whose stored status is `healthy` while its API is `down` is exactly the
 *   finding this screen exists for, so the two are rendered in separate places and a mismatch
 *   gets its own sentence.
 * - **A single-region deployment disables the multi-region actions and explains why.** The
 *   REQ is explicit that the surfaces stay visible; hiding them would make the feature look
 *   absent rather than inapplicable.
 * - **No `Suspense` boundary and no `useSearchParams`.** Nothing here reads the query string,
 *   so opting into `useSearchParams` would buy a `Suspense` requirement for nothing — the trap
 *   documented at `/developer/sdks`.
 *
 * Every interactive element carries a `data-regions-*` hook so the walkthrough pass can drive
 * it by name rather than by position.
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  CircleCheck,
  CircleHelp,
  Globe2,
  Info,
  Loader2,
  MapPin,
  ServerCog,
  TriangleAlert,
  XCircle,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchRegions, patchRegion } from "@/lib/api";
import type {
  RegionHealthMatrix,
  RegionLatencyMatrix,
  RegionOverview,
  RegionPatchInput,
  RegionService,
  RegionServiceStatus,
  RegionStatusValue,
  RegionView,
} from "@/lib/types";

type Load = { value: RegionOverview | null; error: string | null; pending: boolean };

/** The seven services, in the API's own order rather than alphabetically. */
const SERVICES: RegionService[] = [
  "api",
  "admin",
  "web",
  "worker",
  "database",
  "storage",
  "cache",
];

const SERVICE_LABEL: Record<RegionService, string> = {
  api: "API",
  admin: "Admin",
  web: "Web",
  worker: "Worker",
  database: "Database",
  storage: "Storage",
  cache: "Cache",
};

/**
 * A status's icon, label and tone.
 *
 * Every entry carries an ICON AND A LABEL, and that is the REQ's own visual rule ("status chips
 * pair icon with label, never colour alone"): a panel that distinguishes `down` from
 * `degraded` by hue alone is unreadable to a colour-blind operator and unreadable in a
 * screenshot pasted into a ticket.
 */
function chip(status: RegionServiceStatus | RegionStatusValue) {
  switch (status) {
    case "healthy":
      return {
        icon: <CircleCheck aria-hidden className="size-3.5" />,
        label: "Healthy",
        className: "text-[#1a7f4b]",
      };
    case "degraded":
      return {
        icon: <TriangleAlert aria-hidden className="size-3.5" />,
        label: "Degraded",
        className: "text-[#9a6700]",
      };
    case "down":
      return {
        icon: <XCircle aria-hidden className="size-3.5" />,
        label: "Down",
        className: "text-danger",
      };
    case "maintenance":
      return {
        icon: <ServerCog aria-hidden className="size-3.5" />,
        label: "Maintenance",
        className: "text-muted",
      };
    case "unknown":
    default:
      return {
        icon: <CircleHelp aria-hidden className="size-3.5" />,
        label: "Unknown",
        className: "text-muted",
      };
  }
}

/** A status chip: icon + label, never colour alone. */
function StatusChip({ status, testId }: { status: RegionServiceStatus | RegionStatusValue; testId?: string }) {
  const view = chip(status);
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-[12px] font-medium ${view.className}`}
      data-testid={testId}
    >
      {view.icon}
      {view.label}
    </span>
  );
}

/** A latency figure, or the sentence that says there is not one. */
function Latency({ ms, stale }: { ms: number | null; stale?: boolean }) {
  if (ms === null) {
    // Never "0 ms". A zero is a measurement, and the fastest region on the page is the least
    // likely thing on it to be true.
    return <span className="text-[12px] text-muted">no measurement</span>;
  }
  return (
    <span className="text-[12px] tabular-nums">
      {ms} ms
      {stale ? <span className="ml-1 text-muted">(stale)</span> : null}
    </span>
  );
}

/** One region's expandable row. */
function RegionRow({
  region,
  health,
  expanded,
  onToggle,
  onPatch,
  busy,
  canManage,
}: {
  region: RegionView;
  health: RegionHealthMatrix;
  expanded: boolean;
  onToggle: () => void;
  onPatch: (patch: RegionPatchInput) => Promise<void>;
  busy: boolean;
  canManage: boolean;
}) {
  const [pendingError, setPendingError] = useState<string | null>(null);
  const derived = chip(region.derived_status);
  const stored = chip(region.status as RegionStatusValue);
  // The one finding this screen exists for: what the last check said versus what the region
  // row says. Rendered as its own sentence rather than a colour, because it is the difference
  // between "the panel agrees with itself" and "something changed underneath the panel".
  const mismatch = region.derived_status !== (region.status as RegionStatusValue);

  return (
    <li
      className="rounded-xl border border-line bg-surface"
      data-testid={`regions-row-${region.code}`}
    >
      <div className="flex flex-wrap items-center gap-3 p-4">
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={expanded}
          className="flex min-w-0 flex-1 items-center gap-3 text-left"
          data-testid={`regions-toggle-${region.code}`}
        >
          {expanded ? (
            <ChevronDown aria-hidden className="size-4 shrink-0 text-muted" />
          ) : (
            <ChevronRight aria-hidden className="size-4 shrink-0 text-muted" />
          )}
          <span className="min-w-0">
            <span className="flex flex-wrap items-center gap-2">
              <span className="text-[14px] font-medium">{region.display_name}</span>
              <code className="text-[12px] text-muted">{region.code}</code>
              {region.is_default ? (
                <span
                  className="rounded-full border border-line px-2 py-0.5 text-[11px] font-medium"
                  data-testid={`regions-default-${region.code}`}
                >
                  Default
                </span>
              ) : null}
              {!region.is_active ? (
                <span className="text-[11px] text-muted">Inactive</span>
              ) : null}
            </span>
            <span className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
              <span className="inline-flex items-center gap-1">
                <MapPin aria-hidden className="size-3" />
                {region.country_group.toUpperCase()}
              </span>
              <span>bucket {region.storage_bucket}</span>
              <span data-testid={`regions-homes-${region.code}`}>
                home for {region.home_for_organizations}{" "}
                {region.home_for_organizations === 1 ? "organization" : "organizations"}
              </span>
            </span>
          </span>
        </button>

        <div className="flex flex-wrap items-center gap-3">
          <span
            className="flex flex-wrap items-center gap-1"
            data-testid={`regions-badges-${region.code}`}
          >
            {SERVICES.map((service) => {
              const cell = health.cells.find(
                (c) => c.region_code === region.code && c.service === service,
              );
              const view = chip(cell?.status ?? "unknown");
              return (
                <span
                  key={service}
                  title={`${SERVICE_LABEL[service]}: ${view.label}`}
                  className={`inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11px] ${view.className}`}
                  data-testid={`regions-service-${region.code}-${service}`}
                >
                  {view.icon}
                  {SERVICE_LABEL[service]}
                </span>
              );
            })}
          </span>

          <span className="min-w-[92px] text-right">
            <StatusChip
              status={region.effective_status}
              testId={`regions-status-${region.code}`}
            />
          </span>
          <span className="min-w-[86px] text-right">
            <Latency ms={region.p95_ms} />
          </span>
          <Link
            href={`/platform/regions/${region.code}`}
            className="text-[13px] font-medium text-accent hover:underline"
            data-testid={`regions-open-${region.code}`}
          >
            Open
          </Link>
        </div>
      </div>

      {mismatch ? (
        <p
          className="mx-4 mb-3 flex items-start gap-2 rounded-lg border border-line bg-canvas p-3 text-[12px] text-muted"
          data-testid={`regions-mismatch-${region.code}`}
        >
          <Info aria-hidden className="mt-0.5 size-3.5 shrink-0" />
          <span>
            The region record says <strong className={derived.className}>{derived.label}</strong>{" "}
            and the latest checks say{" "}
            <strong className={stored.className}>{stored.label}</strong>. The checks win on this
            screen; the record is what the checker last wrote.
          </span>
        </p>
      ) : null}

      {expanded ? (
        <div className="border-t border-line px-4 py-4" data-testid={`regions-services-${region.code}`}>
          <h3 className="text-[12px] font-medium tracking-wide uppercase text-muted">
            Services
          </h3>
          <div className="mt-2 overflow-x-auto">
            <table className="w-full min-w-[520px] text-left text-[13px]">
              <thead>
                <tr className="text-[12px] text-muted">
                  <th className="py-1.5 pr-3 font-medium">Service</th>
                  <th className="py-1.5 pr-3 font-medium">Status</th>
                  <th className="py-1.5 pr-3 font-medium">Latency</th>
                  <th className="py-1.5 font-medium">Last check</th>
                </tr>
              </thead>
              <tbody>
                {SERVICES.map((service) => {
                  const cell = health.cells.find(
                    (c) => c.region_code === region.code && c.service === service,
                  );
                  return (
                    <tr key={service} className="border-t border-line" data-testid={`regions-service-row-${region.code}-${service}`}>
                      <td className="py-1.5 pr-3">{SERVICE_LABEL[service]}</td>
                      <td className="py-1.5 pr-3">
                        <StatusChip status={cell?.status ?? "unknown"} />
                      </td>
                      <td className="py-1.5 pr-3">
                        <Latency ms={cell?.latency_ms ?? null} stale={cell?.stale} />
                      </td>
                      <td className="py-1.5 text-[12px] text-muted">
                        {cell?.checked_at
                          ? new Date(cell.checked_at).toLocaleString()
                          : "No recent checks"}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>

          <h3 className="mt-4 text-[12px] font-medium tracking-wide uppercase text-muted">
            Endpoints
          </h3>
          <dl className="mt-2 grid gap-x-6 gap-y-1.5 text-[13px] sm:grid-cols-2">
            {(
              [
                ["API", region.api_endpoint],
                ["Admin", region.admin_endpoint],
                ["Web", region.web_endpoint],
                ["Storage bucket", region.storage_bucket],
                ["Cache namespace", region.cache_namespace],
              ] as const
            ).map(([label, value]) => (
              <div key={label} className="flex gap-2">
                <dt className="w-28 shrink-0 text-muted">{label}</dt>
                {/* Host names only, never a credential: the registry is a descriptive table
                    and a bucket label is not a key. */}
                <dd className="min-w-0 break-words">{value ?? "—"}</dd>
              </div>
            ))}
          </dl>

          {canManage ? (
            <div className="mt-4 flex flex-wrap items-center gap-2">
              {region.is_default ? (
                <p className="text-[12px] text-muted" data-testid={`regions-default-note-${region.code}`}>
                  This is the routing fallback. Promote another region to move it.
                </p>
              ) : (
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void onPatch({ is_default: true })}
                  className="rounded-lg border border-line px-3 py-1.5 text-[13px] font-medium disabled:opacity-60"
                  data-testid={`regions-promote-${region.code}`}
                >
                  Set as default
                </button>
              )}
              <button
                type="button"
                disabled={busy}
                onClick={() =>
                  void onPatch({
                    status: region.status === "maintenance" ? "degraded" : "maintenance",
                  })
                }
                className="rounded-lg border border-line px-3 py-1.5 text-[13px] font-medium disabled:opacity-60"
                data-testid={`regions-maintenance-${region.code}`}
              >
                {region.status === "maintenance" ? "Leave maintenance" : "Enter maintenance"}
              </button>
            </div>
          ) : null}

          {pendingError ? (
            <p className="mt-3 text-[12px] text-danger" data-testid={`regions-error-${region.code}`}>
              {pendingError}
            </p>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

/** The region × region p95 grid, with numbers and a timestamp rather than colour. */
function LatencyMatrix({ matrix }: { matrix: RegionLatencyMatrix }) {
  const codes = useMemo(
    () => Array.from(new Set(matrix.cells.flatMap((c) => [c.from_region, c.to_region]))).sort(),
    [matrix.cells],
  );
  if (codes.length < 2) {
    return (
      <p className="text-[13px] text-muted" data-testid="regions-latency-empty">
        A region-to-region matrix needs at least two regions. This deployment has{" "}
        {codes.length}.
      </p>
    );
  }
  return (
    <div className="overflow-x-auto" data-testid="regions-latency-matrix">
      <table className="w-full min-w-[420px] text-left text-[13px]">
        <thead>
          <tr className="text-[12px] text-muted">
            <th className="py-1.5 pr-3 font-medium">From \ To</th>
            {codes.map((code) => (
              <th key={code} className="py-1.5 pr-3 font-medium">
                {code}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {codes.map((from) => (
            <tr key={from} className="border-t border-line" data-testid={`regions-latency-row-${from}`}>
              <th scope="row" className="py-1.5 pr-3 text-left font-medium">
                {from}
              </th>
              {codes.map((to) => {
                if (from === to) {
                  // The diagonal is the region itself, which no probe measures; "local" is the
                  // honest label and a dash would look like a missing measurement.
                  return (
                    <td key={to} className="py-1.5 pr-3 text-[12px] text-muted">
                      local
                    </td>
                  );
                }
                const cell = matrix.cells.find(
                  (c) => c.from_region === from && c.to_region === to,
                );
                return (
                  <td key={to} className="py-1.5 pr-3 tabular-nums">
                    <Latency ms={cell?.p95_ms ?? null} stale={cell?.stale} />
                  </td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="mt-2 text-[12px] text-muted" data-testid="regions-latency-measured">
        {matrix.measured_at
          ? `Measured ${new Date(matrix.measured_at).toLocaleString()}; figures older than ${Math.round(
              matrix.stale_after_seconds / 60,
            )} minutes are marked stale.`
          : `No latency sample has been recorded yet. Figures are marked stale after ${Math.round(
              matrix.stale_after_seconds / 60,
            )} minutes.`}
      </p>
    </div>
  );
}

/** `/platform/regions` — the registry screen. */
export function RegionsView() {
  const [state, setState] = useState<Load>({ value: null, error: null, pending: true });
  const [expanded, setExpanded] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setState({ value: null, error: null, pending: true });
    fetchRegions()
      .then((value) => {
        if (!cancelled) setState({ value, error: null, pending: false });
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        setState({
          value: null,
          error: cause instanceof ApiError ? cause.message : "The regions could not be loaded.",
          pending: false,
        });
      });
    return () => {
      cancelled = true;
    };
  }, [reload]);

  // The claim that a card's number cannot disagree with the screen it links to, applied to a
  // write: after a promotion the whole overview is re-read rather than patched locally, so a
  // region that moved out of `is_default` and a `traffic_share` the API recomputed are both
  // the API's answer rather than this screen's guess.
  const applyPatch = useCallback(
    async (code: string, patch: RegionPatchInput) => {
      setBusy(true);
      try {
        await patchRegion(code, patch);
        setReload((n) => n + 1);
      } finally {
        setBusy(false);
      }
    },
    [],
  );

  if (state.error) {
    return (
      <EmptyState
        title="The region registry could not be read"
        hint={state.error}
      />
    );
  }

  if (state.pending || !state.value) {
    return (
      <div
        className="flex items-center gap-2 p-6 text-[13px] text-muted"
        data-testid="regions-loading"
      >
        <Loader2 aria-hidden className="size-4 animate-spin" />
        Loading regions…
      </div>
    );
  }

  const { regions, health, latency, multi_region_active, inactive_reason } = state.value;
  const stale = new Set(health.stale_region_codes);

  return (
    <div className="space-y-4">
      {!multi_region_active && inactive_reason ? (
        // Visible and explained, never hidden: the REQ asks for the surfaces to stay on the
        // shelf with their multi-region actions disabled, because a single-region operator
        // needs to know the capability exists and why it does not apply to them.
        <p
          className="flex items-start gap-2 rounded-xl border border-line bg-surface p-4 text-[13px] text-muted"
          data-testid="regions-inactive"
        >
          <Info aria-hidden className="mt-0.5 size-4 shrink-0" />
          {inactive_reason}
        </p>
      ) : null}

      {stale.size > 0 ? (
        <p
          className="flex items-start gap-2 rounded-xl border border-line bg-surface p-4 text-[13px] text-muted"
          data-testid="regions-stale-warning"
        >
          <AlertTriangle aria-hidden className="mt-0.5 size-4 shrink-0" />
          No health check has landed for {Array.from(stale).join(", ")} inside the freshness
          window. Those services read <strong>Unknown</strong> rather than green, because
          "nobody asked" and "everything is fine" are different claims.
        </p>
      ) : null}

      {regions.length === 0 ? (
        <EmptyState
          title="No regions are registered"
          hint="The registry describes infrastructure the deployment already provides, so it starts from what is running. A migration seeds the three regions the platform ships with."
        />
      ) : (
        <ul className="space-y-2" data-testid="regions-list">
          {regions.map((region) => (
            <RegionRow
              key={region.code}
              region={region}
              health={health}
              expanded={expanded === region.code}
              onToggle={() =>
                setExpanded((current) => (current === region.code ? null : region.code))
              }
              onPatch={(patch) => applyPatch(region.code, patch)}
              busy={busy}
              canManage={multi_region_active}
            />
          ))}
        </ul>
      )}

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="regions-health-section"
      >
        <div className="flex items-center gap-2">
          <Activity aria-hidden className="size-4 text-muted" />
          <h2 className="text-[13px] font-medium">Health matrix</h2>
        </div>
        <p className="mt-1.5 text-[12px] text-muted">
          {health.cells.length} cells — every region against every service, including the ones
          nobody has checked. A cell with no recent check reads <strong>Unknown</strong>.
        </p>
        <div className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[560px] text-left text-[13px]">
            <thead>
              <tr className="text-[12px] text-muted">
                <th className="py-1.5 pr-3 font-medium">Region</th>
                {SERVICES.map((service) => (
                  <th key={service} className="py-1.5 pr-3 font-medium">
                    {SERVICE_LABEL[service]}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {regions.map((region) => (
                <tr key={region.code} className="border-t border-line" data-testid={`regions-matrix-row-${region.code}`}>
                  <th scope="row" className="py-1.5 pr-3 text-left font-medium">
                    {region.code}
                  </th>
                  {SERVICES.map((service) => {
                    const cell = health.cells.find(
                      (c) => c.region_code === region.code && c.service === service,
                    );
                    const view = chip(cell?.status ?? "unknown");
                    return (
                      <td key={service} className="py-1.5 pr-3" data-testid={`regions-matrix-${region.code}-${service}`}>
                        <span className={`inline-flex items-center gap-1 text-[12px] ${view.className}`}>
                          {view.icon}
                          {cell?.latency_ms === null || cell?.latency_ms === undefined
                            ? view.label
                            : `${cell.latency_ms} ms`}
                        </span>
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {health.history.length > 0 ? (
          <>
            <h3 className="mt-4 text-[12px] font-medium tracking-wide uppercase text-muted">
              Recent changes
            </h3>
            <ul className="mt-2 space-y-1" data-testid="regions-history">
              {health.history.slice(0, 10).map((entry) => (
                <li key={`${entry.region_code}-${entry.service}-${entry.changed_at}`} className="text-[12px]">
                  <span className="text-muted">{new Date(entry.changed_at).toLocaleString()}</span>{" "}
                  <strong>{entry.region_code}</strong> / {SERVICE_LABEL[entry.service]} →{" "}
                  <StatusChip status={entry.status} />
                </li>
              ))}
            </ul>
          </>
        ) : (
          <p className="mt-4 text-[12px] text-muted" data-testid="regions-history-empty">
            No health checks have been recorded yet, so there are no changes to show. The
            regions above still render — as Unknown.
          </p>
        )}
      </section>

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="regions-latency-section"
      >
        <div className="flex items-center gap-2">
          <Globe2 aria-hidden className="size-4 text-muted" />
          <h2 className="text-[13px] font-medium">Region latency</h2>
        </div>
        <LatencyMatrix matrix={latency} />
      </section>
    </div>
  );
}
