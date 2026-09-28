"use client";

/**
 * The Modules, Settings and Billing tabs of `/organizations/[id]` (REQ-005, slice 3).
 *
 * Slices 1 and 2 answered *who is in this organization* and *how they are arranged*. These
 * three answer the questions that make a tenant configurable and bounded: which parts of the
 * platform it uses, how it presents itself, and how much of it it has.
 *
 * The rule that shapes all three: **a limit that only decorates the UI is a lie.** Every number
 * the Billing tab shows is read from the same endpoint the guards read, and every refusal names
 * its ceiling, so an operator never has to guess whether the bar on screen is the rule the API
 * applies or a drawing of one.
 */
import { useCallback, useEffect, useState } from "react";

import { Download, Gauge, Loader2, Palette, Puzzle, RefreshCw, Save } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  downloadOrganizationUsage,
  fetchOrganizationLimits,
  fetchOrganizationModules,
  fetchOrganizationSettings,
  fetchOrganizationUsage,
  updateOrganizationLimits,
  updateOrganizationModules,
  updateOrganizationSettings,
  type OrganizationLimits,
  type OrganizationModulesPayload,
  type OrganizationSettingsPayload,
  type OrganizationUsage,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** The three states a tab can be in; every one of them is rendered, none is implied. */
type TabState = "loading" | "ready" | "error";

/** The inline error a refused save shows above the form, with its API code. */
function Refusal({
  error,
  onRetry,
}: {
  error: ApiError | Error;
  onRetry?: () => void;
}) {
  const code = error instanceof ApiError ? error.code : null;
  return (
    <div
      role="alert"
      className="flex flex-wrap items-center gap-3 rounded-lg border border-caution/40 bg-caution-soft px-3 py-2.5"
    >
      <p className="min-w-0 flex-1 text-[12.5px] text-caution">
        {error.message}
        {code ? <span className="ml-2 opacity-70">({code})</span> : null}
      </p>
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          className="rounded-md border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
        >
          Try again
        </button>
      ) : null}
    </div>
  );
}

