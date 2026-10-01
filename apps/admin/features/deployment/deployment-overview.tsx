"use client";

/**
 * `/deployment` — the environment cards (REQ-024, slice 1).
 *
 * The screen in the owner's brief is four lines and one of them is dangerous:
 *
 * ```text
 * Production      ● Healthy
 * Version         2.4.1
 * Available       2.5.0
 * ```
 *
 * Everything below exists because that third line can be a lie while looking exactly like a
 * lookup. The values arrive from the API as a three-state `availability` enum and are rendered
 * without being re-derived here, so this file cannot compare two version strings and offer a
 * downgrade. Two more states the four lines do not name are rendered rather than hidden: a
 * release that exists but may not be offered (with its reason), and an unreachable environment.
 *
 * **The buttons in this file are honest or absent.** `Deploy` and `Rollback` are slice 2 and
 * slice 3, so they are rendered **disabled with the reason they are not available yet** rather
 * than omitted (an operator would not know the action exists) and never as live controls that
 * would issue a request no route answers. `View Changes` and the release list *are* live, and
 * `Check for updates` is the one write slice 1 ships.
 */

import { useCallback, useEffect, useState } from "react";

import { ArrowRight, History, RefreshCw, Rocket, ScrollText, Undo2, Wrench } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchDeploymentEnvironments,
  fetchDeploymentVersion,
  runDeploymentCheck,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

import { RollbackDialog, type RollbackTarget } from "./rollback-dialog";
import type {
  DeploymentCheckRunResponse,
  DeploymentEnvironmentCard,
  DeploymentEnvironmentsResponse,
  DeploymentVersion,
} from "@/lib/types";

import {
  AvailableLine,
  HealthDot,
  StaleBanner,
  formatDuration,
} from "./deployment-parts";

