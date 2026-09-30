"use client";

/**
 * `/automation/projects/{id}/limits` — the caps, the usage bars and the export (REQ-133, slice 4).
 *
 * The screen exists because three of the REQ's sentences are about *consequences* that nothing
 * displayed: a limit that is enforced and never shown is a rule a person discovers by being
 * refused. So the whole payload is one read — the caps, today's counters, the series and the
 * warnings — and the bars are drawn from it.
 *
 * Five decisions the screen keeps, each of which is a place where a reasonable-looking client
 * would be wrong:
 *
 * 1. **The warning is rendered from `warnings`, not recomputed.** The 80 percent threshold is a
 *    rule the store owns; a panel that re-implemented it would be a second copy of a threshold,
 *    and the copy that drifts is the one nobody tests. If the API did not warn, the bar is plain.
 * 2. **A `0` cap renders as "Unlimited", never as an empty bar.** An empty bar next to an
 *    unlabelled `0` is how a project stops working and nobody knows the cap is not the reason.
 * 3. **`max_credentials` has no bar on this branch.** There is no `credentials` table, so a bar
 *    for it would be a number nobody can act on — the input is still there (the column exists and
 *    the cap is real), and the missing counter is said out loud rather than drawn as zero.
 * 4. **The CSV is built from the series already on screen.** An export that re-queried could
 *    disagree with the bars by one run; the REQ asks for the export to *reproduce* the series, so
 *    it is literally the same array.
 * 5. **The warning links to the owner, because the API's refusal names the owner.** A bar that
 *    says "80 percent" sends the reader hunting; the refusal message they will meet at 100
 *    percent says "ask <name>", and the screen had better agree.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useParams } from "next/navigation";
import { ArrowLeft, Download, Loader2, Save, TriangleAlert } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchProject,
  fetchProjectLimits,
  saveProjectLimits,
} from "@/lib/api";
import type { LimitName, Project, ProjectLimits, ProjectLimitsSave } from "@/lib/types";

/** One bar's worth of state, derived from the payload rather than stored. */
interface Bar {
  name: LimitName;
  label: string;
  current: number;
  limit: number;
  /** `null` when the cap is unlimited, which is a different state from "at zero". */
  percent: number | null;
  warning: boolean;
  exceeded: boolean;
}

/** The four caps, in the order the REQ lists them. */
const CAPS: { name: LimitName; label: string; measure: (body: ProjectLimits) => number }[] = [
  { name: "max_runs_per_day", label: "Runs today", measure: (body) => body.today.runs },
  { name: "max_concurrent_runs", label: "Running now", measure: (body) => body.concurrent_runs },
  { name: "max_workflows", label: "Workflows", measure: (body) => body.workflow_count },
  { name: "max_credentials", label: "Credentials", measure: () => 0 },
];

/**
 * The `max_credentials` bar has no counter on this branch.
 *
 * `credentials` ships with wave 7's REQ-099 runtime and there is no such table here, so the
 * current value is genuinely unknown rather than zero. The screen says so instead of drawing a
 * bar at 0% — a bar at zero is a claim, and a person who believes it will read "3 of 8" off a
 * project that has none of either.
 */
const COUNTERLESS: LimitName[] = ["max_credentials"];

function percentOf(current: number, limit: number): number | null {
  if (limit <= 0) return null;
  return Math.min(100, Math.round((current / limit) * 100));
}

