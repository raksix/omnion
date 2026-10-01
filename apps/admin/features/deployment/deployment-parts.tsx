"use client";

/**
 * The shared pieces of the deployment centre (REQ-024).
 *
 * These are separate from the views because each of them is a claim the API makes and the panel
 * must not restate in its own words. The health dot's colour, the `Available` line, the cached
 * banner: all three come from the server as data, and a panel that formats them itself is a
 * panel that can be wrong about them in a way no test on the panel will catch.
 */

import { AlertTriangle, CircleCheck, CircleSlash, RefreshCw } from "lucide-react";

import type {
  DeploymentAvailability,
  DeploymentCheckRow,
  DeploymentHistoryRow,
  DeploymentStep,
} from "@/lib/types";

/**
 * The health dot.
 *
 * Icon **and** word, never the colour alone: the spec's visual check asks for "a health indicator
 * that is unmistakable with text (not colour alone)", and a green dot beside the word `Degraded`
 * would satisfy the letter of a colour-only design while telling the reader the opposite of the
 * truth. The three icons are different *shapes* as well as colours, so the state survives
 * greyscale printing and a red-green colourblind reader.
 */
export function HealthDot({ health }: { health: string }) {
  const state = healthLabel(health);
  const Icon = state.icon;
  return (
    <span className="inline-flex items-center gap-1.5" title={state.title}>
      <Icon
        aria-hidden="true"
        className={`size-3.5 shrink-0 ${state.className}`}
        strokeWidth={2.25}
      />
      <span className="text-[12.5px] font-medium">{state.label}</span>
    </span>
  );
}

/** The health dot's three states, as data. */
function healthLabel(health: string): {
  label: string;
  className: string;
  title: string;
  icon: typeof CircleCheck;
} {
  switch (health) {
    case "healthy":
      return {
        label: "Healthy",
        className: "text-emerald-600 dark:text-emerald-400",
        title: "The last probe answered for every dependency.",
        icon: CircleCheck,
      };
    case "degraded":
      return {
        label: "Degraded",
        className: "text-amber-600 dark:text-amber-400",
        title: "A dependency answered, but not the way it should.",
        icon: AlertTriangle,
      };
    default:
      return {
        label: "Unreachable",
        className: "text-red-600 dark:text-red-400",
        title: "The last probe did not answer at all.",
        icon: CircleSlash,
      };
  }
}

/**
 * The card's second line, rendered from the enum rather than the rendered string.
 *
 * The API sends both `available` (a finished sentence) and `availability` (the three states).
 * The string is what a screen reader and a copy-paste get; the enum is what decides whether the
 * value is a version or an explanation — and that distinction is the whole point, because a
 * blocked release printed as a bare version is a card offering a deploy it will refuse.
 */
export function AvailableLine({
  availability,
  text,
}: {
  availability: DeploymentAvailability;
  text: string;
}) {
  if (availability.state === "blocked") {
    return (
      <span className="inline-flex flex-wrap items-baseline gap-x-1.5">
        <span className="text-[15px] font-medium tabular-nums">{availability.candidate}</span>
        <span className="text-[12px] text-amber-700 dark:text-amber-300">
          — {availability.reason}
        </span>
      </span>
    );
  }
  if (availability.state === "up-to-date") {
    // The spec names this line: an empty `Available` field is the bug, and the reason it is a
    // bug is that an empty field reads as "we do not know" rather than "there is nothing".
    return <span className="text-[13.5px] text-muted">{text}</span>;
  }
  return (
    <span className="inline-flex flex-wrap items-baseline gap-x-1.5">
      <span className="text-[15px] font-medium tabular-nums">{availability.version}</span>
      {availability.breaking ? (
        <span className="rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 text-[11px] text-amber-700 dark:text-amber-300">
          breaking changes
        </span>
      ) : null}
    </span>
  );
}

/**
 * The cached-data banner, rendered verbatim.
 *
 * The wording comes from the API (`omnion_deployment::stale_banner`) because the spec names it:
 * "manifest feed unreachable — showing cached data from {time}". A panel-authored generic
 * error component cannot quote the cached timestamp, and an error message without it leaves the
 * reader unable to tell whether the data is a minute or a month old.
 */
