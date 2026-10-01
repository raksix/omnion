"use client";

/**
 * `/deployment/upgrade` — the upgrade helper (REQ-128, slice 4).
 *
 * ## `unknown` is a third verdict, not a colour to invent
 *
 * The decision layer reports whether the database can go back as `reversible`, `destructive`, or
 * **`unknown`** — and `unknown` is what this repository's own migrations report, because the
 * REQ-129 `up → down → up` gate has not proved any of them reversible. A release manifest saying
 * `migrations_destructive: false` is the publisher's silence, not a verification, and the screen
 * says so in words wherever the flag would otherwise be read as a promise. **The point of no
 * return attaches to the FIRST migration while the verdict is not `reversible`**, so a reversible
 * upgrade has no marker at all rather than a reassuring one.
 *
 * ## The checklist cannot render as complete without the acknowledgement
 *
 * That is the server's rule and the screen's, and it is a real gate rather than a disabled button:
 * the checklist row shows what is missing, the acknowledge control posts the plan's OWN verdict
 * (never a literal picked from a list), and until the server answers `acknowledged`, the
 * complete-check line keeps saying what it is waiting for.
 *
 * ## The rollback split is the point of the screen
 *
 * Application rollback is always available and carries its command. Database rollback is a down
 * script, a restore, or an honest "nobody knows" — and each is shown as itself, with the command
 * next to it, because an operator reading "rollback: available" for a database that can only be
 * restored is reading the single most expensive sentence on this screen.
 *
 * Keyboard: `r` regenerates the plan, `p` prints the checklist, `Esc` clears a dismissed panel.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  ArrowRight,
  CircleSlash,
  CheckCircle2,
  FileText,
  Flag,
  Loader2,
  Printer,
  RefreshCw,
  ShieldAlert,
  Undo2,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  acknowledgeUpgradePlan,
  fetchUpgradePlan,
  type Destructiveness,
  type PlanStep,
  type Rollback,
  type UpgradePlan,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

const TOPOLOGIES = [
  { key: "compose", label: "Compose", detail: "docker compose on one host" },
  { key: "kubernetes", label: "Kubernetes", detail: "helm install / helm upgrade" },
] as const;

const CHANNELS = ["stable", "beta", "edge"] as const;

/** The verdict, its tone, and the sentence under it. Three verdicts, none of them a colour only. */
const VERDICT: Record<
  string,
  { tone: string; icon: typeof AlertTriangle; label: string; meaning: string }
> = {
  reversible: {
    tone: "bg-positive-soft text-positive",
    icon: CheckCircle2,
    label: "reversible",
    meaning:
      "Every migration in this range shipped a down script and the verification gate has run them.",
  },
  destructive: {
    tone: "bg-danger-soft text-danger",
    icon: ShieldAlert,
    label: "destructive",
    meaning:
      "At least one migration in this range has no down script. The database goes back only from a backup.",
  },
  unknown: {
    tone: "bg-caution-soft text-caution",
    icon: CircleSlash,
    label: "unknown",
    meaning:
      "Nobody has verified whether these migrations can be reversed — a missing down script is not the same as a proven irreversible one, and the release manifest's silence is not a verification.",
  },
};

function verdictOf(name: string) {
  return (
    VERDICT[name] ?? {
      tone: "bg-quiet-soft text-muted",
      icon: CircleSlash,
      label: name,
      meaning: "No verdict rule matches this value.",
    }
  );
}

/** A step's kind, as a word rather than an icon soup. */
const STEP_KIND: Record<string, string> = {
  backup: "Back up",
  migrate: "Migrate",
  deploy: "Deploy",
  verify: "Verify",
  manual: "Operator step",
};