export function ProjectLimitsScreen() {
  const params = useParams<{ id: string }>();
  const projectId = params?.id;

  const [project, setProject] = useState<Project | null>(null);
  const [body, setBody] = useState<ProjectLimits | null>(null);
  const [missing, setMissing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [form, setForm] = useState<Record<LimitName, string>>({
    max_workflows: "",
    max_credentials: "",
    max_runs_per_day: "",
    max_concurrent_runs: "",
  });
  const [warnAt, setWarnAt] = useState("80");

  const load = useCallback(async () => {
    if (!projectId) return;
    setError(null);
    setMissing(false);
    setSaved(false);
    try {
      // The project's own row and its limits are two reads because they answer two questions; the
      // second one failing must not blank a screen whose first half loaded.
      const [detail, limits] = await Promise.all([
        fetchProject(projectId),
        fetchProjectLimits(projectId),
      ]);
      setProject(detail.project);
      setBody(limits);
      setForm({
        max_workflows: String(limits.max_workflows),
        max_credentials: String(limits.max_credentials),
        max_runs_per_day: String(limits.max_runs_per_day),
        max_concurrent_runs: String(limits.max_concurrent_runs),
      });
      setWarnAt(String(limits.warn_at_percent));
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 404) {
        setMissing(true);
        return;
      }
      setError(cause instanceof ApiError ? cause.message : "the limits could not be read");
    }
  }, [projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  const bars: Bar[] = useMemo(() => {
    if (!body) return [];
    return CAPS.map((cap) => {
      const current = cap.measure(body);
      const limit = body[cap.name];
      const reading = body.warnings[cap.name];
      return {
        name: cap.name,
        label: cap.label,
        current,
        limit,
        percent: percentOf(current, limit),
        // Straight from the server's own predicate. `?? false` is deliberate: a bar the API said
        // nothing about is not a bar in warning.
        warning: reading?.warn_at_percent !== undefined && current > 0 && !!reading,
        exceeded: reading?.exceeded ?? false,
      };
    });
  }, [body]);

  const dirty =
    !body ||
    warnAt !== String(body.warn_at_percent) ||
    CAPS.some((cap) => Number(form[cap.name] || 0) !== body[cap.name]);

  const save = async () => {
    if (!body) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    const payload: ProjectLimitsSave = {
      warn_at_percent: Number(warnAt),
      max_workflows: Number(form.max_workflows || 0),
      max_credentials: Number(form.max_credentials || 0),
      max_runs_per_day: Number(form.max_runs_per_day || 0),
      max_concurrent_runs: Number(form.max_concurrent_runs || 0),
    };
    try {
      const saved_ = await saveProjectLimits(body.project_id, payload);
      setBody(saved_);
      setSaved(true);
    } catch (cause) {
      // The API's refusal names the field ("max_workflows cannot be negative…"), so it is shown
      // verbatim rather than replaced with a generic sentence that loses the field name.
      setError(cause instanceof ApiError ? cause.message : "the limits could not be saved");
    } finally {
      setBusy(false);
    }
  };

  /**
   * The CSV, built from the series on screen.
   *
   * Not re-queried: the REQ asks the export to "reproduce the on-screen series", and a second
   * query would agree with the bars only until the next run landed. Escaping quotes the same way
   * for every cell, because a project's key can hold anything and a comma inside a cell is how a
   * spreadsheet silently shifts every column after it.
   */
  const exportCsv = useCallback(() => {
    if (!body) return;
    const escape = (value: string | number) => {
      const text = String(value);
      return /[",\n]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
    };
    const rows = [
      ["date", "runs", "failures", "compute_ms"].map(escape).join(","),
      ...body.series.map((day) =>
        [day.usage_date, day.runs, day.failures, day.compute_ms].map(escape).join(","),
      ),
    ];
    const blob = new Blob([`${rows.join("\n")}\n`], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `${body.project_id.slice(0, 8)}-usage.csv`;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    URL.revokeObjectURL(url);
  }, [body]);

  if (missing) {
    return (
      <EmptyState
        title="No such project"
        hint="It may have been deleted, or it belongs to a team you are not a member of. Both look the same from here on purpose."
        action={
          <Link href="/automation/projects" className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]">
            Back to projects
          </Link>
        }
      />
    );
  }

  if (error && !body) {
    return (
      <div
        role="alert"
        className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
      >
        <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
        <span className="flex-1">{error}</span>
        <button type="button" onClick={() => void load()} className="rounded-lg border border-line px-2 py-1 text-[11.5px]">
          Retry
        </button>
      </div>
    );
  }

  if (!body || !project) {
    return <LoadingTable columns={2} rows={4} />;
  }

  const archived = project.status === "archived";
  const peak = body.series.reduce((max, day) => Math.max(max, day.runs), 0);

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <Link
          href={`/automation/projects/${project.id}`}
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft className="size-3.5" aria-hidden />
          {project.key} — {project.name}
        </Link>
        <button
          type="button"
          data-limits-export
          onClick={exportCsv}
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink"
        >
          <Download className="size-3.5" aria-hidden />
          Export CSV
        </button>
      </div>

      {error ? (
        <div role="alert" data-limits-error className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
        </div>
      ) : null}

      {archived ? (
        <p className="rounded-lg border border-line bg-quiet-soft px-3 py-2.5 text-[12.5px] text-muted">
          This project is archived, so its limits are read-only. Restore it to change them.
        </p>
      ) : null}

      <section className="flex flex-col gap-3">
        <h2 className="text-[13.5px] font-medium">Usage</h2>
        <div className="grid gap-3 sm:grid-cols-2">
          {bars.map((bar) => {
            const counterless = COUNTERLESS.includes(bar.name);
            return (
              <div
                key={bar.name}
                data-limit-bar={bar.name}
                data-limit-state={counterless ? "unknown" : bar.exceeded ? "exceeded" : bar.warning ? "warning" : "ok"}
                className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface px-3 py-2.5"
              >
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-[12.5px]">{bar.label}</span>
                  <span className="text-[11.5px] text-muted">
                    {counterless ? (
                      "no counter on this build"
                    ) : bar.percent === null ? (
                      "Unlimited"
                    ) : (
                      <>
                        {bar.current} of {bar.limit}
                      </>
                    )}
                  </span>
                </div>
                {bar.percent === null || counterless ? (
                  // An unlimited cap gets a full-width quiet rule rather than an empty track: an
                  // empty bar reads as "0 of something", which is a claim this state does not make.
                  <div className="h-1.5 w-full rounded-full bg-line" data-limit-unlimited />
                ) : (
                  <div className="h-1.5 w-full overflow-hidden rounded-full bg-line">
                    <div
                      className={
                        bar.exceeded
                          ? "h-full rounded-full bg-red-500"
                          : bar.warning
                            ? "h-full rounded-full bg-amber-500"
                            : "h-full rounded-full bg-accent"
                      }
                      style={{ width: `${Math.max(2, bar.percent)}%` }}
                    />
                  </div>
                )}
                {bar.exceeded ? (
                  <p className="text-[11.5px] text-red-600" data-limit-exceeded={bar.name}>
                    Over the cap. The engine refuses new runs and names{" "}
                    {project.owner_user_id ? "the project owner" : "nobody — the project has no owner"} in
                    the message.
                  </p>
                ) : null}
                {bar.warning && !bar.exceeded ? (
                  <p className="text-[11.5px] text-amber-700" data-limit-warning={bar.name}>
                    {bar.current} of {bar.limit} — {bar.percent}% of the cap.{" "}
                    {project.owner_user_id ? (
                      <span>Raising it is the owner&apos;s call.</span>
                    ) : (
                      <span>This project has no owner to ask.</span>
                    )}
                  </p>
                ) : null}
              </div>
            );
          })}
        </div>
        <p className="text-[11.5px] text-muted">
          A cap of <strong>0</strong> means unlimited, not none — a fresh project is born with every
          cap at 0 so that a new installation can start a run. The counters reset at midnight in
          the database&apos;s timezone.
        </p>
      </section>

      <section className="flex flex-col gap-3">
        <h2 className="text-[13.5px] font-medium">Overrides</h2>
        <div className="grid gap-3 sm:grid-cols-2">
          {CAPS.map((cap) => (
            <label key={cap.name} className="flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">
                {cap.label} — 0 for unlimited
              </span>
              <input
                value={form[cap.name]}
                data-limit-input={cap.name}
                inputMode="numeric"
                disabled={archived || busy}
                onChange={(event) => setForm({ ...form, [cap.name]: event.target.value })}
                className="rounded-lg border border-line px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
              />
            </label>
          ))}
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Warn at (percent)</span>
            <input
              value={warnAt}
              data-limit-warn
              inputMode="numeric"
              disabled={archived || busy}
              onChange={(event) => setWarnAt(event.target.value)}
              className="rounded-lg border border-line px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
            />
          </label>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-limits-save
            disabled={busy || archived || !dirty}
            onClick={() => void save()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-60"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
            Save limits
          </button>
          {saved && !dirty ? <span className="text-[11.5px] text-muted">Saved</span> : null}
          {dirty ? <span className="text-[11.5px] text-muted">Unsaved changes</span> : null}
        </div>
      </section>

      <section className="flex flex-col gap-2">
        <div className="flex items-center gap-2">
          <h2 className="text-[13.5px] font-medium">Daily usage</h2>
          <span className="text-[11.5px] text-muted">
            {body.series.length} day{body.series.length === 1 ? "" : "s"} on record
            {peak > 0 ? ` · peak ${peak} runs` : ""}
          </span>
        </div>
        {body.series.length === 0 ? (
          <p data-limits-series-empty className="text-[12px] text-muted">
            Nothing has run in this project yet. The counters start when the first run does, and a
            project with no series is not a project with no runs today — today&apos;s counters are{" "}
            {body.today.runs}.
          </p>
        ) : (
          <div className="overflow-x-auto rounded-xl border border-line bg-surface p-3">
            <div className="flex min-w-[18rem] items-end gap-1" data-limits-series>
              {body.series.map((day) => (
                <div
                  key={day.usage_date}
                  data-limit-day={day.usage_date}
                  title={`${day.usage_date}: ${day.runs} runs, ${day.failures} failed`}
                  className="flex flex-1 flex-col items-center gap-1"
                >
                  <div
                    className={
                      day.failures > 0
                        ? "w-full rounded-t bg-amber-500/70"
                        : "w-full rounded-t bg-accent/70"
                    }
                    style={{ height: `${Math.max(3, Math.round((day.runs / Math.max(1, peak)) * 60))}px` }}
                  />
                  <span className="text-[10px] text-muted">{day.usage_date.slice(5)}</span>
                </div>
              ))}
            </div>
          </div>
        )}
      </section>
    </div>
  );
}
