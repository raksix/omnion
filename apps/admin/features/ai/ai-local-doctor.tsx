"use client";

/**
 * `/ai/local/doctor` — can this machine run AI by itself? (REQ-106, slice 4)
 *
 * The screen is a **list of checks**, not a boolean, because an operator who cannot run local AI
 * has four different faults behind one symptom: the server is down, it has no model, the model
 * will not answer, or the air-gap switch is what is refusing. Four faults, four fixes, four
 * support tickets — and one "not ready" tells the reader none of them.
 *
 * Five decisions shape the rendering, and each exists because the naive version states something
 * the platform does not know:
 *
 * 1. **`warn` is drawn as its own state, never as a soft pass.** It means "we did not establish
 *    this". A tick there is a claim nobody measured, and this screen's whole value is that its
 *    verdicts can be repeated in front of a customer.
 *
 * 2. **The summary is the server's sentence, quoted verbatim.** Recomputing a headline in the
 *    browser is how a green banner ends up above a failed row — the one visual contradiction
 *    this screen can never ship.
 *
 * 3. **The fix sits under the check it belongs to.** A separate help page puts the answer three
 *    clicks from the failure and makes the reader decide which failure they have.
 *
 * 4. **"Never run" is an empty state, not a verdict.** A doctor that has never executed has
 *    established nothing, and rendering an empty list of green ticks would be the most expensive
 *    possible first impression.
 *
 * 5. **History sits beside the last run, not behind a tab.** A regression is a *comparison*, so
 *    a screen that shows one run cannot show one.
 *
 * Keyboard: `D` runs every check, `R` re-runs the focused check, `/` focuses the filter — each
 * ignored while a text field has focus, so typing "doctor" into the search box cannot start
 * eight doctor runs.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  CircleHelp,
  History,
  Loader2,
  Play,
  RefreshCw,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ApiError } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import {
  fetchDoctor,
  rerunDoctorCheck,
  runDoctor,
  type CheckStatus,
  type DoctorCheck,
  type DoctorList,
  type DoctorRun,
  type RunStatus,
} from "@/lib/local-api";

/**
 * How one status renders.
 *
 * A **word** first and an icon second, for all three states. Colour alone is the failure this
 * screen cannot afford: `warn` and `pass` in one palette are distinguishable by a colour-blind
 * reader only by reading the sentence underneath, and one of the two has to be legible first.
 */
function tone(status: CheckStatus): { label: string; className: string } {
  switch (status) {
    case "pass":
      return { label: "Passed", className: "bg-positive-soft text-positive" };
    case "fail":
      return { label: "Failed", className: "bg-danger/10 text-danger" };
    default:
      // `warn` gets the caution palette AND its own word. It is not "pending" and not "passed".
      return { label: "Not established", className: "bg-caution-soft text-caution" };
  }
}

/** How a run's verdict renders. */
function runTone(status: RunStatus): { label: string; className: string } {
  switch (status) {
    case "passed":
      return { label: "Ready", className: "bg-positive-soft text-positive" };
    case "failed":
      return { label: "Not ready", className: "bg-danger/10 text-danger" };
    default:
      return { label: "Unproven", className: "bg-caution-soft text-caution" };
  }
}

/** Counts for the header, straight off the run's own checks. */
function tally(checks: DoctorCheck[]): { passed: number; warned: number; failed: number } {
  return checks.reduce(
    (counts, check) => {
      if (check.status === "pass") counts.passed += 1;
      else if (check.status === "fail") counts.failed += 1;
      else counts.warned += 1;
      return counts;
    },
    { passed: 0, warned: 0, failed: 0 },
  );
}

function messageOf(cause: unknown, fallback: string): string {
  return cause instanceof ApiError ? cause.message : fallback;
}

