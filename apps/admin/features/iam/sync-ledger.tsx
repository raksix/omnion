"use client";

/**
 * The sync ledger (REQ-065, slice 4 part 2) — what the last sweeps actually did.
 *
 * The provider list already answers "is this reachable right now", which is a different question
 * from the one an operator has at 09:00: *did the nightly run work, and if not, for whom?* So this
 * is a separate tab rather than another column, and it is built around three sentences the panel
 * has to be able to say honestly:
 *
 * * **"Three accounts were refused."** A `partial` run is amber, never green. It created forty
 *   accounts and refused three people, and a chip that follows the counters hides the only part
 *   that needed a person.
 * * **"Alice failed twice."** The drawer keeps the attempts and collapses the *subject*, because
 *   one row per retry is not an action anybody can take and one row per person is.
 * * **"This run is still going."** A run in progress reports no duration at all. Rendering "time
 *   so far" makes a slow sweep look permanently unfinished and a fast one look like it never
 *   ended.
 *
 * The retry button names its subjects and never sends an empty list — "retry everything that
 * failed" and "retry Alice" are different acts, and a button that silently does the first when the
 * operator clicked the second is how a colleague gets a second failure at 03:00.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  ChevronRight,
  Clock3,
  FolderTree,
  Loader2,
  RefreshCw,
  RotateCcw,
  Users,
  XCircle,
} from "lucide-react";

import {
  ApiError,
  fetchIamSyncGroups,
  fetchIamSyncRun,
  fetchIamSyncRuns,
  retryIamSyncRun,
  type IamFailedSubject,
  type IamSyncRun,
} from "@/lib/api";

/** One run plus the subjects it could not process. */
type Drawer = {
  run: IamSyncRun;
  attempts: number;
  subjects: IamFailedSubject[];
  /** Which subjects are ticked. Default all — the common case is "retry the rest". */
  picked: Set<string>;
};

/** `2m 14s`, or a dash when there is no end. Never "time so far". */
function duration(run: IamSyncRun): string {
  if (run.duration_seconds === null) return "—";
  const seconds = run.duration_seconds;
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  if (minutes < 60) return `${minutes}m ${String(rest).padStart(2, "0")}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, "0")}m`;
}

