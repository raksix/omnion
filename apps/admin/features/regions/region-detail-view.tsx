"use client";

/**
 * `/platform/regions/{code}` — one region (REQ-035, slice 1).
 *
 * The detail screen answers three questions the list cannot: *what are this region's
 * endpoints* (host names only, never a credential — the registry is descriptive and a bucket
 * label is not a key), *when did each service last answer*, and *how far is it from the
 * others*. The fourth thing it shows is a control — the rename field and the `Set as default`
 * action — because a detail screen with no editable field is a read-only view, and the REQ
 * asks for a registry an operator can correct.
 *
 * Three decisions are the same ones the list screen makes, and repeating them here is
 * deliberate rather than an oversight:
 *
 * - **A service with no recent check says `Unknown` and `No recent checks`.** Never a green
 *   chip, never `0 ms`, and never a blank — a blank reads as "not applicable" and a region
 *   whose control plane is unreachable is emphatically applicable.
 * - **The rename form validates before it sends, and the server's own message is shown.**
 *   A panel that only learns "invalid" cannot put the sentence under the input, so the API
 *   attaches `details.field` and this screen reads it.
 * - **A failed save leaves the field showing what the operator typed.** Re-rendering from the
 *   API's copy on failure would silently discard their input, which is the single most
 *   annoying thing a settings form can do.
 */

import { useCallback, useEffect, useState } from "react";
import {
  ArrowLeft,
  CircleCheck,
  CircleHelp,
  Globe2,
  Loader2,
  ServerCog,
  TriangleAlert,
  XCircle,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchRegion, patchRegion } from "@/lib/api";
import type { RegionDetail, RegionService, RegionServiceStatus } from "@/lib/types";

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

function chip(status: RegionServiceStatus) {
  switch (status) {
    case "healthy":
      return { icon: <CircleCheck aria-hidden className="size-3.5" />, label: "Healthy", className: "text-[#1a7f4b]" };
    case "degraded":
      return { icon: <TriangleAlert aria-hidden className="size-3.5" />, label: "Degraded", className: "text-[#9a6700]" };
    case "down":
      return { icon: <XCircle aria-hidden className="size-3.5" />, label: "Down", className: "text-danger" };
    default:
      return { icon: <CircleHelp aria-hidden className="size-3.5" />, label: "Unknown", className: "text-muted" };
  }
}

