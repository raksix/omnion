"use client";

/**
 * The automation **operations** screens (docs/requests/REQ-003, slice 4).
 *
 * Three things an operator reaches for when a rule is not doing what its author expected,
 * and the one control that acts on what they find:
 *
 * * **Run history** (`RunsPanel`) — every run of a rule, and from any row the **trace**: the
 *   step-by-step record with attempts used against attempts allowed, which is one of the two
 *   acceptance criteria no unit test can prove because they are about what a human *sees*.
 *   The loop guard's message lives on this trace too, so "why did my rule stop?" is answered
 *   by reading the run rather than by reading the source.
 * * **Versions** (`VersionsPanel`) — what the rule looked like on every write, what each write
 *   changed in words, and **Restore**. A restore appends rather than rewinds, so the history
 *   stays a line.
 * * **Audit** (`AuditPanel`) — who changed what, and when, read from the trail every
 *   privileged write already records.
 *
 * The **templates gallery** (`AutomationTemplatesView`) is a separate screen rather than a
 * tab, because a template is not a state a rule is in — it is a definition you install, and
 * installing it is an ordinary create.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, ArrowLeft, History, RotateCcw, ScrollText } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { useSession } from "@/lib/session";
import {
  ApiError,
  cancelAutomationRun,
  createAutomation,
  fetchAutomationAudit,
  fetchAutomationRun,
  fetchAutomationRunHistory,
  fetchAutomationTemplates,
  fetchAutomationVersions,
  fetchOrganizations,
  restoreAutomationVersion,
  resumeAutomationFrom,
  type AutomationAuditEntry,
  type AutomationRunDetail,
  type AutomationRunSummary,
  type AutomationInput,
  type AutomationTemplate,
  type AutomationVersion,
  type AutomationVersionList,
  type AutomationVersionSummary,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The columns the run history lists. */
const RUN_COLUMNS = ["Started", "Status", "Trigger", "Steps", "Error"];

/** A short, human reason for a value the diff moved between. */
function describe(value: unknown): string {
  if (value === null || value === undefined) {
    return "not set";
  }
  if (typeof value === "boolean") {
    return value ? "on" : "off";
  }
  if (typeof value === "number") {
    return String(value);
  }
  if (typeof value === "string") {
    return value === "" ? "empty" : value;
  }
  // An object or an array is a count, not a dump: the panel has a whole editor for the
  // contents, and a diff line is not the place to render three levels of JSON.
  const size = Array.isArray(value)
    ? value.length
    : Object.keys(value as Record<string, unknown>).length;
  return `${size} ${Array.isArray(value) ? "items" : "field"}${size === 1 ? "" : "s"}`;
}

/** The label a diff line uses for a field. */
const FIELD_LABELS: Record<string, string> = {
  name: "Name",
  description: "Description",
  enabled: "Armed",
  site_id: "Site",
  trigger: "Trigger",
  conditions: "Conditions",
  actions: "Actions",
  on_error: "On error",
  run_as_user_id: "Runs as",
  rate_limit_per_hour: "Runs per hour",
  concurrency: "Concurrency",
};

/** One line of a version's diff, in the panel's own words. */
function ChangeLine({ field, from, to }: { field: string; from: unknown; to: unknown }) {
  return (
    <li className="flex flex-wrap items-baseline gap-x-2 text-[12.5px]">
      <span className="font-medium">{FIELD_LABELS[field] ?? field}</span>
      <span className="text-muted line-through">{describe(from)}</span>
      <span aria-hidden className="text-muted">
        &rarr;
      </span>
      <span>{describe(to)}</span>
    </li>
  );
}