/** A short local timestamp; the panel never renders a raw ISO string to a person. */
function when(stamp: string): string {
  const moment = new Date(stamp);
  if (Number.isNaN(moment.getTime())) return stamp;
  return moment.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The chip's sentence. `running` is neutral, not a problem. */
function verdict(run: IamSyncRun): { label: string; tone: string } {
  switch (run.status) {
    case "ok":
      return { label: "Completed", tone: "ok" };
    case "running":
      return { label: "Running", tone: "running" };
    case "partial":
      return {
        label: `${run.error_count} failed`,
        tone: "warn",
      };
    default:
      return { label: "Failed", tone: "bad" };
  }
}

export function SyncLedger({
  providerId,
  providerName,
  kind,
}: {
  providerId: string;
  providerName: string;
  kind: string;
}) {
  const [runs, setRuns] = useState<IamSyncRun[]>([]);
  const [summary, setSummary] = useState<{ runs: number; problems: number; running: number }>({
    runs: 0,
    problems: 0,
    running: 0,
  });
  const [interval, setInterval] = useState(0);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [problemsOnly, setProblemsOnly] = useState(false);
  const [drawer, setDrawer] = useState<Drawer | null>(null);
  const [drawerBusy, setDrawerBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [groups, setGroups] = useState<{
    external_id: string;
    external_label: string;
    member_count: number;
    last_seen_at: string;
    synced: boolean;
  }[]>([]);
  const [unsyncedGroups, setUnsyncedGroups] = useState(0);

  const load = useCallback(
    async (onlyProblems: boolean) => {
      setStatus("loading");
      setLoadError(null);
      try {
        const body = await fetchIamSyncRuns(providerId, { problemsOnly: onlyProblems });
        setRuns(body.runs);
        setSummary(body.summary);
        setInterval(body.sync_interval_minutes);
        setStatus("ready");
      } catch (cause) {
        setStatus("error");
        setLoadError(
          cause instanceof ApiError ? cause.message : "The sync history could not be read.",
        );
      }
    },
    [providerId],
  );

  const loadGroups = useCallback(async () => {
    try {
      const body = await fetchIamSyncGroups(providerId);
      setGroups(body.groups);
      setUnsyncedGroups(body.summary.unsynced);
    } catch {
      // The group table is a second question, not part of the ledger's answer. A failure here
      // must not blank the runs an operator came for.
      setGroups([]);
      setUnsyncedGroups(0);
    }
  }, [providerId]);

  useEffect(() => {
    void load(problemsOnly);
  }, [load, problemsOnly]);

  useEffect(() => {
    void loadGroups();
  }, [loadGroups]);

  const openDrawer = useCallback(
    async (runId: string) => {
      setError(null);
      try {
        const body = await fetchIamSyncRun(providerId, runId);
        setDrawer({
          run: body.run,
          attempts: body.attempts,
          subjects: body.failed_subjects,
          picked: new Set(body.failed_subjects.map((subject) => subject.key)),
        });
      } catch (cause) {
        setError(
          cause instanceof ApiError ? cause.message : "That run's failures could not be read.",
        );
      }
    },
    [providerId],
  );

  const toggle = (key: string) => {
    setDrawer((current) => {
      if (!current) return current;
      const picked = new Set(current.picked);
      if (picked.has(key)) picked.delete(key);
      else picked.add(key);
      return { ...current, picked };
    });
  };

  const retry = async () => {
    if (!drawer || drawer.picked.size === 0) return;
    setDrawerBusy(true);
    setError(null);
    try {
      const body = await retryIamSyncRun(providerId, drawer.run.id, [...drawer.picked]);
      setNotice(
        `A retry was recorded for ${body.subjects.length} subject${body.subjects.length === 1 ? "" : "s"}. The scheduler will run it.`,
      );
      setDrawer(null);
      await load(problemsOnly);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The retry could not be requested.");
    } finally {
      setDrawerBusy(false);
    }
  };

  const nextRun = useMemo(() => {
    if (interval <= 0 || runs.length === 0) return null;
    const last = runs[0].finished_at ?? runs[0].started_at;
    return new Date(new Date(last).getTime() + interval * 60_000);
  }, [interval, runs]);

  return (
    <section data-iam-sync-ledger={providerId} className="flex flex-col gap-4">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-[13px] font-medium">Sync history — {providerName}</h3>
          <p className="text-[12px] text-muted">
            {interval > 0
              ? `Every ${interval} minutes${nextRun ? ` · next around ${when(nextRun.toISOString())}` : ""}.`
              : "Not on a schedule — syncs are only started by hand."}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            data-sync-filter={problemsOnly ? "problems" : "all"}
            aria-pressed={problemsOnly}
            onClick={() => setProblemsOnly((current) => !current)}
            className={`flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-[12px] ${
              problemsOnly
                ? "border-caution bg-caution/10 text-caution"
                : "border-line bg-panel text-muted hover:text-ink"
            }`}
          >
            <AlertTriangle className="size-3" aria-hidden />
            {problemsOnly ? `Only problems (${summary.problems})` : "All runs"}
          </button>
          <button
            type="button"
            data-sync-reload={providerId}
            onClick={() => void load(problemsOnly)}
            className="flex items-center gap-1.5 rounded-md border border-line bg-panel px-2.5 py-1 text-[12px] text-muted hover:text-ink"
          >
            <RefreshCw className="size-3" aria-hidden />
            Refresh
          </button>
        </div>
      </header>

      {notice ? (
        <p
          data-sync-notice
          className="flex items-start gap-1.5 rounded-lg border border-emerald-500/40 bg-emerald-500/10 px-2.5 py-2 text-[12px] text-emerald-700"
        >
          <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          {notice}
        </p>
      ) : null}

      {error ? (
        <p
          data-sync-error
          role="alert"
          className="flex items-start gap-1.5 rounded-lg border border-caution bg-caution/10 px-2.5 py-2 text-[12px] text-caution"
        >
          <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          {error}
        </p>
      ) : null}

      {status === "loading" ? (
        <p className="flex items-center gap-2 text-[12px] text-muted" data-sync-loading>
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          Reading the sync history…
        </p>
      ) : status === "error" ? (
        <p className="rounded-lg border border-caution bg-caution/10 px-2.5 py-2 text-[12px] text-caution" data-sync-load-error>
          {loadError}
        </p>
      ) : runs.length === 0 ? (
        <p
          data-sync-empty
          className="rounded-lg border border-dashed border-line px-3 py-6 text-center text-[12px] text-muted"
        >
          {problemsOnly
            ? "No run has ended badly. Nothing to look at here."
            : "This provider has not synced yet. The first run appears here once it finishes."}
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[640px] text-left text-[12px]">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th className="py-1.5 pr-3 font-medium">Started</th>
                <th className="py-1.5 pr-3 font-medium">Kind</th>
                <th className="py-1.5 pr-3 font-medium">Outcome</th>
                <th className="py-1.5 pr-3 font-medium">Seen</th>
                <th className="py-1.5 pr-3 font-medium">C / U / D</th>
                <th className="py-1.5 pr-3 font-medium">Took</th>
                <th className="py-1.5 font-medium" />
              </tr>
            </thead>
            <tbody>
              {runs.map((run) => {
                const chip = verdict(run);
                return (
                  <tr
                    key={run.id}
                    data-sync-run={run.id}
                    data-sync-status={run.status}
                    className="border-b border-line/60 last:border-0"
                  >
                    <td className="py-2 pr-3 font-mono text-[11px] text-muted">{when(run.started_at)}</td>
                    <td className="py-2 pr-3">{run.kind}</td>
                    <td className="py-2 pr-3">
                      <span
                        data-sync-verdict={run.status}
                        className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 ${
                          chip.tone === "ok"
                            ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700"
                            : chip.tone === "running"
                              ? "border-line bg-panel text-muted"
                              : chip.tone === "warn"
                                ? "border-caution bg-caution/10 text-caution"
                                : "border-caution bg-caution/15 text-caution"
                        }`}
                      >
                        {run.status === "ok" ? (
                          <CheckCircle2 className="size-3" aria-hidden />
                        ) : run.status === "running" ? (
                          <Loader2 className="size-3 animate-spin" aria-hidden />
                        ) : (
                          <AlertTriangle className="size-3" aria-hidden />
                        )}
                        {chip.label}
                      </span>
                    </td>
                    <td className="py-2 pr-3 tabular-nums">{run.counts.users_seen}</td>
                    <td className="py-2 pr-3 tabular-nums text-muted">
                      {run.counts.users_created} / {run.counts.users_updated} /{" "}
                      {run.counts.users_deactivated}
                    </td>
                    <td className="py-2 pr-3 tabular-nums text-muted">
                      {run.status === "running" ? (
                        <span className="flex items-center gap-1">
                          <Clock3 className="size-3" aria-hidden />
                          still going
                        </span>
                      ) : (
                        duration(run)
                      )}
                    </td>
                    <td className="py-2 text-right">
                      <button
                        type="button"
                        data-sync-open={run.id}
                        onClick={() => void openDrawer(run.id)}
                        className="inline-flex items-center gap-1 rounded-md border border-line bg-panel px-2 py-1 text-[11px] text-muted hover:text-ink"
                      >
                        {run.status === "running" ? "Inspect" : "Failures"}
                        <ChevronRight className="size-3" aria-hidden />
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* The drawer. Failures are the only reason to open it, so it says so when there are none. */}
      {drawer ? (
        <div
          data-sync-drawer={drawer.run.id}
          className="flex flex-col gap-3 rounded-lg border border-line bg-panel/60 p-3"
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h4 className="text-[12px] font-medium">
              {drawer.subjects.length === 0
                ? "No failed subjects"
                : `${drawer.subjects.length} failed subject${drawer.subjects.length === 1 ? "" : "s"}`}
            </h4>
            <p className="text-[11px] text-muted">
              {/* The two numbers are different on purpose: every honest attempt including
                  repeats, beside the list an operator can act on. */}
              {drawer.attempts} attempt{drawer.attempts === 1 ? "" : "s"} recorded
            </p>
          </div>

          {drawer.subjects.length === 0 ? (
            <p className="text-[12px] text-muted">
              {drawer.run.status === "running"
                ? "This run has not finished, so it has no failures to show yet."
                : "Nothing failed in this run. Retry is not offered because there is nothing to retry."}
            </p>
          ) : (
            <>
              <ul className="flex flex-col gap-1.5">
                {drawer.subjects.map((subject) => (
                  <li
                    key={subject.key}
                    data-sync-subject={subject.key}
                    className="flex items-start gap-2 rounded-md border border-line/70 px-2.5 py-1.5"
                  >
                    <input
                      type="checkbox"
                      className="mt-0.5"
                      checked={drawer.picked.has(subject.key)}
                      onChange={() => toggle(subject.key)}
                      aria-label={`Retry ${subject.key}`}
                    />
                    <div className="flex min-w-0 flex-1 flex-col">
                      <span className="flex flex-wrap items-center gap-1.5">
                        <code className="truncate font-mono text-[11px]">{subject.key}</code>
                        <span className="rounded-full border border-line px-1.5 py-0.5 text-[10px] text-muted">
                          {subject.code}
                        </span>
                        {/* A repeat is the signal that the directory is flapping rather than that
                            one entry is wrong, and it is invisible if the rows are collapsed. */}
                        {subject.attempts > 1 ? (
                          <span
                            data-sync-attempts={subject.attempts}
                            className="rounded-full border border-caution bg-caution/10 px-1.5 py-0.5 text-[10px] text-caution"
                          >
                            {subject.attempts} attempts
                          </span>
                        ) : null}
                      </span>
                      <span className="text-[11px] text-muted">{subject.message}</span>
                    </div>
                  </li>
                ))}
              </ul>

              <div className="flex items-center justify-end gap-2">
                <button
                  type="button"
                  onClick={() => setDrawer(null)}
                  className="rounded-md border border-line bg-panel px-2.5 py-1 text-[12px] text-muted hover:text-ink"
                >
                  Close
                </button>
                <button
                  type="button"
                  data-sync-retry={drawer.run.id}
                  disabled={drawer.picked.size === 0 || drawerBusy || drawer.run.status === "running"}
                  onClick={() => void retry()}
                  className="flex items-center gap-1.5 rounded-md border border-caution bg-caution/10 px-2.5 py-1 text-[12px] text-caution disabled:cursor-not-allowed disabled:opacity-50"
                >
                  {drawerBusy ? (
                    <Loader2 className="size-3 animate-spin" aria-hidden />
                  ) : (
                    <RotateCcw className="size-3" aria-hidden />
                  )}
                  Retry {drawer.picked.size} selected
                </button>
              </div>
            </>
          )}
        </div>
      ) : null}

      {/* The groups a sync has seen. A group whose membership could not be read is shown with a
          warning rather than dropped, so a pending repair does not look like a vanished group. */}
      {kind === "ldap" || kind === "active_directory" ? (
        <div className="flex flex-col gap-2">
          <h4 className="flex items-center gap-1.5 text-[12px] font-medium">
            <FolderTree className="size-3.5" aria-hidden />
            Directory groups
            {unsyncedGroups > 0 ? (
              <span className="rounded-full border border-caution bg-caution/10 px-1.5 py-0.5 text-[10px] text-caution">
                {unsyncedGroups} unread
              </span>
            ) : null}
          </h4>
          {groups.length === 0 ? (
            <p className="text-[12px] text-muted" data-sync-groups-empty>
              No groups recorded yet. They appear here after the first sync that reads them.
            </p>
          ) : (
            <ul className="flex flex-wrap gap-1.5">
              {groups.map((group) => (
                <li
                  key={group.external_id}
                  data-sync-group={group.external_id}
                  className={`flex items-center gap-1.5 rounded-md border px-2 py-1 text-[11px] ${
                    group.synced
                      ? "border-line bg-panel text-muted"
                      : "border-caution bg-caution/10 text-caution"
                  }`}
                >
                  <Users className="size-3" aria-hidden />
                  <span className="font-medium">{group.external_label || group.external_id}</span>
                  <span className="tabular-nums">{group.member_count}</span>
                  {!group.synced ? (
                    <span className="rounded-full border border-caution px-1.5 py-0.5 text-[10px]">
                      membership unread
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : null}
    </section>
  );
}