/** One region's detail screen. */
export function RegionDetailView({ code }: { code: string }) {
  const [detail, setDetail] = useState<RegionDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(true);
  const [name, setName] = useState<string>("");
  const [nameError, setNameError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setPending(true);
    setError(null);
    fetchRegion(code)
      .then((value) => {
        if (cancelled) return;
        setDetail(value);
        setName(value.region.display_name);
        setPending(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) return;
        // A 404 names the code, and the panel repeats it: an operator who mistyped a region
        // needs to know WHICH code was not found, not "this screen could not load".
        setError(cause instanceof ApiError ? cause.message : "The region could not be loaded.");
        setPending(false);
      });
    return () => {
      cancelled = true;
    };
  }, [code]);

  const saveName = useCallback(async () => {
    setNameError(null);
    setSavedAt(null);
    // The length rule is checked HERE as well as on the server. The server is the authority
    // and its message is what the panel shows on failure, but a form that lets you type one
    // character and press Save to learn the answer has already made the operator do work the
    // panel could have refused in place.
    if (name.trim().length < 2 || name.trim().length > 80) {
      setNameError("A region name must be between 2 and 80 characters.");
      return;
    }
    setSaving(true);
    try {
      await patchRegion(code, { display_name: name.trim() });
      // The detail is RE-READ rather than patched locally, and the reason is a type the
      // first version of this got wrong: `PATCH /regions/{code}` answers a bare `Region`,
      // which has no `derived_status`, no `services` and no `p95_ms` — those come from the
      // health matrix. Splicing the patch answer into the detail therefore produces an object
      // that satisfies half the type and lies about the other half, and `tsc` is right to
      // refuse it. A rename is rare and the read is one round trip, so the screen shows the
      // API's own answer rather than a locally merged guess.
      setDetail(await fetchRegion(code));
      setSavedAt(new Date().toLocaleTimeString());
    } catch (cause: unknown) {
      // The operator's text is NOT reset on failure. Re-rendering from the server's copy here
      // would discard exactly what they were trying to save.
      setNameError(cause instanceof ApiError ? cause.message : "The region could not be renamed.");
    } finally {
      setSaving(false);
    }
  }, [code, name]);

  const promote = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      await patchRegion(code, { is_default: true });
      const fresh = await fetchRegion(code);
      setDetail(fresh);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The default could not be moved.");
    } finally {
      setSaving(false);
    }
  }, [code]);

  if (pending) {
    return (
      <div
        className="flex items-center gap-2 p-6 text-[13px] text-muted"
        data-testid="region-detail-loading"
      >
        <Loader2 aria-hidden className="size-4 animate-spin" />
        Loading {code}…
      </div>
    );
  }

  if (!detail) {
    return <EmptyState title={`No region named ${code}`} hint={error ?? undefined} />;
  }

  const region = detail.region;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <Link
          href="/platform/regions"
          className="inline-flex items-center gap-1.5 text-[13px] font-medium text-accent hover:underline"
          data-testid="region-detail-back"
        >
          <ArrowLeft aria-hidden className="size-3.5" />
          All regions
        </Link>
        <h2 className="text-[15px] font-medium" data-testid="region-detail-title">
          {region.display_name}{" "}
          <code className="text-[13px] text-muted">{region.code}</code>
        </h2>
        {region.is_default ? (
          <span
            className="rounded-full border border-line px-2 py-0.5 text-[11px] font-medium"
            data-testid="region-detail-default"
          >
            Default
          </span>
        ) : null}
      </div>

      {error ? (
        <p className="text-[13px] text-danger" data-testid="region-detail-error">
          {error}
        </p>
      ) : null}

      {!detail.multi_region_active && detail.inactive_reason ? (
        <p
          className="rounded-xl border border-line bg-surface p-3 text-[13px] text-muted"
          data-testid="region-detail-inactive"
        >
          {detail.inactive_reason}
        </p>
      ) : null}

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="region-detail-services"
      >
        <h3 className="text-[13px] font-medium">Services</h3>
        <div className="mt-3 overflow-x-auto">
          <table className="w-full min-w-[460px] text-left text-[13px]">
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
                const cell = region.services.find((c) => c.service === service);
                const view = chip(cell?.status ?? "unknown");
                return (
                  <tr key={service} className="border-t border-line" data-testid={`region-service-${service}`}>
                    <td className="py-1.5 pr-3">{SERVICE_LABEL[service]}</td>
                    <td className="py-1.5 pr-3">
                      {/* Icon AND label, always. A status conveyed by colour alone is
                          invisible to a colour-blind operator and unreadable in a pasted
                          screenshot. */}
                      <span className={`inline-flex items-center gap-1.5 text-[12px] font-medium ${view.className}`}>
                        {view.icon}
                        {view.label}
                      </span>
                    </td>
                    <td className="py-1.5 pr-3 tabular-nums">
                      {cell?.latency_ms === null || cell?.latency_ms === undefined ? (
                        <span className="text-[12px] text-muted">no measurement</span>
                      ) : (
                        <span className="text-[12px]">
                          {cell.latency_ms} ms
                          {cell.stale ? <span className="ml-1 text-muted">(stale)</span> : null}
                        </span>
                      )}
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
      </section>

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="region-detail-config"
      >
        <h3 className="text-[13px] font-medium">Configuration</h3>
        <dl className="mt-3 grid gap-x-6 gap-y-2 text-[13px] sm:grid-cols-2">
          {(
            [
              ["Country group", region.country_group.toUpperCase()],
              ["API endpoint", region.api_endpoint],
              ["Admin endpoint", region.admin_endpoint ?? "Not served here"],
              ["Web endpoint", region.web_endpoint ?? "Not served here"],
              ["Storage bucket", region.storage_bucket],
              ["Cache namespace", region.cache_namespace],
              ["Traffic share", region.traffic_share === null ? "—" : `${region.traffic_share}%`],
              [
                "Home for",
                `${region.home_for_organizations} ${region.home_for_organizations === 1 ? "organization" : "organizations"}`,
              ],
            ] as const
          ).map(([label, value]) => (
            <div key={label} className="flex gap-2">
              <dt className="w-32 shrink-0 text-muted">{label}</dt>
              {/* Host names and labels only. Nothing on this screen is a credential, and the
                  registry deliberately has nowhere to put one. */}
              <dd className="min-w-0 break-words">{value}</dd>
            </div>
          ))}
        </dl>
      </section>

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="region-detail-settings"
      >
        <h3 className="text-[13px] font-medium">Settings</h3>
        <div className="mt-3 max-w-md space-y-3">
          <div>
            <label
              htmlFor="region-name"
              className="block text-[12px] font-medium text-muted"
            >
              Display name
            </label>
            <input
              id="region-name"
              value={name}
              onChange={(event) => {
                setName(event.target.value);
                setNameError(null);
                setSavedAt(null);
              }}
              // `maxLength` matches the server's bound so the field cannot produce a value the
              // API will refuse; the *lower* bound still has to be checked on submit, because
              // an input can always be emptied.
              maxLength={80}
              className="mt-1 w-full rounded-lg border border-line bg-canvas px-3 py-2 text-[13px]"
              data-testid="region-detail-name"
            />
            {nameError ? (
              <p className="mt-1 text-[12px] text-danger" data-testid="region-detail-name-error">
                {nameError}
              </p>
            ) : null}
            {savedAt ? (
              <p className="mt-1 text-[12px] text-muted" data-testid="region-detail-saved">
                Saved at {savedAt}
              </p>
            ) : null}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => void saveName()}
              disabled={saving}
              className="rounded-lg border border-line px-3 py-1.5 text-[13px] font-medium disabled:opacity-60"
              data-testid="region-detail-save"
            >
              Save name
            </button>
            {region.is_default ? (
              <span className="text-[12px] text-muted" data-testid="region-detail-default-note">
                This is the routing fallback. The last default cannot be demoted — the routing
                policy has to point somewhere.
              </span>
            ) : (
              <button
                type="button"
                onClick={() => void promote()}
                disabled={saving || !detail.multi_region_active}
                className="rounded-lg border border-line px-3 py-1.5 text-[13px] font-medium disabled:opacity-60"
                data-testid="region-detail-promote"
              >
                Set as default
              </button>
            )}
            {region.status === "maintenance" ? (
              <span className="inline-flex items-center gap-1.5 text-[12px] text-muted" data-testid="region-detail-maintenance">
                <ServerCog aria-hidden className="size-3.5" />
                In maintenance
              </span>
            ) : null}
          </div>
        </div>
      </section>

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="region-detail-latency"
      >
        <div className="flex items-center gap-2">
          <Globe2 aria-hidden className="size-4 text-muted" />
          <h3 className="text-[13px] font-medium">This region against the others</h3>
        </div>
        <ul className="mt-3 space-y-1.5" data-testid="region-detail-latency-list">
          {detail.latency.cells
            .filter((cell) => cell.from_region === code || cell.to_region === code)
            .map((cell) => {
              const other = cell.from_region === code ? cell.to_region : cell.from_region;
              return (
                <li key={`${cell.from_region}-${cell.to_region}`} className="text-[13px]">
                  <span className="text-muted">
                    {cell.from_region === code ? "to" : "from"}
                  </span>{" "}
                  <strong>{other}</strong>{" "}
                  {cell.p95_ms === null ? (
                    <span className="text-[12px] text-muted">no measurement</span>
                  ) : (
                    <span className="tabular-nums">
                      {cell.p95_ms} ms
                      {cell.stale ? <span className="ml-1 text-muted">(stale)</span> : null}
                    </span>
                  )}
                </li>
              );
            })}
          {detail.latency.cells.filter(
            (cell) => cell.from_region === code || cell.to_region === code,
          ).length === 0 ? (
            <li className="text-[12px] text-muted">
              No region-to-region measurement involves {code} yet.
            </li>
          ) : null}
        </ul>
      </section>
    </div>
  );
}
