"use client";

/**
 * The Health, Usage and failover panels of the AI Hub (REQ-097 slice 3).
 *
 * Three questions an operator asks about a provider connection, in the order they ask them:
 * *Is it up?* · *What is it costing me?* · *Who does a failing call fall back to?* Each gets one
 * panel, and each panel is fed by a single call — a header that disagrees with the samples under
 * it is the one thing that makes a health screen useless during an incident.
 *
 * The panels are a separate module from `ai-view.tsx` because they have their own loading and
 * error lifecycles: the provider list must stay usable while a probe is dialing a dead endpoint.
 */
import { useCallback, useEffect, useState } from "react";

import {
  Activity,
  ArrowDown,
  ArrowUp,
  Loader2,
  RefreshCw,
  Stethoscope,
} from "lucide-react";

import {
  ApiError,
  type AiFailoverView,
  type AiHealthSample,
  type AiHealthView,
  type AiProvider,
  type AiUsageView,
  fetchAiFailover,
  fetchAiProviderHealth,
  fetchAiProviderUsage,
  probeAiProvider,
  setAiFailoverOrder,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The window the tab shows first: a day is long enough to be a verdict, short enough to be recent. */
const DEFAULT_WINDOW = "24h";

/** Which of the three panels is open. One at a time — they compete for the same vertical space. */
type Panel = "health" | "usage" | "failover";

/** The button that opens a panel, and the panel it belongs to. */
const PANEL_BUTTONS: { panel: Panel; label: string; icon: typeof Activity }[] = [
  { panel: "health", label: "Health", icon: Activity },
  { panel: "usage", label: "Usage", icon: ArrowUp },
  { panel: "failover", label: "Failover", icon: ArrowDown },
];

/** The message a failed call shows, from whatever it threw. */
function reasonOf(cause: unknown, fallback: string): string {
  if (cause instanceof ApiError) {
    return cause.message;
  }
  return fallback;
}

/** A number, or an honest dash when the window holds none. */
function numberOrDash(value: number | null): string {
  return value === null ? "—" : value.toLocaleString();
}

/**
 * A window's cost, in micros (REQ-098 slice 5).
 *
 * `null` renders as an em dash and never as `0`, and the difference is the point: a null means
 * nothing in the window could be priced, which is a different statement from a window of free
 * models. The unit is spelled out in the figure's own hint rather than baked into the string, so
 * a locale that formats numbers differently cannot turn the amount into a different number.
 */
function formatCostMicros(micros: number | null): string {
  if (micros === null) return "—";
  return `${micros.toLocaleString("en-US")} µ`;
}

/** One figure in the header strip. */
function Figure({
  label,
  value,
  hint,
  testId,
}: {
  label: string;
  value: string;
  hint?: string;
  testId?: string;
}) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[10.5px] uppercase tracking-wide text-muted">{label}</span>
      <span className="text-[15px] font-semibold tabular-nums" data-figure={testId ?? label}>
        {value}
      </span>
      {hint ? <span className="text-[10.5px] text-muted">{hint}</span> : null}
    </div>
  );
}

/**
 * The latency sparkline.
 *
 * Oldest sample on the left, so the line reads the way the eye expects time to run. Each bar's
 * height is relative to the **slowest sample in the window**, not to a fixed scale: a provider
 * answering in 40 ms and one answering in 4 s both have to be readable, and a fixed ceiling would
 * flatten the fast one into a straight line that looks healthy when it is not the question.
 */
function Sparkline({ samples }: { samples: AiHealthSample[] }) {
  if (samples.length === 0) {
    return null;
  }
  const ordered = [...samples].reverse();
  const slowest = Math.max(...ordered.map((sample) => sample.latency_ms), 1);

  return (
    <div
      role="img"
      aria-label={`Latency of the last ${ordered.length} probes, oldest first`}
      data-sparkline
      className="flex h-12 items-end gap-px"
    >
      {ordered.map((sample) => (
        <span
          key={sample.id}
          title={`${sample.latency_ms} ms · ${formatTimestamp(sample.checked_at)}`}
          data-spark-bar={sample.status}
          className={`min-w-0.5 flex-1 rounded-t-sm ${
            sample.status === "down" ? "bg-danger" : "bg-accent/70"
          }`}
          style={{ height: `${Math.max(8, (sample.latency_ms / slowest) * 100)}%` }}
        />
      ))}
    </div>
  );
}

