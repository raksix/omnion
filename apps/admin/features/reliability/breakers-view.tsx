"use client";

/**
 * `/settings/reliability/breakers` — the outbound circuit breakers (REQ-127, slice 3).
 *
 * A breaker is the thing standing between a provider that is down and a platform that keeps
 * calling it, so this screen has one job above all others: **never let an operator misread what
 * the platform is currently refusing to call.** Five readings it refuses to blur:
 *
 * - **`forced_open` is not `open`.** A forced breaker stays refused through its cooldown and
 *   through any probe — the flag outranks every rule — and it shows a banner until somebody
 *   resets it. Rendering both as an orange chip makes a deliberate drain indistinguishable from
 *   a provider that failed, and those want opposite actions.
 * - **`retry_after: null` is not a missing number.** It means no probe is scheduled at all
 *   (held open deliberately), so there is no wait to show. A `0` or a dash-with-a-number would
 *   be a promise the platform is not making.
 * - **`closed` after a restart is a stored state, not a computed one.** The row was read from
 *   the database; a breaker derived from a timestamp would have closed itself once the cooldown
 *   passed with nobody watching.
 * - **`half_open` is not recovering.** It is probing: a configured number of calls is let
 *   through and `successes_in_half_open` of them must succeed to close. A chip that says
 *   "recovering" invites an operator to leave it alone during the one window it can still close.
 * - **Editing thresholds does not change behaviour until a call is observed.** The save writes
 *   settings; the state machine is what moves.
 *
 * Keyboard: `/` filters, `e` edits the selected breaker, `Esc` closes a dialog. Under `sm:` the
 * table becomes cards and the confirmations stay reachable.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertOctagon,
  AlertTriangle,
  Ban,
  CheckCircle2,
  Info,
  Loader2,
  RotateCcw,
  Save,
  Search,
  ShieldCheck,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchReliabilityBreakers,
  forceOpenReliabilityBreaker,
  resetReliabilityBreaker,
  updateReliabilityBreaker,
  type ReliabilityBreaker,
  type ReliabilityBreakers,
} from "@/lib/api";

/** The state chip: icon + text, so the state is legible without colour. */
function stateChip(breaker: ReliabilityBreaker): {
  label: string;
  icon: typeof ShieldCheck;
  tone: string;
} {
  if (breaker.forced_open) {
    return { label: "Forced open", icon: Ban, tone: "border-rose-300 text-rose-800" };
  }
  switch (breaker.state) {
    case "open":
      return { label: "Open", icon: AlertOctagon, tone: "border-rose-300 text-rose-800" };
    case "half_open":
      return { label: "Half-open (probing)", icon: AlertTriangle, tone: "border-amber-300 text-amber-800" };
    default:
      return { label: "Closed", icon: ShieldCheck, tone: "border-emerald-300 text-emerald-800" };
  }
}

type Confirm = { breaker: ReliabilityBreaker; mode: "reset" | "force-open" } | null;