export function AiLocalDoctorView() {
  const [data, setData] = useState<DoctorList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [running, setRunning] = useState(false);
  const [rerunning, setRerunning] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchDoctor()
      .then(setData)
      .catch((cause: unknown) => {
        setData(null);
        setError(messageOf(cause, "The doctor's last run could not be loaded."));
      })
      .finally(() => setBusy(false));
  }, []);

  useEffect(load, [load]);

  const latest = data?.latest ?? null;

  /**
   * Run every check.
   *
   * The notice is set **after** the awaits and the screen is re-read first, because a load that
   * resolves later would otherwise wipe a message the operator is still reading. That ordering
   * was a real defect once: the button worked and printed nothing, because the notice was set
   * before a fetch whose promise resolved afterwards and cleared it.
   */
  const runAll = useCallback(async () => {
    setRunning(true);
    setNotice(null);
    try {
      const run: DoctorRun = await runDoctor();
      await fetchDoctor().then(setData).catch(() => undefined);
      setNotice(
        run.status === "passed"
          ? "Every check passed."
          : `Run #${run.id}: ${runTone(run.status).label.toLowerCase()} — ${run.summary}`,
      );
    } catch (cause) {
      setNotice(messageOf(cause, "The doctor could not be run."));
    } finally {
      setRunning(false);
    }
  }, []);

  const rerun = useCallback(async (check: DoctorCheck) => {
    setRerunning(check.key);
    setNotice(null);
    try {
      const run = await rerunDoctorCheck(check.key);
      // The whole list comes back, and the whole list is what replaces the screen: a client that
      // patched in one row would be showing a summary beside checks nobody re-ran.
      setData((current) => ({
        previous: [],
        never_run: false,
        airgap_enabled: run.airgap_enabled,
        ...(current ?? {}),
        latest: run,
      }));
      const again = run.checks.find((row) => row.key === check.key);
      setNotice(
        again
          ? `${again.label} is now "${tone(again.status).label.toLowerCase()}".`
          : `${runTone(run.status).label}: ${run.summary}`,
      );
    } catch (cause) {
      setNotice(messageOf(cause, "That check could not be re-run."));
    } finally {
      setRerunning(null);
    }
  }, []);

  /**
   * Keyboard: `D` runs everything, `R` re-runs the focused row, `/` focuses the filter.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable === true;
      if (typing) return;

      if (event.key === "/") {
        event.preventDefault();
        document.querySelector<HTMLInputElement>("[data-doctor-filter]")?.focus();
        return;
      }
      if (event.key === "d" || event.key === "D") {
        if (running) return;
        event.preventDefault();
        void runAll();
        return;
      }
      if (event.key === "r" || event.key === "R") {
        const focused = document.querySelector<HTMLElement>("[data-doctor-check]:focus");
        const key = focused?.dataset.doctorCheck;
        if (!key || rerunning) return;
        const row = latest?.checks.find((check) => check.key === key);
        if (row) {
          event.preventDefault();
          void rerun(row);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [latest, rerunning, runAll, rerun]);

  const visible = useMemo(() => {
    const rows = latest?.checks ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter(
      (check) =>
        check.label.toLowerCase().includes(needle) ||
        check.key.toLowerCase().includes(needle) ||
        (check.endpoint ?? "").toLowerCase().includes(needle),
    );
  }, [latest, filter]);

  const counts = useMemo(() => tally(latest?.checks ?? []), [latest]);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => void runAll()}
          disabled={running}
          data-doctor-run-all
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
        >
          {running ? (
            <Loader2 aria-hidden size={14} className="animate-spin" />
          ) : (
            <Play aria-hidden size={14} />
          )}
          Run all checks
        </button>
        <button
          type="button"
          onClick={load}
          disabled={busy}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-60"
        >
          {busy ? (
            <Loader2 aria-hidden size={14} className="animate-spin" />
          ) : (
            <RefreshCw aria-hidden size={14} />
          )}
          Refresh
        </button>
        <input
          data-doctor-filter
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          placeholder="Filter checks  ( / )"
          aria-label="Filter checks"
          className="ml-auto rounded-lg border border-line bg-canvas px-3 py-1.5 text-[12px] text-ink"
        />
      </div>

      {notice ? (
        <p
          data-doctor-notice
          role="status"
          className="rounded-xl border border-line bg-surface px-3.5 py-2.5 text-[12.5px] text-ink"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          className="flex flex-wrap items-center gap-3 rounded-xl border border-danger/40 bg-danger/5 px-3.5 py-3 text-[12.5px] text-danger"
        >
          <AlertTriangle aria-hidden size={15} />
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={load}
            className="rounded-lg border border-danger/40 px-2.5 py-1 text-[12px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      {busy && !latest && !error ? (
        <p className="flex items-center gap-2 text-[12.5px] text-muted" data-doctor-loading>
          <Loader2 aria-hidden size={14} className="animate-spin" />
          Loading the last run…
        </p>
      ) : null}

      {data?.never_run ? (
        <EmptyState
          title="The doctor has not run yet"
          hint="Running it checks every local endpoint, asks one real completion, and reports whether this machine can serve inference with no internet at all. Nothing has been verified."
          action={
            <button
              type="button"
              onClick={() => void runAll()}
              disabled={running}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
            >
              {running ? (
                <Loader2 aria-hidden size={14} className="animate-spin" />
              ) : (
                <Play aria-hidden size={14} />
              )}
              Run all checks
            </button>
          }
        />
      ) : null}

      {latest ? (
        <section className="flex flex-col gap-3" data-doctor-latest>
          <div className="flex flex-wrap items-center gap-2 rounded-xl border border-line bg-surface px-4 py-3">
            {(() => {
              const badge = runTone(latest.status);
              return (
                <span
                  data-doctor-verdict
                  className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${badge.className}`}
                >
                  {latest.status === "passed" ? (
                    <CheckCircle2 aria-hidden size={11} />
                  ) : latest.status === "failed" ? (
                    <AlertTriangle aria-hidden size={11} />
                  ) : (
                    <CircleHelp aria-hidden size={11} />
                  )}
                  {badge.label}
                </span>
              );
            })()}
            {/* The server's own sentence, quoted. Recomputing a headline here is how a green
                banner ends up above a failed row. */}
            <p data-doctor-summary className="flex-1 text-[12.5px] text-ink">
              {latest.summary}
            </p>
            <span className="text-[11.5px] text-muted">
              {counts.passed} passed · {counts.warned} not established · {counts.failed} failed
            </span>
          </div>

          <p className="text-[11.5px] text-muted">
            Run #{latest.id} · {formatTimestamp(latest.started_at)}
            {latest.elapsed_ms !== null ? ` · ${latest.elapsed_ms} ms` : ""} · air gap{" "}
            {latest.airgap_enabled ? "was on" : "was off"} for this run
          </p>

          {visible.length === 0 ? (
            <EmptyState
              title="No check matches that filter"
              hint={`This run produced ${latest.checks.length} check(s). Clear the filter to see them.`}
              action={
                <button
                  type="button"
                  onClick={() => setFilter("")}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12px]"
                >
                  Clear filter
                </button>
              }
            />
          ) : (
            <ul className="flex flex-col gap-2">
              {visible.map((check) => {
                const badge = tone(check.status);
                return (
                  <li
                    key={`${check.key}-${check.endpoint ?? ""}`}
                    data-doctor-check
                    data-doctor-check-key={check.key}
                    data-doctor-status={check.status}
                    tabIndex={0}
                    className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4 focus:outline-none focus:ring-2 focus:ring-accent/40"
                  >
                    <div className="flex flex-wrap items-center gap-2">
                      <span
                        className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${badge.className}`}
                      >
                        {check.status === "pass" ? (
                          <CheckCircle2 aria-hidden size={11} />
                        ) : check.status === "fail" ? (
                          <AlertTriangle aria-hidden size={11} />
                        ) : (
                          <CircleHelp aria-hidden size={11} />
                        )}
                        {badge.label}
                      </span>
                      <span className="text-[13px] font-medium text-ink">{check.label}</span>
                      {check.endpoint ? (
                        <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                          {check.endpoint}
                        </span>
                      ) : null}
                      {check.latency_ms !== null && check.latency_ms !== undefined ? (
                        <span className="text-[11px] text-muted">{check.latency_ms} ms</span>
                      ) : null}
                      <button
                        type="button"
                        onClick={() => void rerun(check)}
                        disabled={rerunning !== null}
                        className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                      >
                        {rerunning === check.key ? (
                          <Loader2 aria-hidden size={12} className="animate-spin" />
                        ) : (
                          <RefreshCw aria-hidden size={12} />
                        )}
                        Re-run
                      </button>
                    </div>
                    <p className="text-[12.5px] text-muted">{check.detail}</p>
                    {check.fix ? (
                      <p className="rounded-lg bg-canvas px-3 py-2 text-[12px] text-ink">
                        <span className="font-medium">Fix: </span>
                        {check.fix}
                      </p>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          )}
        </section>
      ) : null}

      {data && data.previous.length > 0 ? (
        <section className="flex flex-col gap-2" data-doctor-history>
          <h2 className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
            <History aria-hidden size={14} className="text-muted" />
            Previous runs
          </h2>
          <ul className="flex flex-col gap-1.5">
            {data.previous.map((run) => {
              const badge = runTone(run.status);
              return (
                <li
                  key={run.id}
                  data-doctor-history-row
                  className="flex flex-wrap items-center gap-2 rounded-lg border border-line bg-surface px-3 py-2 text-[12px]"
                >
                  <span
                    className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${badge.className}`}
                  >
                    {badge.label}
                  </span>
                  <span className="text-muted">Run #{run.id}</span>
                  <span className="text-muted">{formatTimestamp(run.started_at)}</span>
                  {/* The one-line cause, so a regression is legible without opening the run. */}
                  <span className="flex-1 text-muted">{run.summary}</span>
                </li>
              );
            })}
          </ul>
        </section>
      ) : null}
    </div>
  );
}