/** The shared "this tab failed to load" panel. */
function LoadFailure({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
      <p className="text-[12.5px] text-accent-strong">{message}</p>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
      >
        <RefreshCw className="size-3.5" aria-hidden />
        Try again
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------------------------

/** What switching a module off changes, in one sentence the operator can act on. */
function consequenceOf(key: string, enabled: boolean): string {
  if (enabled) {
    return "Switching this off hides its navigation and makes its API answer 403 naming the module.";
  }
  return "Switching this on restores its navigation and its API.";
}

/** The Modules tab: one row per installed module with the switch that decides it. */
export function ModulesTab({ organization }: { organization: Organization }) {
  const [payload, setPayload] = useState<OrganizationModulesPayload | null>(null);
  const [state, setState] = useState<TabState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<ApiError | Error | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      setPayload(await fetchOrganizationModules(organization.id));
      setState("ready");
    } catch (cause) {
      setState("error");
      setError(
        cause instanceof ApiError ? cause.message : "The modules could not be loaded.",
      );
    }
  }, [organization.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggle = useCallback(
    async (key: string, enabled: boolean) => {
      setBusy(key);
      setRefusal(null);
      try {
        setPayload(await updateOrganizationModules(organization.id, [{ module_key: key, enabled }]));
      } catch (cause) {
        setRefusal(
          cause instanceof ApiError ? cause : new Error("The module could not be switched."),
        );
        // A refused switch must leave the screen agreeing with the server, not with the click.
        await load();
      } finally {
        setBusy(null);
      }
    },
    [organization.id, load],
  );

  // `S` toggles the focused module, as the request's keyboard section asks for.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (event.key !== "s" && event.key !== "S") return;
      const focused = document.activeElement as HTMLElement | null;
      const key = focused?.dataset?.organizationModule;
      if (!key) return;
      event.preventDefault();
      const row = payload?.modules.find((module) => module.key === key);
      if (row && !busy) void toggle(key, !row.enabled);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [payload, busy, toggle]);

  if (state === "loading") {
    return <LoadingTable columns={3} rows={5} />;
  }
  if (state === "error" || !payload) {
    return <LoadFailure message={error ?? "The modules could not be loaded."} onRetry={load} />;
  }

  return (
    <div className="flex flex-col gap-3">
      {refusal ? <Refusal error={refusal} /> : null}

      {payload.modules.length === 0 ? (
        <p className="rounded-xl border border-line bg-surface px-4 py-8 text-center text-[12.5px] text-muted">
          This installation ships no modules, so there is nothing to enable or switch off.
        </p>
      ) : (
        <ul className="flex flex-col gap-2">
          {payload.modules.map((module) => {
            const pending = busy === module.key;
            return (
              <li
                key={module.key}
                data-organization-module={module.key}
                className="flex items-start gap-3 rounded-xl border border-line bg-surface px-3.5 py-3"
              >
                <button
                  type="button"
                  role="switch"
                  aria-checked={module.enabled}
                  aria-label={`${module.enabled ? "Disable" : "Enable"} ${module.name}`}
                  disabled={pending}
                  onClick={() => void toggle(module.key, !module.enabled)}
                  className={`mt-0.5 flex h-5 w-9 shrink-0 items-center rounded-full border transition disabled:opacity-60 ${
                    module.enabled ? "border-accent bg-accent-soft" : "border-line bg-canvas"
                  }`}
                >
                  <span
                    className={`size-3.5 rounded-full transition ${
                      module.enabled ? "ml-4.5 bg-accent-strong" : "ml-0.5 bg-muted/60"
                    }`}
                  />
                </button>

                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <Puzzle className="size-3.5 text-muted" aria-hidden />
                    <p className="text-[13px] font-medium">{module.name}</p>
                    <code className="rounded bg-canvas px-1.5 py-0.5 text-[11px] text-muted">
                      {module.key}
                    </code>
                    {module.explicit ? null : (
                      <span className="rounded-full border border-line px-2 py-0.5 text-[10.5px] text-muted">
                        On by default
                      </span>
                    )}
                    {pending ? (
                      <Loader2 className="size-3.5 animate-spin text-muted" aria-hidden />
                    ) : null}
                  </div>
                  <p className="mt-0.5 text-[12px] text-muted">{module.description}</p>
                  <p className="mt-1 text-[11.5px] text-muted/80">
                    {consequenceOf(module.key, module.enabled)}
                  </p>
                </div>
              </li>
            );
          })}
        </ul>
      )}

      <p className="text-[11.5px] text-muted">
        Focus a module and press <kbd className="rounded border border-line px-1">S</kbd> to
        switch it.
      </p>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/** The Settings tab: the organization's own preferences, saved as one row. */
export function SettingsTab({ organization }: { organization: Organization }) {
  const [payload, setPayload] = useState<OrganizationSettingsPayload | null>(null);
  const [state, setState] = useState<TabState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<ApiError | Error | null>(null);
  const [saving, setSaving] = useState(false);

  // The form is edited in local state and only pushed on save, because the API takes the whole
  // row: a field the operator is halfway through typing would otherwise be a `PUT`.
  const [locale, setLocale] = useState("en");
  const [timezone, setTimezone] = useState("UTC");
  const [policy, setPolicy] = useState("owner_approval");
  const [accent, setAccent] = useState("");
  const [retention, setRetention] = useState(365);

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      const next = await fetchOrganizationSettings(organization.id);
      setPayload(next);
      setLocale(next.settings.locale);
      setTimezone(next.settings.timezone);
      setPolicy(next.settings.invite_policy);
      setAccent(next.settings.accent_color ?? "");
      setRetention(next.settings.audit_retention_days);
      setState("ready");
    } catch (cause) {
      setState("error");
      setError(
        cause instanceof ApiError ? cause.message : "The settings could not be loaded.",
      );
    }
  }, [organization.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = useCallback(async () => {
    setSaving(true);
    setRefusal(null);
    try {
      const next = await updateOrganizationSettings(organization.id, {
        locale,
        timezone,
        invite_policy: policy,
        default_invite_role_id: payload?.settings.default_invite_role_id ?? null,
        logo_media_id: payload?.settings.logo_media_id ?? null,
        accent_color: accent.trim() === "" ? null : accent.trim(),
        audit_retention_days: retention,
      });
      setPayload(next);
      setAccent(next.settings.accent_color ?? "");
    } catch (cause) {
      setRefusal(
        cause instanceof ApiError ? cause : new Error("The settings could not be saved."),
      );
    } finally {
      setSaving(false);
    }
  }, [organization.id, locale, timezone, policy, accent, retention, payload]);

  if (state === "loading") {
    return <LoadingTable columns={2} rows={4} />;
  }
  if (state === "error" || !payload) {
    return <LoadFailure message={error ?? "The settings could not be loaded."} onRetry={load} />;
  }

  const activePolicy = payload.invite_policies.find((entry) => entry.key === policy);

  return (
    <form
      className="flex flex-col gap-4"
      onSubmit={(event) => {
        event.preventDefault();
        void save();
      }}
    >
      {refusal ? <Refusal error={refusal} /> : null}

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1.5">
          <span className="text-[12px] font-medium text-ink">Interface language</span>
          <select
            value={locale}
            onChange={(event) => setLocale(event.target.value)}
            data-organization-settings-locale
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          >
            {payload.available_locales.map((entry) => (
              <option key={entry} value={entry}>
                {entry}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1.5">
          <span className="text-[12px] font-medium text-ink">Timezone</span>
          <input
            value={timezone}
            onChange={(event) => setTimezone(event.target.value)}
            placeholder="Europe/Istanbul"
            data-organization-settings-timezone
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
        </label>
      </div>

      <fieldset className="flex flex-col gap-1.5">
        <legend className="text-[12px] font-medium text-ink">Who may invite</legend>
        <div className="flex flex-col gap-1.5">
          {payload.invite_policies.map((entry) => (
            <label
              key={entry.key}
              className="flex cursor-pointer items-start gap-2.5 rounded-lg border border-line px-3 py-2"
            >
              <input
                type="radio"
                name="invite-policy"
                value={entry.key}
                checked={policy === entry.key}
                onChange={() => setPolicy(entry.key)}
                className="mt-0.5"
              />
              <span className="min-w-0">
                <span className="block text-[12.5px] font-medium">{entry.key.replace("_", " ")}</span>
                <span className="block text-[11.5px] leading-relaxed text-muted">
                  {entry.description}
                </span>
              </span>
            </label>
          ))}
        </div>
        {activePolicy ? (
          <p className="text-[11.5px] text-muted">
            Saved policy: <strong className="font-medium">{activePolicy.key}</strong> —{" "}
            {activePolicy.description}
          </p>
        ) : null}
      </fieldset>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1.5">
          <span className="text-[12px] font-medium text-ink">Accent colour</span>
          <div className="flex items-center gap-2">
            <input
              type="color"
              value={accent === "" ? "#000000" : accent}
              onChange={(event) => setAccent(event.target.value)}
              aria-label="Pick the accent colour"
              className="h-8 w-10 cursor-pointer rounded border border-line bg-surface"
            />
            <input
              value={accent}
              onChange={(event) => setAccent(event.target.value)}
              placeholder="Platform default"
              data-organization-settings-accent
              className="w-32 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] font-mono"
            />
            {accent === "" ? (
              <span className="text-[11.5px] text-muted">Platform default</span>
            ) : null}
          </div>
        </label>

        <label className="flex flex-col gap-1.5">
          <span className="text-[12px] font-medium text-ink">Audit retention (days)</span>
          <input
            type="number"
            min={30}
            max={3650}
            value={retention}
            onChange={(event) => setRetention(Number(event.target.value))}
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          />
          <span className="text-[11.5px] text-muted">Between 30 and 3650 days.</span>
        </label>
      </div>

      <div className="flex items-center gap-3">
        <button
          type="submit"
          disabled={saving}
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent-strong px-3 py-1.5 text-[12.5px] font-medium text-canvas transition disabled:opacity-60"
        >
          {saving ? (
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
          ) : (
            <Save className="size-3.5" aria-hidden />
          )}
          Save settings
        </button>
        <span className="inline-flex items-center gap-1.5 text-[11.5px] text-muted">
          <Palette className="size-3.5" aria-hidden />
          The accent colour is what the switcher and the sign-in surface use.
        </span>
      </div>
    </form>
  );
}

// ---------------------------------------------------------------------------------------------
// Billing
// ---------------------------------------------------------------------------------------------

/** A number the panel shows: `1.2 GB` rather than `1288490188`. */
function humanBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

/** A number the panel shows for AI spend, in whole currency units. */
function humanMicros(micros: number): string {
  return (micros / 1_000_000).toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/** One usage bar: the used figure, the ceiling, and where the ceiling comes from. */
function UsageBar({
  label,
  used,
  limit,
  render,
  source,
}: {
  label: string;
  used: number;
  limit: number | null;
  render: (value: number) => string;
  source: string;
}) {
  const ratio = limit && limit > 0 ? Math.min(1, used / limit) : 0;
  const unlimited = limit === null;
  return (
    <div className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface px-3.5 py-3">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <p className="text-[12.5px] font-medium">{label}</p>
        <p className="text-[12.5px] text-muted">
          <span className="font-medium text-ink">{render(used)}</span>
          {" / "}
          {unlimited ? "unlimited" : render(limit)}
        </p>
      </div>
      {/* An unlimited bar has no maximum, and `role="progressbar"` without `aria-valuemax`
          announces as indeterminate — which is honest about the ceiling but says nothing about
          the figure. `aria-valuetext` carries the readable "3 of unlimited" in that case, so a
          screen reader gets the number either way rather than a bar that only says "busy". */}
      <div
        role="progressbar"
        aria-label={label}
        aria-valuenow={used}
        aria-valuemin={0}
        aria-valuemax={unlimited ? undefined : limit ?? undefined}
        aria-valuetext={`${render(used)} of ${unlimited ? "unlimited" : render(limit)}`}
        className="h-1.5 w-full overflow-hidden rounded-full bg-canvas"
      >
        <div
          className={`h-full rounded-full ${unlimited ? "bg-muted/40" : "bg-accent-strong"}`}
          style={{ width: unlimited ? "100%" : `${Math.round(ratio * 100)}%` }}
        />
      </div>
      <p className="text-[11px] text-muted/80">{source}</p>
    </div>
  );
}

/** The Billing tab: the plan, the ceilings, and what the organization actually holds. */
export function BillingTab({ organization }: { organization: Organization }) {
  const [usage, setUsage] = useState<OrganizationUsage | null>(null);
  const [limitsPayload, setLimitsPayload] = useState<{
    limits: OrganizationLimits;
    available_plans: { key: string; description: string }[];
  } | null>(null);
  const [state, setState] = useState<TabState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<ApiError | Error | null>(null);
  const [saving, setSaving] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const [draft, setDraft] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    setState("loading");
    setError(null);
    try {
      const [nextUsage, nextLimits] = await Promise.all([
        fetchOrganizationUsage(organization.id),
        fetchOrganizationLimits(organization.id),
      ]);
      setUsage(nextUsage);
      setLimitsPayload({ limits: nextLimits.limits, available_plans: nextLimits.available_plans });
      setDraft({
        plan: nextLimits.limits.plan,
        seat_limit: nextLimits.limits.seat_limit === null ? "" : String(nextLimits.limits.seat_limit),
        site_limit: nextLimits.limits.site_limit === null ? "" : String(nextLimits.limits.site_limit),
        storage_bytes_limit:
          nextLimits.limits.storage_bytes_limit === null
            ? ""
            : String(Math.round(nextLimits.limits.storage_bytes_limit / (1024 * 1024))),
        ai_monthly_limit_micros:
          nextLimits.limits.ai_monthly_limit_micros === null
            ? ""
            : String(Math.round(nextLimits.limits.ai_monthly_limit_micros / 1_000_000)),
      });
      setState("ready");
    } catch (cause) {
      setState("error");
      setError(
        cause instanceof ApiError ? cause.message : "The usage could not be loaded.",
      );
    }
  }, [organization.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = useCallback(async () => {
    setSaving(true);
    setRefusal(null);
    // A blank field is "unlimited" rather than zero — the API refuses a zero ceiling, and the
    // empty box is how an operator says "no ceiling" without having to know that.
    const numberOrNull = (value: string) => (value.trim() === "" ? null : Number(value));
    try {
      const next = await updateOrganizationLimits(organization.id, {
        plan: draft.plan ?? "standard",
        seat_limit: numberOrNull(draft.seat_limit ?? ""),
        site_limit: numberOrNull(draft.site_limit ?? ""),
        storage_bytes_limit:
          numberOrNull(draft.storage_bytes_limit ?? "") === null
            ? null
            : Number(draft.storage_bytes_limit) * 1024 * 1024,
        ai_monthly_limit_micros:
          numberOrNull(draft.ai_monthly_limit_micros ?? "") === null
            ? null
            : Math.round(Number(draft.ai_monthly_limit_micros) * 1_000_000),
      });
      setLimitsPayload({ limits: next.limits, available_plans: next.available_plans });
      await load();
    } catch (cause) {
      setRefusal(
        cause instanceof ApiError ? cause : new Error("The plan could not be saved."),
      );
    } finally {
      setSaving(false);
    }
  }, [organization.id, draft, load]);

  const download = useCallback(async () => {
    setDownloading(true);
    setRefusal(null);
    try {
      const blob = await downloadOrganizationUsage(organization.id);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `omnion-usage-${organization.slug}.csv`;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
    } catch (cause) {
      setRefusal(
        cause instanceof ApiError ? cause : new Error("The usage file could not be downloaded."),
      );
    } finally {
      setDownloading(false);
    }
  }, [organization.id, organization.slug]);

  if (state === "loading") {
    return <LoadingTable columns={2} rows={4} />;
  }
  if (state === "error" || !usage || !limitsPayload) {
    return <LoadFailure message={error ?? "The usage could not be loaded."} onRetry={load} />;
  }

  const activePlan = limitsPayload.available_plans.find((entry) => entry.key === draft.plan);

  return (
    <div className="flex flex-col gap-4">
      {refusal ? <Refusal error={refusal} /> : null}

      <div className="grid gap-2.5 sm:grid-cols-2">
        <UsageBar
          label="Members"
          used={usage.seats_used}
          limit={usage.limits.seat_limit}
          render={(value) => String(Math.round(value))}
          source={usage.sources.seats}
        />
        <UsageBar
          label="Sites"
          used={usage.sites_used}
          limit={usage.limits.site_limit}
          render={(value) => String(Math.round(value))}
          source={usage.sources.sites}
        />
        <UsageBar
          label="Storage"
          used={usage.storage_used_bytes}
          limit={usage.limits.storage_bytes_limit}
          render={humanBytes}
          source={usage.sources.storage_bytes}
        />
        <UsageBar
          label="AI spend this month"
          used={usage.ai_micros_this_month}
          limit={usage.limits.ai_monthly_limit_micros}
          render={humanMicros}
          source={usage.sources.ai_monthly_micros}
        />
      </div>

      <form
        className="flex flex-col gap-3 rounded-xl border border-line bg-surface px-3.5 py-3"
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <div className="flex items-center gap-2">
          <Gauge className="size-4 text-muted" aria-hidden />
          <p className="text-[13px] font-medium">Plan and ceilings</p>
        </div>

        <label className="flex flex-col gap-1.5">
          <span className="text-[12px] font-medium text-ink">Plan</span>
          <select
            value={draft.plan ?? "standard"}
            onChange={(event) => setDraft((prev) => ({ ...prev, plan: event.target.value }))}
            data-organization-billing-plan
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          >
            {limitsPayload.available_plans.map((entry) => (
              <option key={entry.key} value={entry.key}>
                {entry.key}
              </option>
            ))}
          </select>
          {activePlan ? (
            <span className="text-[11.5px] text-muted">{activePlan.description}</span>
          ) : null}
        </label>

        <div className="grid gap-3 sm:grid-cols-2">
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] font-medium text-ink">Seats</span>
            <input
              type="number"
              min={1}
              value={draft.seat_limit ?? ""}
              onChange={(event) =>
                setDraft((prev) => ({ ...prev, seat_limit: event.target.value }))
              }
              placeholder="Unlimited"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] font-medium text-ink">Sites</span>
            <input
              type="number"
              min={1}
              value={draft.site_limit ?? ""}
              onChange={(event) =>
                setDraft((prev) => ({ ...prev, site_limit: event.target.value }))
              }
              placeholder="Unlimited"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] font-medium text-ink">Storage (MB)</span>
            <input
              type="number"
              min={1}
              value={draft.storage_bytes_limit ?? ""}
              onChange={(event) =>
                setDraft((prev) => ({ ...prev, storage_bytes_limit: event.target.value }))
              }
              placeholder="Unlimited"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] font-medium text-ink">AI budget per month</span>
            <input
              type="number"
              min={0}
              step="0.01"
              value={draft.ai_monthly_limit_micros ?? ""}
              onChange={(event) =>
                setDraft((prev) => ({ ...prev, ai_monthly_limit_micros: event.target.value }))
              }
              placeholder="Unlimited"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
            />
          </label>
        </div>

        <p className="text-[11.5px] text-muted">
          Leave a field empty for no ceiling. Payment is out of scope here — changing the plan
          records a request, it does not charge anything.
        </p>

        <div className="flex flex-wrap items-center gap-2">
          <button
            type="submit"
            disabled={saving}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent-strong px-3 py-1.5 text-[12.5px] font-medium text-canvas transition disabled:opacity-60"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Save className="size-3.5" aria-hidden />
            )}
            Save plan
          </button>
          <button
            type="button"
            onClick={() => void download()}
            disabled={downloading}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-60"
          >
            {downloading ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Download className="size-3.5" aria-hidden />
            )}
            Download usage CSV
          </button>
        </div>
      </form>
    </div>
  );
}