function StepRow({ step, index }: { step: PlanStep; index: number }) {
  return (
    <li
      className={`rounded-lg border px-3 py-2.5 ${
        step.destructive ? "border-danger-soft bg-danger-soft/30" : "border-line"
      }`}
      data-plan-step={step.kind}
      data-plan-step-index={index}
      data-plan-step-destructive={String(step.destructive)}
    >
      <div className="flex items-start gap-2">
        <span className="mt-0.5 font-mono text-[11px] tabular-nums text-muted">
          {String(index + 1).padStart(2, "0")}
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-[12.5px]">
            <span className="font-medium">{STEP_KIND[step.kind] ?? step.kind}</span>{" "}
            <span className="text-muted">{step.text}</span>
          </p>
          {step.command ? (
            <pre
              className="mt-1.5 overflow-x-auto rounded-md bg-quiet-soft/60 p-2 font-mono text-[11px]"
              data-plan-step-command={index}
            >
              {step.command}
            </pre>
          ) : step.check ? (
            <p className="mt-1.5 text-[11.5px] text-muted" data-plan-step-check={index}>
              Check <code className="font-mono">{step.check.path}</code> answers{" "}
              <span className="font-mono">{step.check.expect_status}</span>. {step.check.how}
            </p>
          ) : null}
          {step.migrations && step.migrations.length > 0 ? (
            <ul className="mt-1.5 space-y-0.5 font-mono text-[11px] text-muted">
              {step.migrations.map((migration) => (
                <li key={migration}>{migration}</li>
              ))}
            </ul>
          ) : null}
          {step.image ? (
            <p className="mt-1 break-all font-mono text-[11px] text-muted">
              {step.image}
            </p>
          ) : null}
          {step.notes_url ? (
            <a
              href={step.notes_url}
              target="_blank"
              rel="noreferrer noopener"
              className="mt-1 inline-block text-[11.5px] underline underline-offset-2"
              data-plan-step-notes={index}
            >
              This version&rsquo;s notes
            </a>
          ) : null}
        </div>
        {step.point_of_no_return ? (
          <span
            className="flex shrink-0 items-center gap-1 rounded-full bg-danger-soft px-1.5 py-0.5 text-[11px] font-medium text-danger"
            title="Past this step the database cannot return to the previous version on its own."
            data-plan-point-of-no-return={index}
          >
            <Flag className="size-3" aria-hidden="true" />
            No return
          </span>
        ) : null}
      </div>
    </li>
  );
}

function RollbackCard({ title, children, available }: { title: string; children: React.ReactNode; available: boolean | null }) {
  return (
    <div
      className={`rounded-lg border p-3 ${
        available === false ? "border-caution-soft" : "border-line"
      }`}
      data-rollback-card={title}
      data-rollback-available={String(available)}
    >
      <h4 className="text-[12.5px] font-medium">{title}</h4>
      <div className="mt-1 text-[11.5px] text-muted">{children}</div>
    </div>
  );
}

function rollbackText(rollback: Rollback): string {
  switch (rollback.database.method) {
    case "down-script":
      return "This range shipped a verified down script, so the database itself can step back.";
    case "restore-from-backup":
      return "The database cannot be stepped back. Restoring the backup taken in step 1 is the only way back — and the backup is as old as that first step.";
    default:
      return "Nobody knows whether the database can be stepped back: this range has migrations with no down script, and no verification gate has proved them irreversible either. Restore from the backup.";
  }
}

// -------------------------------------------------------------------------------------------