export function ReliabilityBreakersView() {
  const [data, setData] = useState<ReliabilityBreakers | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [editing, setEditing] = useState<ReliabilityBreaker | null>(null);
  const [draft, setDraft] = useState({
    name: "",
    failure_threshold: 5,
    window_seconds: 60,
    cooldown_seconds: 30,
    half_open_probes: 3,
    success_threshold: 3,
  });
  const [confirm, setConfirm] = useState<Confirm>(null);
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchReliabilityBreakers());
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      }
      if (event.key === "Escape") {
        setConfirm(null);
        setEditing(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle || !data) return data?.breakers ?? [];
    return data.breakers.filter((breaker) =>
      [breaker.key, breaker.name, breaker.state]
        .filter(Boolean)
        .some((field) => field.toLowerCase().includes(needle)),
    );
  }, [data, filter]);

  const forced = (data?.breakers ?? []).filter((b) => b.forced_open);

  const openEditor = (breaker: ReliabilityBreaker) => {
    setEditing(breaker);
    setDraft({
      name: breaker.name,
      failure_threshold: breaker.failure_threshold,
      window_seconds: breaker.window_seconds,
      cooldown_seconds: breaker.cooldown_seconds,
      half_open_probes: breaker.half_open_probes,
      success_threshold: breaker.success_threshold,
    });
  };

  const saveThresholds = async () => {
    if (!editing) return;
    setBusy(true);
    try {
      await updateReliabilityBreaker(editing.key, draft);
      setEditing(null);
      setNotice(
        `Saved the thresholds for ${editing.key}. They take effect on the next observation, not now.`,
      );
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  const runConfirm = async () => {
    if (!confirm) return;
    setBusy(true);
    try {
      if (confirm.mode === "reset") {
        await resetReliabilityBreaker(confirm.breaker.key, reason);
        setNotice(
          `Closed ${confirm.breaker.key} and cleared its forced flag. A reset that only wrote the state would leave the flag set, and the breaker would refuse while looking healthy.`,
        );
      } else {
        await forceOpenReliabilityBreaker(confirm.breaker.key, reason);
        setNotice(
          `${confirm.breaker.key} is drained until somebody resets it — the flag outranks the cooldown.`,
        );
      }
      setConfirm(null);
      setReason("");
      await load();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  };

  if (loading && !data) {
    return (
      <div data-view="reliability-breakers">
        <LoadingTable columns={5} />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-6" data-view="reliability-breakers">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-rose-200 bg-rose-50 px-4 py-3 text-[13px] text-rose-800"
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
          <span>{error}</span>
        </div>
      ) : null}
      {notice ? (
        <div
          role="status"
          className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-4 py-3 text-[13px]"
        >
          <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-emerald-700" aria-hidden="true" />
          <span>{notice}</span>
        </div>
      ) : null}

      {/* A forced breaker gets a banner, because its state chip alone does not say "on purpose". */}
      {forced.length > 0 ? (
        <div
          role="status"
          className="rounded-lg border border-rose-300 bg-rose-50 px-4 py-3 text-[13px] text-rose-900"
        >
          <p className="font-medium">
            {forced.length} provider{forced.length === 1 ? " is" : "s are"} drained by hand
          </p>
          <p className="mt-1">
            {forced.map((b) => b.key).join(", ")} will stay refused until somebody resets them. A
            forced breaker does not close on its cooldown and no probe closes it.
          </p>
        </div>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-3">
        {(data?.states ?? ["closed", "open", "half_open"]).map((state) => {
          const count =
            data?.state_counts.find(([name]) => name === state)?.[1] ?? 0;
          return (
            <div key={state} className="rounded-lg border border-line px-4 py-3">
              <p className="text-[12px] text-muted">{state.replace("_", "-")}</p>
              <p className="text-[22px] font-semibold tabular-nums">{count}</p>
              <p className="text-[11.5px] text-muted">providers</p>
            </div>
          );
        })}
      </div>

      <section className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[14px] font-medium">Breakers</h2>
          <label className="flex items-center gap-2 text-[12.5px] text-muted">
            <Search className="h-3.5 w-3.5" aria-hidden="true" />
            <input
              ref={searchRef}
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter (press /)"
              className="w-52 rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink outline-none focus:border-ink-soft"
            />
          </label>
        </header>

        {visible.length === 0 ? (
          <EmptyState
            title={data?.breakers.length ? "Nothing matches that filter" : "No breaker has a row yet"}
            hint={
              data?.breakers.length
                ? "Clear the filter to see every provider."
                : "A row is written the first time a provider trips or an operator arms it, so an install nobody configured has no permanently-closed rows for every provider in existence."
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead className="text-[12px] text-muted">
                <tr>
                  <th className="px-4 py-2.5 font-medium">Provider</th>
                  <th className="px-4 py-2.5 font-medium">State</th>
                  <th className="px-4 py-2.5 font-medium">Failures</th>
                  <th className="px-4 py-2.5 font-medium">Thresholds</th>
                  <th className="px-4 py-2.5 font-medium">Trips</th>
                  <th className="px-4 py-2.5" />
                </tr>
              </thead>
              <tbody>
                {visible.map((breaker) => {
                  const chip = stateChip(breaker);
                  const Icon = chip.icon;
                  return (
                    <tr key={breaker.key} className="border-t border-line" data-breaker={breaker.key}>
                      <td className="px-4 py-3">
                        <span className="font-medium">{breaker.key}</span>
                      </td>
                      <td className="px-4 py-3">
                        <span
                          className={`inline-flex items-center gap-1.5 rounded border px-2 py-0.5 text-[12px] ${chip.tone}`}
                        >
                          <Icon className="h-3.5 w-3.5" aria-hidden="true" />
                          {chip.label}
                        </span>
                        {breaker.retry_after !== null ? (
                          <span className="ml-2 text-[12px] text-muted">
                            probe in {breaker.retry_after}s
                          </span>
                        ) : breaker.forced_open ? (
                          <span className="ml-2 text-[12px] text-muted">no probe scheduled</span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3 tabular-nums">
                        {breaker.failures_in_window}
                        <span className="text-muted">
                          {" "}
                          in {breaker.window_seconds}s
                        </span>
                        {breaker.state === "half_open" ? (
                          <span className="ml-2 text-[12px] text-amber-700">
                            {breaker.successes_in_half_open}/{breaker.success_threshold} probes
                            succeeded
                          </span>
                        ) : null}
                      </td>
                      <td className="px-4 py-3 tabular-nums text-muted">
                        {breaker.failure_threshold} · {breaker.cooldown_seconds}s cooldown
                      </td>
                      <td className="px-4 py-3 tabular-nums">{breaker.trips_total}</td>
                      <td className="px-4 py-3 text-right">
                        <span className="inline-flex gap-1.5">
                          <button
                            type="button"
                            onClick={() => openEditor(breaker)}
                            className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            Thresholds
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setConfirm({ breaker, mode: "reset" });
                              setReason("");
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            <RotateCcw className="h-3.5 w-3.5" aria-hidden="true" />
                            Reset
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setConfirm({ breaker, mode: "force-open" });
                              setReason("");
                            }}
                            className="rounded-md border border-line px-2.5 py-1 text-[12.5px]"
                          >
                            Force open
                          </button>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="rounded-lg border border-line">
        <header className="border-b border-line px-4 py-3">
          <h2 className="text-[14px] font-medium">Recent transitions</h2>
          <p className="text-[12px] text-muted">
            Every state change and every manual action, so "we fixed it, close it" and "the provider
            tripped" stay tellable apart.
          </p>
        </header>
        {!data || data.recent_events.length === 0 ? (
          <EmptyState
            title="No transitions yet"
            hint="A healthy provider writes nothing here — this log is for the moments something moved."
          />
        ) : (
          <ul className="divide-y divide-line">
            {data.recent_events.map((event) => (
              <li key={event.id} className="flex flex-wrap items-baseline gap-2 px-4 py-2.5 text-[13px]">
                <span className="font-medium">{event.key}</span>
                <span className="text-muted">
                  {event.from_state} → {event.to_state}
                </span>
                {event.reason ? <span className="text-muted">· {event.reason}</span> : null}
                <span className="ml-auto text-[12px] text-muted">
                  {new Date(event.created_at).toLocaleString()}
                </span>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* Confirmation for both manual actions. A reason is required by the API and typed here. */}
      {confirm ? (
        <div className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-0 sm:items-center sm:p-6">
          <div
            role="dialog"
            aria-label={confirm.mode === "reset" ? "Reset breaker" : "Force open breaker"}
            className="w-full max-w-md rounded-t-xl border border-line bg-panel p-5 sm:rounded-xl"
          >
            <div className="flex items-start justify-between gap-3">
              <h3 className="text-[15px] font-medium">
                {confirm.mode === "reset" ? "Reset" : "Force open"}{" "}
                <span className="font-normal text-muted">{confirm.breaker.key}</span>
              </h3>
              <button
                type="button"
                onClick={() => setConfirm(null)}
                aria-label="Close"
                className="rounded-md border border-line p-1"
              >
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>
            <p className="mt-2 text-[13px] text-muted">
              {confirm.mode === "reset"
                ? "The platform resumes calling this provider and clears the forced flag."
                : "The platform stops calling this provider. The flag outranks the cooldown, so it stays refused until somebody resets it."}
            </p>
            <label className="mt-3 block text-[12.5px] text-muted">
              Reason (recorded in the timeline)
              <input
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                placeholder="e.g. provider recovered, 14:20"
                className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
              />
            </label>
            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setConfirm(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={busy || reason.trim().length === 0}
                onClick={() => void runConfirm()}
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-60"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Save className="h-3.5 w-3.5" aria-hidden="true" />
                )}
                {confirm.mode === "reset" ? "Reset it" : "Drain it"}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {/* Threshold editor. */}
      {editing ? (
        <div className="fixed inset-0 z-50 flex items-end justify-center bg-black/40 p-0 sm:items-center sm:p-6">
          <div
            role="dialog"
            aria-label="Edit breaker thresholds"
            className="max-h-[90vh] w-full max-w-lg overflow-y-auto rounded-t-xl border border-line bg-panel p-5 sm:rounded-xl"
          >
            <div className="flex items-start justify-between gap-3">
              <div>
                <h3 className="text-[15px] font-medium">Thresholds</h3>
                <p className="text-[12.5px] text-muted">{editing.key}</p>
              </div>
              <button
                type="button"
                onClick={() => setEditing(null)}
                aria-label="Close"
                className="rounded-md border border-line p-1"
              >
                <X className="h-4 w-4" aria-hidden="true" />
              </button>
            </div>

            <div className="mt-4 grid gap-3 sm:grid-cols-2">
              {(
                [
                  ["name", "Display name", "text"],
                  ["failure_threshold", "Failure threshold", "number"],
                  ["window_seconds", "Window (s)", "number"],
                  ["cooldown_seconds", "Cooldown (s)", "number"],
                  ["half_open_probes", "Half-open probes", "number"],
                  ["success_threshold", "Successes to close", "number"],
                ] as const
              ).map(([key, label, type]) => (
                <label key={key} className="text-[12.5px] text-muted">
                  {label}
                  <input
                    type={type}
                    value={draft[key]}
                    onChange={(event) =>
                      setDraft({ ...draft, [key]: type === "number" ? Number(event.target.value) : event.target.value })
                    }
                    className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px] text-ink"
                  />
                </label>
              ))}
            </div>

            <p className="mt-3 flex items-start gap-1.5 text-[12px] text-muted">
              <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
              <span>
                Entering half-open <strong>resets</strong> the success counter, so the probe that
                opens the window is not one of the successes that close it. A threshold of two needs
                two successes <em>after</em> the probe.
              </span>
            </p>

            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setEditing(null)}
                className="rounded-md border border-line px-3 py-1.5 text-[13px]"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={busy}
                onClick={() => void saveThresholds()}
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-60"
              >
                {busy ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <Save className="h-3.5 w-3.5" aria-hidden="true" />
                )}
                Save thresholds
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