/** What a version's summary says, in words. */
function Summary({ summary }: { summary: AutomationVersionSummary }) {
  if (summary.first) {
    return <p className="text-[12.5px] text-muted">The first version of this rule.</p>;
  }
  const changed = summary.changed ?? [];
  if (changed.length === 0) {
    // A write that changed nothing still belongs in the history — it happened — but saying
    // "nothing changed" is what keeps it from looking like a rendering failure.
    return <p className="text-[12.5px] text-muted">Saved; nothing in the definition moved.</p>;
  }
  return (
    <ul className="flex flex-col gap-1">
      {changed.map((change) => (
        <ChangeLine
          key={change.field}
          field={change.field}
          from={change.from}
          to={change.to}
        />
      ))}
    </ul>
  );
}

/**
 * The Versions tab: the rule's history, newest first, with Restore on every row but the one
 * the rule is running.
 */
export function VersionsPanel({
  automationId,
  onRestored,
}: {
  automationId: string;
  /** Called after a restore, so the open editor reloads the rule that changed. */
  onRestored?: () => void;
}) {
  const [history, setHistory] = useState<AutomationVersionList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let live = true;
    setHistory(null);
    setError(null);
    fetchAutomationVersions(automationId)
      .then((answer) => {
        if (live) {
          setHistory(answer);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setError(
            cause instanceof ApiError ? cause.message : "The version history could not be loaded.",
          );
        }
      });
    return () => {
      live = false;
    };
  }, [automationId, reload]);

  const restore = useCallback(
    async (version: AutomationVersion) => {
      setBusy(version.id);
      setError(null);
      setNotice(null);
      try {
        const written = await restoreAutomationVersion(automationId, version.id);
        setNotice(
          `Version ${written.version} restored. The rule now reads as version ${written.version}; the earlier versions are still here.`,
        );
        setReload((token) => token + 1);
        onRestored?.();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "That version could not be restored.");
      } finally {
        setBusy(null);
      }
    },
    [automationId, onRestored],
  );

  if (error && !history) {
    return (
      <p role="alert" className="px-4 py-6 text-[12.5px] text-negative">
        {error}
      </p>
    );
  }
  if (!history) {
    return <LoadingTable columns={4} rows={3} />;
  }

  // "Untracked" and "never changed" are different facts and the tab says which one it is:
  // a rule written before this feature shipped has no history at all, which is not the same
  // as a rule nobody has edited.
  if (history.untracked) {
    return (
      <EmptyState
        testId="automation-versions"
        title="No history yet"
        hint={`This rule was written before versions were recorded. The first edit from now on starts the history at version ${history.current_version + 1}.`}
      />
    );
  }
  if (history.versions.length === 0) {
    return <EmptyState testId="automation-versions" title="No history yet" hint="Save the rule once and its version appears here." />;
  }

  return (
    <div className="flex flex-col gap-3 p-4">
      {notice ? (
        <p
          role="status"
          data-automation-versions-notice
          className="rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          role="alert"
          data-automation-versions-error
          className="rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative"
        >
          {error}
        </p>
      ) : null}
      <ul data-automation-versions className="flex flex-col gap-2">
        {history.versions.map((version) => (
          <li
            key={version.id}
            data-automation-version-row={String(version.version)}
            className="flex flex-col gap-2 rounded-md border border-line px-3 py-2.5"
          >
            <div className="flex flex-wrap items-center gap-2 text-[12.5px]">
              <span className="font-medium">v{version.version}</span>
              <StatusBadge status={version.change === "created" ? "published" : "draft"} />
              {version.current ? (
                <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                  running now
                </span>
              ) : null}
              {version.restored_from ? (
                <span className="text-[11.5px] text-muted">restored from an earlier version</span>
              ) : null}
              <span className="ml-auto text-[11.5px] text-muted">
                {formatTimestamp(version.created_at)}
              </span>
            </div>
            <Summary summary={version.summary} />
            {version.current ? null : (
              <div>
                <button
                  type="button"
                  data-automation-version-restore={String(version.version)}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
                  onClick={() => restore(version)}
                  disabled={busy === version.id}
                >
                  <RotateCcw size={13} aria-hidden />
                  {busy === version.id ? "Restoring…" : "Restore this version"}
                </button>
              </div>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The Audit tab: who changed this rule, and when. */
export function AuditPanel({ automationId }: { automationId: string }) {
  const [entries, setEntries] = useState<AutomationAuditEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setEntries(null);
    setError(null);
    fetchAutomationAudit(automationId, { limit: 100 })
      .then((answer) => {
        if (live) {
          setEntries(answer.entries);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setEntries([]);
          setError(
            cause instanceof ApiError ? cause.message : "The audit trail could not be loaded.",
          );
        }
      });
    return () => {
      live = false;
    };
  }, [automationId]);

  if (error) {
    return (
      <p role="alert" data-automation-audit-error className="px-4 py-6 text-[12.5px] text-negative">
        {error}
      </p>
    );
  }
  if (!entries) {
    return <LoadingTable columns={3} rows={3} />;
  }
  if (entries.length === 0) {
    // A panel that renders an empty box reads as a failure; one that never renders reads as
    // a missing feature. This says the trail is empty, which is a different fact.
    return (
      <EmptyState
        testId="automation-audit"
        title="Nothing has been recorded yet"
        hint="Every change to this rule is written here the moment it happens."
      />
    );
  }

  return (
    <ul data-automation-audit className="flex flex-col divide-y divide-line">
      {entries.map((entry) => (
        <li
          key={entry.id}
          data-automation-audit-row={entry.action}
          className="flex flex-wrap items-baseline gap-x-2 px-4 py-2.5 text-[12.5px]"
        >
          <span className="font-mono text-[11.5px] text-muted">
            {formatTimestamp(entry.created_at)}
          </span>
          <span className="font-medium">{entry.action}</span>
          <span className="text-muted">
            {entry.actor_user_id ? `by ${entry.actor_user_id.slice(0, 8)}` : `by the ${entry.actor_type}`}
          </span>
          {typeof entry.metadata?.name === "string" ? (
            <span className="text-muted">— {entry.metadata.name}</span>
          ) : null}
          {typeof entry.metadata?.from_version === "number" ? (
            <span className="text-muted">
              — v{String(entry.metadata.from_version)} &rarr; v{String(entry.metadata.to_version)}
            </span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

/** The tab strip below the editor: the four things an operator reads after editing. */
export type OperationsTab = "runs" | "versions" | "audit";

/** The tabs, in the order they are drawn. */
export const OPERATIONS_TABS: { key: OperationsTab; label: string }[] = [
  { key: "runs", label: "Runs" },
  { key: "versions", label: "Versions" },
  { key: "audit", label: "Audit" },
];

/** One step of a run's trace, with the two numbers the acceptance criteria name. */
function TraceStep({
  step,
  run,
  onRerun,
  busy,
}: {
  step: AutomationRunDetail["steps"][number];
  run: AutomationRunDetail;
  onRerun: (stepNo: number) => void;
  busy: number | null;
}) {
  // Attempts used against attempts allowed is the line an operator reads first, and it is
  // the one the request calls out by name: "the step shows attempts used against attempts
  // allowed". `attempts` counts the first run as one, so a step that has never run reads
  // 0/1 rather than a bare zero that looks like a bug.
  const attempts = `${step.attempts} of ${step.max_attempts}`;
  const duration = durationOf(step.started_at, step.finished_at);

  return (
    <li
      data-automation-trace-step={String(step.step_no)}
      data-automation-trace-status={step.status}
      className="flex flex-col gap-1.5 rounded-md border border-line px-3 py-2.5"
    >
      <div className="flex flex-wrap items-baseline gap-2 text-[12.5px]">
        <span className="font-mono text-[11.5px] text-muted">#{step.step_no}</span>
        <span className="font-medium">{step.name}</span>
        <StatusBadge status={step.status} />
        {step.action ? <span className="text-muted">{step.action}</span> : null}
        <span data-automation-trace-attempts className="ml-auto text-[11.5px] text-muted">
          {attempts} attempts · {duration}
        </span>
      </div>
      {step.error ? (
        <p
          data-automation-trace-step-error
          className="flex items-start gap-1.5 text-[12.5px] text-negative"
        >
          <AlertTriangle size={13} aria-hidden className="mt-0.5 shrink-0" />
          <span>{step.error}</span>
        </p>
      ) : null}
      {run.can_retry && step.status === "failed" ? (
        <div>
          <button
            type="button"
            data-automation-trace-retry={String(step.step_no)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
            onClick={() => onRerun(step.step_no)}
            disabled={busy === step.step_no}
          >
            <RotateCcw size={13} aria-hidden />
            {busy === step.step_no ? "Queueing…" : "Retry from here"}
          </button>
        </div>
      ) : null}
    </li>
  );
}

/** How long a step took, or an em dash when it has not finished. */
function durationOf(started: string | null | undefined, finished: string | null | undefined): string {
  if (!started || !finished) {
    return "—";
  }
  const from = new Date(started).getTime();
  const to = new Date(finished).getTime();
  if (Number.isNaN(from) || Number.isNaN(to) || to < from) {
    return "—";
  }
  return `${to - from} ms`;
}

/**
 * The run trace (`/automations/[id]/runs/[run_id]`).
 *
 * A sidebar holds the raw event payload behind a disclosure, because the payload is what a
 * developer wants and what an operator never reads — collapsed by default, it does not push
 * the trace off the screen.
 */
export function RunTrace({
  executionId,
  onChanged,
}: {
  executionId: string;
  /** Called after a cancel, retry or resume, so the caller can refresh its list. */
  onChanged?: () => void;
}) {
  const [run, setRun] = useState<AutomationRunDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [reload, setReload] = useState(0);
  const [payloadOpen, setPayloadOpen] = useState(false);

  useEffect(() => {
    let live = true;
    setRun(null);
    setError(null);
    fetchAutomationRun(executionId)
      .then((answer) => {
        if (live) {
          setRun(answer);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setError(cause instanceof ApiError ? cause.message : "That run could not be loaded.");
        }
      });
    return () => {
      live = false;
    };
  }, [executionId, reload]);

  const rerun = useCallback(
    async (stepNo: number) => {
      setBusy(stepNo);
      setError(null);
      try {
        await resumeAutomationFrom(executionId, stepNo);
        setReload((token) => token + 1);
        onChanged?.();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "That step could not be re-queued.");
      } finally {
        setBusy(null);
      }
    },
    [executionId, onChanged],
  );

  const cancel = useCallback(async () => {
    setError(null);
    try {
      await cancelAutomationRun(executionId);
      setReload((token) => token + 1);
      onChanged?.();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "That run could not be cancelled.");
    }
  }, [executionId, onChanged]);

  if (error && !run) {
    return (
      <p role="alert" className="px-4 py-6 text-[12.5px] text-negative">
        {error}
      </p>
    );
  }
  if (!run) {
    return <LoadingTable columns={4} rows={4} />;
  }

  const succeeded = run.steps.filter((step) => step.status === "succeeded").length;
  const failed = run.steps.filter((step) => step.status === "failed").length;

  return (
    <div data-automation-trace={run.id} className="flex flex-col gap-4 p-4">
      <header className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <Link
          href="/automations"
          className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft size={13} aria-hidden />
          All automations
        </Link>
        <StatusBadge status={run.status} />
        <span className="text-[12.5px] text-muted">
          started {formatTimestamp(run.started_at)} · {run.trigger_kind}
        </span>
        <span data-automation-trace-summary className="text-[12.5px] text-muted">
          {succeeded} succeeded, {failed} failed of {run.steps.length}
        </span>
        {run.can_cancel ? (
          <button
            type="button"
            data-automation-trace-cancel
            className="ml-auto rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            onClick={cancel}
          >
            Cancel this run
          </button>
        ) : null}
      </header>

      {error ? (
        <p
          role="alert"
          data-automation-trace-error
          className="rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative"
        >
          {error}
        </p>
      ) : null}
      {run.error ? (
        <p
          data-automation-trace-run-error
          className="rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative"
        >
          {run.error}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_18rem]">
        <ol data-automation-trace-steps className="flex flex-col gap-2">
          {run.steps.map((step) => (
            <TraceStep
              key={step.step_no}
              step={step}
              run={run}
              onRerun={rerun}
              busy={busy}
            />
          ))}
        </ol>

        <aside className="flex flex-col gap-2">
          <button
            type="button"
            data-automation-trace-payload
            className="flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
            aria-expanded={payloadOpen}
            onClick={() => setPayloadOpen((open) => !open)}
          >
            <ScrollText size={13} aria-hidden />
            {payloadOpen ? "Hide the event payload" : "Show the event payload"}
          </button>
          {payloadOpen ? (
            run.event_payload ? (
              <pre className="max-h-96 overflow-auto rounded-md bg-quiet-soft p-3 text-[11.5px]">
                {JSON.stringify(run.event_payload, null, 2)}
              </pre>
            ) : (
              <p className="text-[12.5px] text-muted">
                This run started by hand or on a schedule, so it has no event payload.
              </p>
            )
          ) : null}
        </aside>
      </div>
    </div>
  );
}

/** The run history of one rule, newest first; a row opens its trace. */
export function RunsPanel({
  automationId,
  reloadToken = 0,
}: {
  automationId: string;
  /** Bumped by whoever started a run, so the list is not read once and abandoned. */
  reloadToken?: number;
}) {
  const [runs, setRuns] = useState<AutomationRunSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reload, setReload] = useState(0);

  useEffect(() => {
    let live = true;
    setRuns(null);
    setError(null);
    fetchAutomationRunHistory(automationId)
      .then((answer) => {
        if (live) {
          setRuns(answer);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setRuns([]);
          setError(
            cause instanceof ApiError ? cause.message : "The run history could not be loaded.",
          );
        }
      });
    return () => {
      live = false;
    };
  }, [automationId, reload, reloadToken]);

  if (error) {
    return (
      <p role="alert" className="px-4 py-6 text-[12.5px] text-negative">
        {error}
      </p>
    );
  }
  if (!runs) {
    return <LoadingTable columns={RUN_COLUMNS.length} rows={3} />;
  }
  if (runs.length === 0) {
    // "Never run" and "the list is broken" are different facts, and only one of them is
    // true here — so the empty state says which, instead of showing a table with no rows.
    return (
      <EmptyState
        title="This rule has not run yet"
        hint="Runs appear here the moment its event arrives, or when you press Run now."
      />
    );
  }

  return (
    <table data-automation-runs className="w-full text-[12.5px]">
      <thead>
        <tr className="border-b border-line text-left text-muted">
          {RUN_COLUMNS.map((column) => (
            <th key={column} className="px-4 py-2 font-medium">
              {column}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {runs.map((run) => (
          <tr key={run.id} data-automation-run-row={run.id} className="border-b border-line last:border-0">
            <td className="px-4 py-2">
              <Link
                data-automation-run-open={run.id}
                href={`/automations/${automationId}/runs/${run.id}`}
                className="hover:underline"
              >
                {formatTimestamp(run.started_at)}
              </Link>
            </td>
            <td className="px-4 py-2">
              <StatusBadge status={run.status} />
            </td>
            <td className="px-4 py-2 text-muted">{run.trigger}</td>
            <td className="px-4 py-2 text-muted">{run.step_count}</td>
            <td className="px-4 py-2 text-muted">
              {run.error ? run.error.slice(0, 80) : "—"}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/**
 * The gallery route's whole screen: `/automations/templates`.
 *
 * Every card is a *real* starter, and *Use this* is a create — the same `POST
 * /api/v1/automations` a hand-written rule takes, so an installed template is validated,
 * audited and rate-bounded like any other rule. Nothing here is a preview.
 */
export function AutomationTemplatesView() {
  // The tenant is resolved *here* rather than passed in. The route is a server component
  // that has no session, and the install is the one control on this screen that writes — so
  // the screen that reads the session is the screen that must send it. A prop would push the
  // question one level up to a caller that has no better answer.
  const { user } = useSession();
  // A platform account belongs to no organization, and a rule is created *into* one: the
  // create endpoint refuses a body with no tenant, so the gallery's one real control would
  // fail on exactly the installation where a starter is most wanted. The rule list solved
  // the same problem by reading the organizations and defaulting to the first, so the
  // gallery does the same instead of handing the button a null it cannot use. An account
  // that belongs to an organization never pays for this.
  const [platformOrganization, setPlatformOrganization] = useState<string | null>(null);
  // `false` until the organizations read has answered, so "there are none" and "we have not
  // asked" do not look the same for the length of one round trip.
  const [tenantResolved, setTenantResolved] = useState(false);
  useEffect(() => {
    if (!user || user.organization_id !== null) {
      setTenantResolved(true);
      return;
    }
    let live = true;
    fetchOrganizations()
      .then((list) => {
        if (live) {
          setPlatformOrganization(list[0]?.id ?? null);
          setTenantResolved(true);
        }
      })
      .catch(() => {
        if (live) {
          setTenantResolved(true);
        }
      });
    return () => {
      live = false;
    };
  }, [user]);
  const organizationId = user?.organization_id ?? platformOrganization;
  // "Which tenant" is a question with three answers, and two of them are the same button
  // state: not yet known, and none. Both have to read as *not pressable*, because the
  // difference between them is a network round trip and a pass that clicks inside it sends
  // a create with no tenant and reads a 400 off a control that works.
  const resolvingTenant =
    Boolean(user) && user?.organization_id === null && platformOrganization === null &&
    tenantResolved === false;
  const needsOrg =
    Boolean(user) && user?.organization_id === null && tenantResolved && organizationId === null;
  const [templates, setTemplates] = useState<AutomationTemplate[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [using, setUsing] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetchAutomationTemplates()
      .then((answer) => {
        if (live) {
          setTemplates(answer.templates);
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setError(cause instanceof ApiError ? cause.message : "The templates could not be loaded.");
        }
      });
    return () => {
      live = false;
    };
  }, []);

  const install = useCallback(
    async (key: string) => {
      setUsing(key);
      setError(null);
      setNotice(null);
      try {
        const created = await installTemplate(key, organizationId);
        setNotice(`"${created}" is created and paused. Open it to fill in what it needs, then arm it.`);
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "That template could not be installed.");
      } finally {
        setUsing(null);
      }
    },
    [organizationId],
  );

  const grouped = useMemo(() => {
    const byCategory = new Map<string, typeof templates>();
    for (const template of templates ?? []) {
      const bucket = byCategory.get(template.category) ?? [];
      bucket.push(template);
      byCategory.set(template.category, bucket);
    }
    return [...byCategory.entries()];
  }, [templates]);

  if (error && !templates) {
    return (
      <p role="alert" className="px-6 py-10 text-[12.5px] text-negative">
        {error}
      </p>
    );
  }
  if (!templates) {
    return <LoadingTable columns={3} rows={3} />;
  }
  if (templates.length === 0) {
    return (
      <EmptyState
        title="No templates"
        hint="The starter rules are part of the platform, so this list is never empty unless the API is unreachable."
      />
    );
  }
  // A platform account with no tenant has nowhere to install a starter into. Saying so in
  // one sentence beats six cards whose buttons each fail the same way.
  if (needsOrg) {
    return (
      <div data-automation-templates className="flex flex-col gap-5 p-6">
        <header className="flex flex-wrap items-baseline gap-3">
          <h1 className="text-[15px] font-medium">Templates</h1>
          <Link href="/automations" className="text-[12.5px] text-muted hover:text-ink">
            Back to automations
          </Link>
        </header>
        <p role="status" className="text-[12.5px] text-muted">
          A rule belongs to a tenant. Create or open an organization on the sites screen, then
          install a starter from here.
        </p>
      </div>
    );
  }

  return (
    <div data-automation-templates className="flex flex-col gap-5 p-6">
      <header className="flex flex-wrap items-baseline gap-3">
        <h1 className="text-[15px] font-medium">Templates</h1>
        <Link href="/automations" className="text-[12.5px] text-muted hover:text-ink">
          Back to automations
        </Link>
      </header>

      {notice ? (
        <p
          role="status"
          data-automation-templates-notice
          className="rounded-md bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}
      {error ? (
        <p
          role="alert"
          data-automation-templates-error
          className="rounded-md bg-negative-soft px-3 py-2 text-[12.5px] text-negative"
        >
          {error}
        </p>
      ) : null}

      {grouped.map(([category, items]) => (
        <section key={category} data-automation-template-category={category} className="flex flex-col gap-2">
          <h2 className="text-[12.5px] font-medium text-muted">{category}</h2>
          <ul className="grid gap-3 md:grid-cols-2">
            {items?.map((template) => (
              <li
                key={template.key}
                data-automation-template-card={template.key}
                className="flex flex-col gap-2 rounded-md border border-line p-4"
              >
                <div className="flex flex-wrap items-baseline gap-2">
                  <span className="text-[13.5px] font-medium">{template.name}</span>
                  <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                    {template.event}
                  </span>
                </div>
                <p className="text-[12.5px] text-muted">{template.description}</p>
                <p className="text-[12px] text-muted">
                  {template.condition_count} condition{template.condition_count === 1 ? "" : "s"} ·{" "}
                  {template.action_count} action{template.action_count === 1 ? "" : "s"}
                </p>
                {template.requires.length > 0 ? (
                  <p className="text-[12px] text-muted">
                    You will need: {template.requires.join("; ")}.
                  </p>
                ) : (
                  <p className="text-[12px] text-muted">Ready to use as it is.</p>
                )}
                {template.blocked_reason ? (
                  <p className="flex items-start gap-1.5 text-[12px] text-caution">
                    <History size={13} aria-hidden className="mt-0.5 shrink-0" />
                    {template.blocked_reason}
                  </p>
                ) : null}
                <div>
                  <button
                    type="button"
                    data-automation-template-use={template.key}
                    className="rounded-md border border-line px-2.5 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
                    onClick={() => install(template.key)}
                    disabled={
                      using === template.key ||
                      template.installable === false ||
                      organizationId === null ||
                      resolvingTenant
                    }
                  >
                    {using === template.key
                      ? "Installing…"
                      : template.installable === false
                        ? "Not available on this installation"
                        : resolvingTenant
                          ? "Finding the tenant…"
                          : organizationId === null
                            ? "Choose a tenant to install"
                            : "Use this template"}
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

/**
 * Create a rule from a starter and return its name, so the notice can quote it.
 *
 * The gallery's *own* read decides what to write rather than trusting a body the button was
 * handed: the card is a pointer, not a payload. The create is an ordinary one, so the
 * installed rule is validated, audited and rate-bounded like any rule the author typed.
 *
 * `organizationId` is not decoration. A starter's body carries no tenant — it is a *shape* —
 * so without it the create is refused for any account that is not already bound to one, and
 * the gallery's one real control fails on exactly the installation where a starter is most
 * wanted. The same value the editor sends is the one this sends.
 */
async function installTemplate(key: string, organizationId?: string | null): Promise<string> {
  const answer = await fetchAutomationTemplates();
  const template = answer.templates.find((row) => row.key === key);
  if (!template) {
    throw new ApiError(404, "template_not_found", "That template is no longer offered.");
  }
  const created = await createAutomation({
    ...(template.body as AutomationInput),
    organization_id: organizationId ?? null,
  });
  return created.name;
}