/** The cards, the version block, and the update check. */
export function DeploymentOverview() {
  const [data, setData] = useState<DeploymentEnvironmentsResponse | null>(null);
  const [version, setVersion] = useState<DeploymentVersion | null>(null);
  const [checks, setChecks] = useState<DeploymentCheckRunResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [requestId, setRequestId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [running, setRunning] = useState(false);
  // Which environment's rollback dialog is open, if any (REQ-024, slice 3). Held here rather
  // than per-card so exactly one dialog can exist: two stacked modals is a screen where the
  // operator confirms the wrong one.
  const [rollback, setRollback] = useState<RollbackTarget | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    setRequestId(null);
    try {
      // Both in one pass: the footer reads the version and the cards read the offers, and a
      // card that rendered before the version arrived would show its own idea of the current
      // version for a frame.
      const [cards, build] = await Promise.all([
        fetchDeploymentEnvironments(),
        fetchDeploymentVersion(),
      ]);
      setData(cards);
      setVersion(build.version);
    } catch (caught) {
      const message =
        caught instanceof ApiError ? caught.message : "The deployment centre could not be read.";
      setError(message);
      setRequestId(caught instanceof ApiError ? String(caught.status) : null);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const runCheck = useCallback(async () => {
    setRunning(true);
    try {
      const outcome = await runDeploymentCheck();
      setChecks(outcome);
      // The offers may have changed, so the cards are re-read rather than patched in the panel:
      // the check refreshed a cache, and a client-side guess of what that cache now says is
      // the exact class of bug the enum exists to prevent.
      await load();
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The update check could not be started.",
      );
    } finally {
      setRunning(false);
    }
  }, [load]);

  if (loading) {
    return (
      <div className="flex flex-col gap-4">
        <LoadingTable columns={2} rows={3} />
      </div>
    );
  }

  if (error && !data) {
    return (
      <div
        role="alert"
        className="flex flex-col items-start gap-3 rounded-xl border border-red-500/40 bg-red-500/5 px-4 py-5"
      >
        <p className="text-[13.5px] font-medium text-red-800 dark:text-red-200">{error}</p>
        {requestId ? (
          <p className="text-[12px] text-muted">
            The API answered with status {requestId}.
          </p>
        ) : null}
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
        >
          <RefreshCw aria-hidden="true" className="size-3.5" />
          Try again
        </button>
      </div>
    );
  }

  const summary = data?.summary;

  return (
    <div className="flex flex-col gap-5">
      {summary?.stale_banner ? (
        <StaleBanner text={summary.stale_banner} onRetry={() => void runCheck()} />
      ) : null}

      {error ? (
        <p role="alert" className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {/* The version block from the brief, on its own so it is one object rather than four
          lines repeated per card. The card still prints its own Version/Available pair — the
          spec asks for it on the card, and an environment pinned behind production has a
          different answer than the installation does. */}
      {version ? (
        <section
          aria-label="Installation version"
          className="flex flex-wrap items-center gap-x-8 gap-y-3 rounded-xl border border-line bg-surface px-5 py-4"
        >
          <div className="flex flex-col gap-1">
            <span className="text-[11px] font-medium tracking-wide text-muted uppercase">
              Version
            </span>
            <span className="text-[20px] leading-tight font-medium tabular-nums">
              {version.version}
            </span>
          </div>
          <div className="flex flex-col gap-1">
            <span className="text-[11px] font-medium tracking-wide text-muted uppercase">
              Available
            </span>
            <AvailableLine availability={version.availability} text={version.available} />
          </div>
          <div className="flex flex-col gap-1">
            <span className="text-[11px] font-medium tracking-wide text-muted uppercase">
              Channel
            </span>
            <span className="text-[14px] font-medium">{version.channel}</span>
            {!version.channel_understood ? (
              // Shown rather than normalized away: an unrecognised channel means the offers
              // above were computed under a rule nobody wrote, and the operator should know that
              // before pressing Deploy.
              <span className="text-[11.5px] text-amber-700 dark:text-amber-300">
                this channel is not one the deployment code recognises
              </span>
            ) : null}
          </div>
          <div className="ml-auto flex flex-wrap items-center gap-2">
            <Link
              href="/deployment/releases"
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
            >
              <ScrollText aria-hidden="true" className="size-3.5" />
              Releases
            </Link>
            <Link
              href="/deployment/history"
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
            >
              <History aria-hidden="true" className="size-3.5" />
              History
            </Link>
            {/* The maintenance screen is reachable from the centre, not only from its own URL.
                A screen that exists only where somebody typed its path is a screen nobody opens
                in the moment they need it. */}
            <Link
              href="/deployment/maintenance"
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
            >
              <Wrench aria-hidden="true" className="size-3.5" />
              Maintenance
            </Link>
            <button
              type="button"
              onClick={() => void runCheck()}
              disabled={running}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
            >
              <RefreshCw
                aria-hidden="true"
                className={`size-3.5 ${running ? "animate-spin" : ""}`}
              />
              {running ? "Checking…" : "Check for updates"}
            </button>
          </div>
        </section>
      ) : null}

      {checks ? (
        <CheckResultBanner checks={checks} />
      ) : null}

      {/* The cards. Skeletons while loading are the *card* skeletons the spec asks for, not a
          table skeleton: a card grid loading into a table shape is a visible jump, and the
          loading state here is a different component entirely. */}
      {data && data.environments.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="No environment has reported yet"
            hint="The health probe writes one row per environment. Until it has run, there is nothing to show here — and nothing is being invented to fill the gap."
            action={
              <button
                type="button"
                onClick={() => void load()}
                className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
              >
                <RefreshCw aria-hidden="true" className="size-3.5" />
                Check again
              </button>
            }
          />
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
          {data?.environments.map((card) => (
            <EnvironmentCard key={card.environment} card={card} onRollback={setRollback} />
          ))}
        </div>
      )}

      {/* The rollback dialog, mounted once for the whole page (REQ-024, slice 3). It closes into
          the history screen, so the operator's next question — "what happened to it" — is one
          click away rather than three. */}
      <RollbackDialog target={rollback} onClose={() => setRollback(null)} />
    </div>
  );
}

/** One environment card. */
function EnvironmentCard({
  card,
  onRollback,
}: {
  card: DeploymentEnvironmentCard;
  /** Opens the rollback dialog for this card's environment (REQ-024, slice 3). */
  onRollback: (target: RollbackTarget) => void;
}) {
  const tooltip = [
    card.checked_at ? `Last probe ${formatTimestamp(card.checked_at)}` : "No probe has run yet",
    card.failing_probe,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <article className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-5 py-4">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex flex-col gap-1">
          <h2 className="text-[15px] font-medium">{card.name}</h2>
          <span title={tooltip}>
            <HealthDot health={card.health} />
          </span>
        </div>
        <div className="text-right">
          <div className="flex flex-col items-end gap-0.5">
            <span className="text-[11px] font-medium tracking-wide text-muted uppercase">
              Version
            </span>
            <span className="text-[15px] leading-tight font-medium tabular-nums">
              {card.version ?? "—"}
            </span>
          </div>
          <div className="mt-2 flex flex-col items-end gap-0.5">
            <span className="text-[11px] font-medium tracking-wide text-muted uppercase">
              Available
            </span>
            {/* Right-aligned so the two columns of every card line up: the spec's visual check
                asks for Version/Available aligned on every card, and left-aligning one inside
                a right-aligned block is what breaks that. */}
            <AvailableLine availability={card.availability} text={card.available} />
          </div>
        </div>
      </header>

      {card.failing_probe ? (
        <p className="rounded-lg border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-[12.5px] text-amber-900 dark:text-amber-200">
          {card.failing_probe}
        </p>
      ) : null}

      {card.blocked_reason ? (
        <p className="text-[12.5px] text-muted">{card.blocked_reason}</p>
      ) : null}

      {card.last_deploy ? (
        <p className="text-[12px] text-muted">
          Last deploy {formatTimestamp(card.last_deploy.started_at)} ·{" "}
          {formatDuration(card.last_deploy.duration_ms)}
          {card.last_deploy.started_by ? "" : " · actor not recorded"}
        </p>
      ) : (
        <p className="text-[12px] text-muted">No deploy has been recorded for this environment.</p>
      )}

      <footer className="flex flex-wrap items-center gap-2 border-t border-line pt-3.5">
        {card.availability.state === "upgrade" ? (
          <Link
            href={`/deployment/releases/${encodeURIComponent(card.availability.version)}`}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
          >
            <ScrollText aria-hidden="true" className="size-3.5" />
            View Changes
          </Link>
        ) : (
          // Present and disabled, never absent: an operator who cannot see the button does not
          // know the action exists, and a disabled one with this reason tells them why.
          <span
            aria-disabled="true"
            title="There is no release on this channel to view changes for."
            className="inline-flex cursor-not-allowed items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-muted opacity-60"
          >
            <ScrollText aria-hidden="true" className="size-3.5" />
            View Changes
          </span>
        )}

        {/* A live `Deploy` needs a version to deploy *to*, and the card's own `available` line
            is prose ("2.5.0", "— (up to date)", "2.6.0-rc.1 needs core 2.7.0"). Only the enum
            says whether there is something to press, and only an upgrade the server offers has a
            target — so the link is built from `availability`, never by parsing that line. */}
        {card.deployable && card.availability.state === "upgrade" ? (
          <Link
            href={`/deployment/deploy?to=${encodeURIComponent(card.availability.version)}&environment=${encodeURIComponent(card.environment)}`}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-ink hover:bg-panel"
          >
            <Rocket aria-hidden="true" className="size-3.5" />
            Deploy
          </Link>
        ) : (
          <span
            aria-disabled="true"
            title={card.blocked_reason ?? "There is no release this environment may upgrade to."}
            className="inline-flex cursor-not-allowed items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-muted opacity-60"
          >
            <Rocket aria-hidden="true" className="size-3.5" />
            Deploy
          </span>
        )}

        {/* A live `Rollback` needs somewhere to roll back *to*, and only the card's own
            `rollback` object knows it — derived server-side from the last deploy's
            `from_version`. With no previous known-good version there is nothing to offer, and
            the button says so instead of opening a dialog with an empty target. */}
        {card.rollback ? (
          <button
            type="button"
            onClick={() => onRollback({ environment: card.environment, toVersion: card.rollback!.to_version })}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-ink hover:bg-panel"
          >
            <Undo2 aria-hidden="true" className="size-3.5" />
            Rollback
          </button>
        ) : (
          <span
            aria-disabled="true"
            title={
              "No previous known-good version to roll back to. This environment has only " +
              "ever been deployed once, so there is nothing before the current version."
            }
            className="inline-flex cursor-not-allowed items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-muted opacity-60"
          >
            <Undo2 aria-hidden="true" className="size-3.5" />
            Rollback
          </span>
        )}

        <Link
          href={`/deployment/history?environment=${encodeURIComponent(card.environment)}`}
          className="ml-auto inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
        >
          History
          <ArrowRight aria-hidden="true" className="size-3.5" />
        </Link>
      </footer>
    </article>
  );
}

/** What the just-run check found, or why it could not be run. */
function CheckResultBanner({ checks }: { checks: DeploymentCheckRunResponse }) {
  if (checks.result.state === "failed") {
    return (
      <p
        role="alert"
        className="rounded-xl border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-[12.5px] text-amber-900 dark:text-amber-200"
      >
        The check did not complete: {checks.result.reason}. The release list is still the cached
        data from the last successful run.
      </p>
    );
  }
  const announced = checks.announced;
  return (
    <p
      role="status"
      className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px]"
    >
      {announced.length === 0
        ? `The check read ${checks.result.seen} release(s) and found nothing new.`
        : `New release(s): ${announced.join(", ")}. The cards above now show the newest one this installation may take.`}
    </p>
  );
}