export function StaleBanner({ text, onRetry }: { text: string; onRetry?: () => void }) {
  return (
    <div
      role="status"
      className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-xl border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-[12.5px] text-amber-900 dark:text-amber-200"
    >
      <AlertTriangle aria-hidden="true" className="size-4 shrink-0" />
      <span className="min-w-0 flex-1">{text}</span>
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          className="inline-flex items-center gap-1.5 rounded-lg border border-amber-500/50 px-2.5 py-1 text-[12px] font-medium hover:bg-amber-500/10"
        >
          <RefreshCw aria-hidden="true" className="size-3.5" />
          Check again
        </button>
      ) : null}
    </div>
  );
}

/** A duration in milliseconds, in the words an operator reads. */
export function formatDuration(durationMs: number | null): string {
  if (durationMs === null) return "—";
  if (durationMs < 1000) return `${durationMs} ms`;
  const seconds = durationMs / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  return `${minutes} min ${rest} s`;
}

/** A step's status, as a badge. */
export function StepBadge({ status }: { status: string }) {
  const map: Record<string, string> = {
    done: "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300 border-emerald-500/30",
    running: "bg-blue-500/10 text-blue-700 dark:text-blue-300 border-blue-500/30",
    failed: "bg-red-500/10 text-red-700 dark:text-red-300 border-red-500/30",
    skipped: "bg-quiet-soft text-muted border-line",
    pending: "bg-quiet-soft text-muted border-line",
  };
  return (
    <span
      className={`inline-block rounded-full border px-2 py-0.5 text-[11px] font-medium ${
        map[status] ?? "bg-quiet-soft text-muted border-line"
      }`}
    >
      {status}
    </span>
  );
}

/** A check row, in the same vocabulary the wizard uses. */
export function CheckRowLine({ row }: { row: DeploymentCheckRow }) {
  const tone =
    row.state === "fail"
      ? "border-red-500/40 text-red-700 dark:text-red-300"
      : row.state === "warn"
        ? "border-amber-500/40 text-amber-700 dark:text-amber-300"
        : row.state === "unknown"
          ? "border-line text-muted"
          : "border-emerald-500/30 text-emerald-700 dark:text-emerald-300";
  return (
    <li className="flex flex-col gap-1 border-t border-line py-2.5 first:border-t-0">
      <div className="flex flex-wrap items-center gap-2">
        <span className={`rounded-full border px-2 py-0.5 text-[11px] font-medium ${tone}`}>
          {row.state}
        </span>
        <span className="text-[13px] font-medium">{row.title}</span>
      </div>
      <p className="text-[12.5px] text-muted">{row.detail}</p>
      {row.suggestion ? (
        <p className="text-[12px] text-muted italic">{row.suggestion}</p>
      ) : null}
    </li>
  );
}

/** One step of a run, with its output in a pane of its own. */
export function StepLine({ step }: { step: DeploymentStep }) {
  return (
    <li className="flex flex-col gap-1.5 border-t border-line py-2.5 first:border-t-0">
      <div className="flex flex-wrap items-center gap-2">
        <span className="w-5 shrink-0 text-[11px] text-muted tabular-nums">{step.position + 1}</span>
        <span className="text-[13px] font-medium">{step.name}</span>
        <StepBadge status={step.status} />
      </div>
      {step.output ? (
        <pre className="max-h-40 overflow-auto rounded-lg bg-quiet-soft px-3 py-2 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap">
          {step.output}
        </pre>
      ) : null}
    </li>
  );
}

/** One history row's summary, without the steps — the expandable part is the caller's. */
export function HistoryRowSummary({ row }: { row: DeploymentHistoryRow }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12.5px]">
      <span className="font-medium">{row.environment}</span>
      <span className="text-muted">
        {row.from_version ?? "—"} → {row.to_version ?? "—"}
      </span>
      <span className="text-muted">{row.kind}</span>
      <StepBadge status={row.status} />
      <span className="text-muted tabular-nums">{formatDuration(row.duration_ms)}</span>
    </div>
  );
}