/** The window switcher. The keys come from the server, so a new window needs no edit here. */
function WindowPicker({
  windows,
  current,
  onChange,
}: {
  windows: string[];
  current: string;
  onChange: (window: string) => void;
}) {
  if (windows.length === 0) {
    return null;
  }
  return (
    <div
      role="group"
      aria-label="Time window"
      className="flex items-center gap-1 rounded-lg border border-line p-0.5"
    >
      {windows.map((window) => (
        <button
          key={window}
          type="button"
          onClick={() => onChange(window)}
          aria-pressed={window === current}
          data-window={window}
          className={`rounded-md px-2 py-1 text-[11.5px] transition ${
            window === current
              ? "bg-accent-soft font-medium text-accent-strong"
              : "text-muted hover:bg-canvas"
          }`}
        >
          {window}
        </button>
      ))}
    </div>
  );
}

/**
 * The Health tab: what the provider's status is, and the samples that made it so.
 *
 * `Probe now` writes exactly one sample and takes the refreshed header out of the probe's own
 * answer — there is no second request, so what the operator sees after pressing the button is what
 * the server computed, not a guess made before the write landed.
 */
function HealthPanel({ provider }: { provider: AiProvider }) {
  const [view, setView] = useState<AiHealthView | null>(null);
  const [window, setWindow] = useState(DEFAULT_WINDOW);
  const [loading, setLoading] = useState(true);
  const [probing, setProbing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setView(await fetchAiProviderHealth(provider.id, window));
    } catch (cause: unknown) {
      setView(null);
      setError(reasonOf(cause, "The health of this provider could not be read."));
    } finally {
      setLoading(false);
    }
  }, [provider.id, window]);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      // `load` owns the cancelled flag's absence: the effect below re-runs on every window
      // change, and a late answer from a window the operator already left must not overwrite the
      // one they are looking at.
      if (cancelled) {
        return;
      }
      await load();
    })();
    return () => {
      cancelled = true;
    };
  }, [load]);

  const probe = async () => {
    setProbing(true);
    setError(null);
    setNotice(null);
    try {
      const outcome = await probeAiProvider(provider.id);
      // The header comes back in the answer; the sample list is refetched because the probe added
      // one and the sparkline has to show it without a reload.
      setView((current) => (current ? { ...current, summary: outcome.summary } : current));
      await load();
      setNotice(
        outcome.transition
          ? `${provider.name} moved from ${outcome.transition.from} to ${outcome.transition.to}.`
          : `${provider.name} probed in ${outcome.latency_ms} ms. The status did not change.`,
      );
    } catch (cause: unknown) {
      setError(reasonOf(cause, "The probe did not go through."));
    } finally {
      setProbing(false);
    }
  };

  if (loading && view === null) {
    return (
      <p className="flex items-center gap-2 px-4 py-4 text-[12.5px] text-muted" data-health-loading>
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Reading the health of {provider.name}…
      </p>
    );
  }

  if (error && view === null) {
    return (
      <div className="flex flex-col gap-2 px-4 py-4" data-health-error>
        <p role="alert" className="text-[12.5px] text-danger">
          {error}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          data-health-retry
          className="w-fit rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    );
  }

  if (view === null) {
    return null;
  }

  const { summary, samples } = view;

  return (
    <div className="flex flex-col gap-3 px-4 py-3" data-health-panel={provider.id}>
      <div className="flex flex-wrap items-center gap-3">
        <WindowPicker windows={view.windows} current={view.window} onChange={setWindow} />
        <button
          type="button"
          onClick={() => void load()}
          data-health-refresh={provider.name}
          aria-label="Reload the health of this provider"
          className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
        >
          <RefreshCw className="size-3" aria-hidden />
          Refresh
        </button>
        <button
          type="button"
          onClick={() => void probe()}
          disabled={probing}
          data-health-probe={provider.name}
          className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
        >
          {probing ? (
            <Loader2 className="size-3 animate-spin" aria-hidden />
          ) : (
            <Stethoscope className="size-3" aria-hidden />
          )}
          Probe now
        </button>
        {loading ? (
          <span className="text-[11.5px] text-muted">Reloading…</span>
        ) : null}
      </div>

      {error ? (
        <p role="alert" data-health-inline-error className="text-[12px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p data-health-notice className="text-[12px] text-positive">
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-end gap-6" data-health-figures>
        <Figure
          label="Status"
          value={summary.status === "unknown" ? "Never probed" : summary.status}
          testId="status"
        />
        <Figure
          label="Uptime"
          value={
            summary.uptime_percent === null
              ? "—"
              : `${summary.uptime_percent.toFixed(1)}%`
          }
          hint={`over ${view.window}`}
          testId="uptime"
        />
        <Figure
          label="p95 latency"
          value={
            summary.p95_latency_ms === null ? "—" : `${summary.p95_latency_ms} ms`
          }
          testId="p95"
        />
        <Figure
          label="Baseline"
          value={
            summary.baseline_latency_ms === null
              ? "—"
              : `${summary.baseline_latency_ms} ms`
          }
          hint="7-day median this provider is judged against"
          testId="baseline"
        />
        <Figure label="Samples" value={numberOrDash(summary.sample_count)} testId="samples" />
      </div>

      {summary.last_error ? (
        <p data-health-last-error className="text-[12px] text-danger">
          {summary.last_error}
        </p>
      ) : null}

      {samples.length === 0 ? (
        <p className="text-[12.5px] text-muted" data-health-empty>
          No probe has been taken in this window yet. “Probe now” takes one immediately.
        </p>
      ) : (
        <>
          <Sparkline samples={samples} />
          <div className="max-h-56 overflow-y-auto rounded-lg border border-line">
            <table className="w-full text-left text-[12px]">
              <thead className="sticky top-0 bg-surface text-[10.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th scope="col" className="px-3 py-1.5 font-medium">
                    Time
                  </th>
                  <th scope="col" className="px-3 py-1.5 font-medium">
                    Status
                  </th>
                  <th scope="col" className="px-3 py-1.5 font-medium">
                    Latency
                  </th>
                  <th scope="col" className="px-3 py-1.5 font-medium">
                    HTTP
                  </th>
                  <th scope="col" className="px-3 py-1.5 font-medium">
                    Error
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-[var(--color-line)]">
                {samples.map((sample) => (
                  <tr key={sample.id} data-sample-row={sample.status}>
                    <td className="px-3 py-1.5 tabular-nums text-muted">
                      {formatTimestamp(sample.checked_at)}
                    </td>
                    <td className="px-3 py-1.5">
                      <span
                        data-sample-status={sample.status}
                        className={
                          sample.status === "down"
                            ? "text-danger"
                            : sample.status === "degraded"
                              ? "text-caution"
                              : "text-positive"
                        }
                      >
                        {sample.status}
                      </span>
                    </td>
                    <td className="px-3 py-1.5 tabular-nums">{sample.latency_ms} ms</td>
                    <td className="px-3 py-1.5 tabular-nums text-muted">
                      {sample.http_status ?? "—"}
                    </td>
                    <td className="max-w-[18rem] truncate px-3 py-1.5 text-muted" title={sample.error ?? ""}>
                      {sample.error ?? "—"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  );
}

/** The Usage tab: what the provider served, and what it cost in calls and tokens. */
function UsagePanel({ provider }: { provider: AiProvider }) {
  const [view, setView] = useState<AiUsageView | null>(null);
  const [window, setWindow] = useState(DEFAULT_WINDOW);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setView(await fetchAiProviderUsage(provider.id, window));
    } catch (cause: unknown) {
      setView(null);
      setError(reasonOf(cause, "The usage of this provider could not be read."));
    } finally {
      setLoading(false);
    }
  }, [provider.id, window]);

  useEffect(() => {
    void load();
  }, [load]);

  if (loading && view === null) {
    return (
      <p className="flex items-center gap-2 px-4 py-4 text-[12.5px] text-muted" data-usage-loading>
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Reading what {provider.name} served…
      </p>
    );
  }

  if (error && view === null) {
    return (
      <div className="flex flex-col gap-2 px-4 py-4" data-usage-error>
        <p role="alert" className="text-[12.5px] text-danger">
          {error}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          data-usage-retry
          className="w-fit rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    );
  }

  if (view === null) {
    return null;
  }

  const { summary } = view;
  const busiest = Math.max(...summary.by_day.map((day) => day.requests), 1);

  return (
    <div className="flex flex-col gap-3 px-4 py-3" data-usage-panel={provider.id}>
      <WindowPicker windows={view.windows} current={view.window} onChange={setWindow} />

      <div className="flex flex-wrap items-end gap-6" data-usage-figures>
        <Figure label="Requests" value={numberOrDash(summary.requests)} testId="requests" />
        <Figure
          label="Errors"
          value={numberOrDash(summary.errors)}
          hint={`${summary.error_rate_percent.toFixed(1)}% of calls`}
          testId="errors"
        />
        <Figure
          label="Prompt tokens"
          value={numberOrDash(summary.prompt_tokens)}
          testId="prompt-tokens"
        />
        <Figure
          label="Completion tokens"
          value={numberOrDash(summary.completion_tokens)}
          testId="completion-tokens"
        />
        <Figure
          label="p95 latency"
          value={summary.p95_latency_ms === null ? "—" : `${summary.p95_latency_ms} ms`}
          testId="usage-p95"
        />
        <Figure
          label="Cost"
          value={formatCostMicros(summary.cost_micros)}
          hint="at the price each call was billed"
          testId="usage-cost"
        />
      </div>

      {summary.uncosted_calls > 0 ? (
        <p data-usage-uncosted className="text-[12px] text-caution">
          {summary.uncosted_calls} call{summary.uncosted_calls === 1 ? "" : "s"} in this window
          could not be priced — the model has no price, or the endpoint reported no token counts.
          They are left out of the cost figure rather than counted as free, and they stay unpriced
          in the history too: editing a price today changes new calls only.
        </p>
      ) : null}

      {summary.missing_usage > 0 ? (
        <p data-usage-missing className="text-[12px] text-caution">
          {summary.missing_usage} call{summary.missing_usage === 1 ? "" : "s"} in this window
          reported no token counts, so they are left out of the totals above rather than counted as
          zero.
        </p>
      ) : null}

      {summary.by_day.length === 0 ? (
        <p className="text-[12.5px] text-muted" data-usage-empty>
          {provider.name} has served no call in this window yet. The counters fill as agents and
          AI features use it.
        </p>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full text-left text-[12px]">
            <thead className="text-[10.5px] uppercase tracking-wide text-muted">
              <tr>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  Day
                </th>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  Requests
                </th>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  Errors
                </th>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  Prompt
                </th>
                <th scope="col" className="px-3 py-1.5 font-medium">
                  Completion
                </th>
              </tr>
            </thead>
            <tbody className="divide-y divide-[var(--color-line)]">
              {summary.by_day.map((day) => (
                <tr key={day.day} data-usage-day={day.day}>
                  <td className="px-3 py-1.5 tabular-nums">{day.day}</td>
                  <td className="px-3 py-1.5">
                    <span className="flex items-center gap-2">
                      <span
                        aria-hidden
                        data-day-bar
                        className="h-1.5 w-10 rounded-full bg-accent/60"
                        style={{ width: `${Math.max(8, (day.requests / busiest) * 100)}%` }}
                      />
                      <span className="tabular-nums">{day.requests}</span>
                    </span>
                  </td>
                  <td className="px-3 py-1.5 tabular-nums text-muted">{day.errors}</td>
                  <td className="px-3 py-1.5 tabular-nums text-muted">
                    {numberOrDash(day.prompt_tokens)}
                  </td>
                  <td className="px-3 py-1.5 tabular-nums text-muted">
                    {numberOrDash(day.completion_tokens)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/**
 * The failover editor: the order a failing call walks, and the buttons that change it.
 *
 * The order is not a drag list on purpose — the chain is a handful of rows, and a keyboard-driven
 * "move up / move down" is both faster and reachable, where a drag handle is neither. The list
 * renders what the server answered after the PUT, so a rejected order cannot look accepted.
 */
function FailoverPanel() {
  const [view, setView] = useState<AiFailoverView | null>(null);
  const [pending, setPending] = useState<string[] | null>(null);
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setView(await fetchAiFailover());
    } catch (cause: unknown) {
      setView(null);
      setError(reasonOf(cause, "The failover chain could not be read."));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async (ids: string[]) => {
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      // The answer is the stored order, not the one that was asked for.
      setView(await setAiFailoverOrder(ids));
      setNotice("The failover order was saved.");
    } catch (cause: unknown) {
      setError(reasonOf(cause, "The order was not saved."));
    } finally {
      setSaving(false);
    }
  };

  const move = (index: number, delta: number) => {
    const current = pending ?? view?.chain.map((entry) => entry.id) ?? [];
    const target = index + delta;
    if (target < 0 || target >= current.length) {
      return;
    }
    const next = [...current];
    [next[index], next[target]] = [next[target], next[index]];
    setPending(next);
    void save(next);
  };

  if (loading && view === null) {
    return (
      <p className="flex items-center gap-2 px-4 py-4 text-[12.5px] text-muted" data-failover-loading>
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Reading the failover chain…
      </p>
    );
  }

  if (view === null) {
    return (
      <div className="flex flex-col gap-2 px-4 py-4" data-failover-error>
        <p role="alert" className="text-[12.5px] text-danger">
          {error ?? "The failover chain could not be read."}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          data-failover-retry
          className="w-fit rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    );
  }

  if (view.chain.length === 0) {
    return (
      <p className="px-4 py-4 text-[12.5px] text-muted" data-failover-empty>
        No enabled provider can take a call. Enable a connection above and it joins the chain
        automatically.
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-3 px-4 py-3" data-failover-panel>
      <p className="text-[12px] text-muted">
        A call that names only a task walks this chain in order. A call that names a provider and
        model is never rerouted.
      </p>

      {error ? (
        <p role="alert" data-failover-inline-error className="text-[12px] text-danger">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p data-failover-notice className="text-[12px] text-positive">
          {notice}
        </p>
      ) : null}

      <ol className="divide-y divide-[var(--color-line)] rounded-lg border border-line">
        {view.chain.map((entry, index) => (
          <li
            key={entry.id}
            data-failover-row={entry.name}
            className="flex items-center gap-2 px-3 py-2"
          >
            <span className="w-5 text-[12px] font-medium tabular-nums text-muted">
              {index + 1}
            </span>
            <span className="min-w-0 flex-1 truncate text-[12.5px]">{entry.name}</span>
            {entry.is_default ? (
              <span className="rounded-full bg-accent-soft px-2 py-0.5 text-[10.5px] font-medium text-accent-strong">
                Default
              </span>
            ) : null}
            <span
              data-failover-health={entry.health}
              className="text-[11.5px] text-muted"
            >
              {entry.health === "unknown" ? "never probed" : entry.health}
            </span>
            <span className="flex items-center gap-1">
              <button
                type="button"
                onClick={() => move(index, -1)}
                disabled={saving || index === 0}
                aria-label={`Move ${entry.name} up`}
                data-failover-up={entry.name}
                className="rounded-lg border border-line p-1 transition hover:bg-canvas disabled:opacity-40"
              >
                <ArrowUp className="size-3" aria-hidden />
              </button>
              <button
                type="button"
                onClick={() => move(index, 1)}
                disabled={saving || index === view.chain.length - 1}
                aria-label={`Move ${entry.name} down`}
                data-failover-down={entry.name}
                className="rounded-lg border border-line p-1 transition hover:bg-canvas disabled:opacity-40"
              >
                <ArrowDown className="size-3" aria-hidden />
              </button>
            </span>
          </li>
        ))}
      </ol>

      {saving ? (
        <p className="flex items-center gap-2 text-[11.5px] text-muted">
          <Loader2 className="size-3 animate-spin" aria-hidden />
          Saving the order…
        </p>
      ) : null}
    </div>
  );
}

/**
 * The row of panel buttons, plus whichever panel is open.
 *
 * Rendered under a provider row, so `provider` decides whether the Health and Usage panels have a
 * subject at all; the failover chain is installation-wide, which is why it opens on the first
 * provider and stays open when the operator looks at another one.
 */
export function AiHealthPanels({
  provider,
  openPanel,
  onToggle,
}: {
  provider: AiProvider;
  openPanel: Panel | null;
  onToggle: (panel: Panel) => void;
}) {
  return (
    <div className="flex flex-col">
      <div className="flex flex-wrap items-center gap-1.5 px-4 pb-2">
        {PANEL_BUTTONS.map(({ panel, label, icon: Icon }) => (
          <button
            key={panel}
            type="button"
            onClick={() => onToggle(panel)}
            aria-expanded={openPanel === panel}
            data-panel-toggle={panel}
            className={`flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] transition ${
              openPanel === panel
                ? "bg-accent-soft font-medium text-accent-strong"
                : "text-muted hover:bg-canvas"
            }`}
          >
            <Icon className="size-3" aria-hidden />
            {label}
          </button>
        ))}
      </div>

      {openPanel === "health" ? <HealthPanel provider={provider} /> : null}
      {openPanel === "usage" ? <UsagePanel provider={provider} /> : null}
      {openPanel === "failover" ? <FailoverPanel /> : null}
    </div>
  );
}