export function UpgradeView() {
  const [topology, setTopology] = useState<"compose" | "kubernetes">("compose");
  const [bundleKind, setBundleKind] = useState("compose-small");
  const [channel, setChannel] = useState<string>("stable");
  const [target, setTarget] = useState("");

  const [plan, setPlan] = useState<UpgradePlan | null>(null);
  const [stored, setStored] = useState<{
    id: string;
    destructive_verdict: string;
    destructive_acknowledged_by: string | null;
    destructive_acknowledged_at: string | null;
  } | null>(null);
  const [running, setRunning] = useState<string>("");
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const [problems, setProblems] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const [acknowledging, setAcknowledging] = useState(false);
  const [ackError, setAckError] = useState<string | null>(null);
  const [checked, setChecked] = useState<Record<number, boolean>>({});
  const targetRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const response = await fetchUpgradePlan({
        to: target.trim() || undefined,
        channel,
        topology,
        bundle_kind: topology === "compose" ? bundleKind : undefined,
      });
      setRunning(response.running_version);
      setPlan(response.summary.plan);
      setUnavailable(response.summary.unavailable);
      setProblems(response.problems ?? response.summary.problems ?? []);
      setStored(response.summary.stored);
      setChecked({});
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The upgrade plan could not be built.",
      );
      setPlan(null);
    } finally {
      setLoading(false);
    }
  }, [target, channel, topology, bundleKind]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const element = event.target as HTMLElement | null;
      if (element?.tagName === "INPUT" || element?.tagName === "SELECT") return;
      if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "p" && plan) {
        event.preventDefault();
        window.print();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, plan]);

  const done = useMemo(
    () => Object.values(checked).filter(Boolean).length,
    [checked],
  );

  const acknowledge = async (verdict: string) => {
    if (!stored) return;
    setAcknowledging(true);
    setAckError(null);
    try {
      await acknowledgeUpgradePlan(stored.id, verdict);
      await load();
    } catch (caught) {
      setAckError(
        caught instanceof ApiError ? caught.message : "The acknowledgement could not be recorded.",
      );
    } finally {
      setAcknowledging(false);
    }
  };

  const verdict = plan ? verdictOf(plan.destructive.verdict) : null;
  const Icon = verdict?.icon ?? CircleSlash;

  return (
    <div className="space-y-4" data-view="deployment-upgrade">
      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <ArrowRight className="size-4 text-accent" aria-hidden="true" />
            Plan an upgrade
          </h2>
          <span className="text-[12px] text-muted" data-upgrade-running={running}>
            this instance runs {running || "an unknown build"}
          </span>
          <div className="ml-auto flex items-center gap-2">
            <button
              type="button"
              onClick={() => void load()}
              data-upgrade-refresh
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <RefreshCw className="h-3 w-3" aria-hidden="true" />
              Regenerate
            </button>
            <button
              type="button"
              onClick={() => window.print()}
              disabled={!plan}
              data-upgrade-print
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft disabled:opacity-60"
            >
              <Printer className="h-3 w-3" aria-hidden="true" />
              Checklist
            </button>
          </div>
        </header>

        <div className="grid gap-3 p-4 sm:grid-cols-2 lg:grid-cols-4">
          <div>
            <label className="text-[12px] text-muted" htmlFor="upgrade-topology">
              Topology
            </label>
            <select
              id="upgrade-topology"
              value={topology}
              onChange={(event) => setTopology(event.target.value as "compose" | "kubernetes")}
              data-upgrade-topology
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
            >
              {TOPOLOGIES.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.label}
                </option>
              ))}
            </select>
            <p className="mt-1 text-[11.5px] text-muted">
              {TOPOLOGIES.find((entry) => entry.key === topology)?.detail}
            </p>
          </div>
          {topology === "compose" ? (
            <div>
              <label className="text-[12px] text-muted" htmlFor="upgrade-stack">
                Stack
              </label>
              <select
                id="upgrade-stack"
                value={bundleKind}
                onChange={(event) => setBundleKind(event.target.value)}
                data-upgrade-stack
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
              >
                <option value="compose-small">compose-small — own datastores</option>
                <option value="compose-enterprise">
                  compose-enterprise — external datastores
                </option>
              </select>
            </div>
          ) : null}
          <div>
            <label className="text-[12px] text-muted" htmlFor="upgrade-channel">
              Channel
            </label>
            <select
              id="upgrade-channel"
              value={channel}
              onChange={(event) => setChannel(event.target.value)}
              data-upgrade-channel
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
            >
              {CHANNELS.map((entry) => (
                <option key={entry} value={entry}>
                  {entry}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className="text-[12px] text-muted" htmlFor="upgrade-target">
              Target version (optional)
            </label>
            <input
              id="upgrade-target"
              ref={targetRef}
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              data-upgrade-target
              placeholder="newest cached"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
            />
          </div>
        </div>
      </section>

      {error ? (
        <p role="alert" data-upgrade-error className="rounded-xl border border-line px-4 py-3 text-[12.5px] text-danger">
          {error}
        </p>
      ) : loading && !plan ? (
        <div className="rounded-xl border border-line px-4 py-3">
          <LoadingTable rows={4} columns={4} />
        </div>
      ) : unavailable ? (
        <section className="rounded-xl border border-line">
          <EmptyState
            title="There is no release to plan against"
            hint={unavailable}
          />
        </section>
      ) : !plan ? null : (
        <>
          {problems.length > 0 ? (
            <section
              className="rounded-xl border border-caution-soft bg-caution-soft/30 p-4"
              data-upgrade-problems
            >
              <h3 className="flex items-center gap-2 text-[13px] font-medium text-caution">
                <AlertTriangle className="size-4" aria-hidden="true" />
                {problems.length} thing{problems.length === 1 ? "" : "s"} about this plan
              </h3>
              <ul className="mt-2 space-y-1 text-[12px]">
                {problems.map((problem) => (
                  <li key={problem} data-upgrade-problem={problem}>
                    {problem}
                  </li>
                ))}
              </ul>
            </section>
          ) : null}

          <section className="rounded-xl border border-line">
            <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
              <h2 className="font-mono text-[14px]">
                {plan.from_version} → {plan.to_version}
              </h2>
              <span
                className={`inline-flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[11px] ${
                  verdict?.tone ?? "bg-quiet-soft text-muted"
                }`}
                title={verdict?.meaning}
                data-upgrade-verdict={plan.destructive.verdict}
              >
                {Icon ? <Icon className="size-3" aria-hidden="true" /> : null}
                {verdict?.label ?? plan.destructive.verdict}
              </span>
              <span className="text-[12px] text-muted">
                {plan.migrations_applied.length} migration
                {plan.migrations_applied.length === 1 ? "" : "s"}
                {plan.image ? " · pinned by digest" : " · tagged"}
              </span>
            </header>

            <div className="grid gap-3 p-4 lg:grid-cols-3">
              <div className="lg:col-span-2 space-y-2">
                <h3 className="text-[12.5px] font-medium">
                  Ordered steps ({plan.steps.length})
                </h3>
                <ol className="space-y-2" data-plan-steps>
                  {plan.steps.map((step, index) => (
                    <StepRow key={`${step.kind}-${index}`} step={step} index={index} />
                  ))}
                </ol>
              </div>

              <aside className="space-y-3">
                <div className="rounded-lg border border-line p-3">
                  <h3 className="text-[12.5px] font-medium">What is known about rollback</h3>
                  <p
                    className={`mt-1.5 rounded-md px-2 py-1.5 text-[11.5px] ${
                      verdict?.tone ?? "bg-quiet-soft text-muted"
                    }`}
                    data-upgrade-verdict-reason={plan.destructive.source}
                  >
                    {plan.destructive.reason}
                  </p>
                  <p className="mt-1.5 text-[11px] text-muted">
                    Decided by <span className="font-mono">{plan.destructive.source}</span>.
                  </p>
                  {plan.destructive.destructive_migrations.length > 0 ? (
                    <ul className="mt-1.5 space-y-0.5 font-mono text-[11px] text-danger">
                      {plan.destructive.destructive_migrations.map((migration) => (
                        <li key={migration} data-destructive-migration={migration}>
                          {migration}
                        </li>
                      ))}
                    </ul>
                  ) : null}
                </div>

                <RollbackCard
                  title="Application rollback"
                  available={plan.rollback.application.available}
                >
                  {plan.rollback.application.available ? (
                    <>
                      Always available: a previous image tag is a deployment, not a schema change.
                      {plan.rollback.application.command ? (
                        <pre
                          className="mt-1.5 overflow-x-auto rounded-md bg-quiet-soft/60 p-2 font-mono text-[11px]"
                          data-rollback-application-command
                        >
                          {plan.rollback.application.command}
                        </pre>
                      ) : null}
                    </>
                  ) : (
                    <>No previous image reference is reachable, so there is no command to show.</>
                  )}
                </RollbackCard>

                <RollbackCard
                  title="Database rollback"
                  available={plan.rollback.database.available}
                >
                  <p data-rollback-database-method={plan.rollback.database.method}>
                    {rollbackText(plan.rollback)}
                  </p>
                  {plan.rollback.database.command ? (
                    <pre
                      className="mt-1.5 overflow-x-auto rounded-md bg-quiet-soft/60 p-2 font-mono text-[11px]"
                      data-rollback-database-command
                    >
                      {plan.rollback.database.command}
                    </pre>
                  ) : null}
                </RollbackCard>
              </aside>
            </div>
          </section>

          <section className="rounded-xl border border-line" data-plan-checklist>
            <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
              <h3 className="flex items-center gap-2 text-[13.5px] font-medium">
                <FileText className="size-4 text-accent" aria-hidden="true" />
                Printable checklist
              </h3>
              <span className="text-[12px] text-muted" data-checklist-progress>
                {done} of {plan.checklist.items.length} marked
              </span>
            </header>

            <div className="p-4">
              <ul className="space-y-1.5" data-checklist-items>
                {plan.checklist.items.map((item) => (
                  <li key={`${item.index}-${item.kind}`}>
                    <label className="flex items-start gap-2 text-[12.5px]">
                      <input
                        type="checkbox"
                        checked={Boolean(checked[item.index])}
                        onChange={(event) =>
                          setChecked((current) => ({
                            ...current,
                            [item.index]: event.target.checked,
                          }))
                        }
                        data-checklist-item={item.index}
                        className="mt-0.5 size-4 accent-[var(--accent)]"
                      />
                      <span>
                        <span className="font-mono text-[11px] text-muted">
                          {String(item.index + 1).padStart(2, "0")}
                        </span>{" "}
                        {STEP_KIND[item.kind] ?? item.kind} — {item.text}
                        {item.destructive ? (
                          <span className="ml-1.5 text-[11px] text-danger">
                            (moves the database one way)
                          </span>
                        ) : null}
                      </span>
                    </label>
                  </li>
                ))}
              </ul>

              <div className="mt-4 border-t border-line pt-3">
                {plan.checklist.complete ? (
                  <p
                    className="flex items-center gap-2 text-[12.5px] text-positive"
                    data-checklist-complete
                  >
                    <CheckCircle2 className="size-4" aria-hidden="true" />
                    {plan.checklist.acknowledged
                      ? "Acknowledged — this checklist may render as complete."
                      : "Every step is listed and no acknowledgement is required."}
                  </p>
                ) : (
                  <div data-checklist-blocked>
                    <p className="text-[12.5px] text-caution" data-checklist-blocked-reason>
                      {plan.checklist.requires_acknowledgement && !plan.checklist.acknowledged
                        ? "This range needs an operator to accept that the database may not be reversible before the checklist can be complete."
                        : "The checklist is not complete yet: the steps above have not all been marked."}
                    </p>
                    {plan.checklist.requires_acknowledgement && !plan.checklist.acknowledged ? (
                      <div className="mt-2 flex flex-wrap items-center gap-2">
                        <button
                          type="button"
                          onClick={() => void acknowledge(plan.destructive.verdict)}
                          disabled={acknowledging || !stored}
                          data-upgrade-acknowledge={plan.destructive.verdict}
                          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] text-white disabled:opacity-60"
                        >
                          {acknowledging ? (
                            <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
                          ) : (
                            <Undo2 className="size-3.5" aria-hidden="true" />
                          )}
                          I accept &ldquo;{plan.destructive.verdict}&rdquo; for {plan.from_version} →{" "}
                          {plan.to_version}
                        </button>
                        {ackError ? (
                          <span role="alert" data-upgrade-ack-error className="text-[12px] text-danger">
                            {ackError}
                          </span>
                        ) : null}
                      </div>
                    ) : null}
                    {stored?.destructive_acknowledged_at ? (
                      <p className="mt-1.5 text-[11.5px] text-muted" data-upgrade-acknowledged-at>
                        Acknowledged {formatTimestamp(stored.destructive_acknowledged_at)}.
                      </p>
                    ) : null}
                  </div>
                )}
              </div>
            </div>
          </section>

          <p className="text-[11.5px] text-muted">
            Press <kbd className="rounded border border-line px-1">r</kbd> to regenerate the plan and{" "}
            <kbd className="rounded border border-line px-1">p</kbd> to print the checklist. The
            plan is stored, so an acknowledgement survives regeneration and is a fact about this
            range rather than about this visit.
          </p>
        </>
      )}
    </div>
  );
}