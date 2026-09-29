"use client";

/**
 * The backup centre's overview (REQ-013, slice 1).
 *
 * Six things on this screen are not decoration, and each exists because the shortcut is wrong:
 *
 * - **The last-successful card says "never" rather than hiding itself.** A backup centre
 *   whose status card is absent on a fresh installation reads as a screen that has not
 *   loaded; one that says "no successful backup has ever run" is the alarm, and it is the
 *   single most important sentence on the page.
 * - **`partial` is rendered as its own outcome with its own colour.** It is not a failure —
 *   four of five artifacts are on the destination and an operator can still restore from
 *   them — and it is not a success, because the fifth part is not there. A screen that maps
 *   it to either word teaches the reader to stop reading it.
 * - **A part that produced nothing shows `0 items, 0 bytes, done`.** That is not an empty
 *   cell: `plugins` is empty by design until a package installer exists, and the row that
 *   says so is the difference between an honest platform and a broken one.
 * - **The destination card carries the probe's own sentence.** "Destination healthy" is a
 *   claim; "Wrote and removed a test file at /var/lib/omnion/backups/probe" is a fact, and
 *   when the probe failed the reason is the operating system's own words, not "unwritable".
 * - **Encryption is stated even when it is off.** A settings screen that shows a dropdown
 *   and nothing else lets an operator believe archives are encrypted because encryption is a
 *   feature the product has.
 * - **Delete asks, and the confirmation names the backup.** A restore point that disappears
 *   without a word is indistinguishable from one that was never taken.
 * - **Delete takes the bytes, and the result says how many.** The confirmation states that
 *   the artifacts come off the destination with the row, because the alternative — a green
 *   list over a directory still full of the media library — is a backup root that costs
 *   money per byte forever. After the call the notice carries the *actual* counts from the
 *   server: an entry that could not be removed is named with the operating system's own
 *   words rather than hidden behind a cheerful "removed". A row whose directory was never
 *   there says so; that is a different fact from "twelve files were deleted".
 */
import { useCallback, useEffect, useState } from "react";

import {
  CheckCircle2,
  ClipboardCopy,
  Database,
  HardDrive,
  Loader2,
  Package,
  Palette,
  Play,
  Plus,
  ShieldAlert,
  Trash2,
  TriangleAlert,
  Unplug,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createBackup,
  deleteBackup,
  fetchBackup,
  fetchBackups,
  fetchBackupStatus,
  sweepBackups,
  verifyBackup,
} from "@/lib/api";
import { formatBytes } from "@/lib/format";
import type {
  BackupDetail,
  BackupPart,
  BackupRun,
  BackupStatus,
  BackupStatusCounts,
  BackupSweepReport,
} from "@/lib/types";

/** The five parts, with the icon each row carries. */
const PARTS: { name: string; blurb: string; Icon: typeof Database }[] = [
  {
    name: "database",
    blurb: "Every platform table, with its row count",
    Icon: Database,
  },
  {
    name: "media",
    blurb: "The library's objects, per site",
    Icon: HardDrive,
  },
  {
    name: "configuration",
    blurb: "The shape and size of each settings table — never a value",
    Icon: ClipboardCopy,
  },
  {
    name: "themes",
    blurb: "The themes this platform knows about",
    Icon: Palette,
  },
  {
    name: "plugins",
    blurb: "Installed components — empty until a package installer exists",
    Icon: Package,
  },
];

/** The five terminal states, in the order the filter chips show them. */
const STATUSES = ["succeeded", "partial", "failed", "running", "queued"] as const;

/** The tone each state is drawn in. `partial` has its own, on purpose. */
const TONE: Record<string, string> = {
  succeeded: "bg-positive-soft text-positive",
  partial: "bg-caution-soft text-caution",
  failed: "bg-danger-soft text-danger",
  running: "bg-info-soft text-info",
  queued: "bg-quiet-soft text-muted",
};

/** The state a row's state pill carries. */
function StatePill({ status }: { status: string }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
        TONE[status] ?? "bg-quiet-soft text-muted"
      }`}
      data-testid="backup-state"
    >
      {status}
    </span>
  );
}

/** A card in the status row. */
function Card({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: string;
  hint?: string;
  tone?: "ok" | "warn" | "bad";
}) {
  const colour =
    tone === "bad" ? "text-danger" : tone === "warn" ? "text-caution" : "text-ink";
  return (
    <div
      className="rounded-xl border border-line bg-panel px-4 py-3"
      data-testid="backup-card"
    >
      <p className="text-[11px] uppercase tracking-wide text-muted">{label}</p>
      <p className={`mt-1 text-[19px] font-semibold ${colour}`}>{value}</p>
      {hint ? <p className="mt-0.5 text-[11.5px] text-muted">{hint}</p> : null}
    </div>
  );
}

/** "3 minutes ago", from an instant, or "never" when there is none. */
function age(instant: string | null): string {
  if (!instant) return "never";
  const then = Date.parse(instant);
  if (Number.isNaN(then)) return "unknown";
  const seconds = Math.max(0, Math.round((Date.now() - then) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/** A list screen is one `useEffect` and three pieces of state. */
export function BackupsOverviewScreen() {
  const [runs, setRuns] = useState<BackupRun[]>([]);
  const [total, setTotal] = useState(0);
  const [counts, setCounts] = useState<BackupStatusCounts | null>(null);
  const [status, setStatus] = useState<BackupStatus | null>(null);
  const [detail, setDetail] = useState<BackupDetail | null>(null);
  const [filter, setFilter] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  // The retention sweep is destructive and unattended, so the panel keeps its own state for
  // it rather than folding it into `busy`: the two buttons must not disable each other, and a
  // sweep that reports stranded artifacts needs a panel of its own to put them in.
  const [sweeping, setSweeping] = useState(false);
  const [sweep, setSweep] = useState<BackupSweepReport | null>(null);

  const reload = useCallback(() => {
    setLoading(true);
    setError(null);
    Promise.all([
      fetchBackups(filter ? { status: filter, limit: 50 } : { limit: 50 }),
      fetchBackupStatus(),
    ])
      .then(([list, cards]) => {
        setRuns(list.items);
        setTotal(list.total);
        setCounts(list.counts);
        setStatus(cards);
      })
      .catch((cause: unknown) => {
        setError(
          cause instanceof ApiError ? cause.message : "The backup list could not be loaded.",
        );
      })
      .finally(() => setLoading(false));
  }, [filter]);

  useEffect(() => {
    reload();
  }, [reload]);

  async function runNow() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await createBackup({ scopes: PARTS.map((part) => part.name) });
      const failed = result.parts.filter((part) => part.status === "failed");
      const done = result.parts.filter((part) => part.status === "done");
      // The result sentence names what actually happened, in three parts. "Backup created"
      // on its own is indistinguishable from a run where four parts failed.
      const sentence =
        failed.length === 0
          ? `Backup finished: all ${done.length} parts were written (${formatBytes(result.backup.size_bytes)}).`
          : `${done.length} of ${result.parts.length} parts were written. ${failed
              .map((part) => `${part.part}: ${part.error ?? "failed"}`)
              .join(" ")}`;
      setNotice(sentence);
      setDetail({ backup: result.backup, parts: result.parts, manifest: null });
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The backup could not be started.");
    } finally {
      setBusy(false);
      setCreating(false);
    }
  }

  async function open(id: string) {
    setError(null);
    try {
      setDetail(await fetchBackup(id));
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "That backup could not be read.");
    }
  }

  async function verify(id: string) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const verdict = await verifyBackup(id);
      // The verdict is the answer, not an exception. A `clean: false` renders as its own
      // sentence naming which parts are wrong.
      setNotice(verdict.summary);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The backup could not be verified.");
    } finally {
      setBusy(false);
    }
  }

  async function remove(id: string) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      // The server reports what it actually removed, and the notice is built from that
      // rather than from a template. The three answers are genuinely different facts and
      // collapsing them into "Backup removed" is what made the old behaviour survivable:
      // the row was gone, the archive was not, and the screen said nothing either way.
      const purge = await deleteBackup(id);
      const target = purge.root || "the destination";
      if (purge.failures.length > 0) {
        setNotice(
          `Backup removed, and ${purge.removed_entries} of ${
            purge.removed_entries + purge.failed_entries
          } entries came off ${target}. ${purge.failed_entries} could not be removed and are ` +
            `still there: ${purge.failures
              .map((failure) => `${failure.path} (${failure.reason})`)
              .join("; ")}`,
        );
      } else if (!purge.existed) {
        setNotice(`Backup removed. Nothing was on the destination at ${target}.`);
      } else {
        setNotice(
          `Backup removed with its artifacts — ${purge.removed_entries} ${
            purge.removed_entries === 1 ? "entry" : "entries"
          } deleted from ${target}.`,
        );
      }
      setConfirming(null);
      setDetail(null);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The backup could not be removed.");
    } finally {
      setBusy(false);
    }
  }

  /**
   * Run the retention sweep now, and report what it actually did.
   *
   * The report is kept rather than flattened into `notice`, because the three counts are
   * three different facts: "pruned 4" and "1 of those 4 left a file behind" and "the sweep
   * itself failed" reconcile three different ways, and a single green line can only carry
   * one of them. A sweep that found nothing is its own sentence too — it is not a failure
   * and not "pruned 0", and an operator who pressed the button deserves to know which.
   */
  async function runSweep() {
    setSweeping(true);
    setError(null);
    setNotice(null);
    try {
      const report = await sweepBackups();
      setSweep(report);
      if (report.failed > 0) {
        setError(
          `The retention sweep failed for ${report.failed} tenant${report.failed === 1 ? "" : "s"}. ` +
            `Its rows are untouched; nothing was deleted.`,
        );
      } else if (report.candidates === 0) {
        setNotice("Nothing to prune — no backup is past its retention window.");
      } else {
        setNotice(
          `Pruned ${report.removed + report.partial} of ${report.candidates} expired ` +
            `backup${report.candidates === 1 ? "" : "s"}: ${report.removed} fully, ` +
            `${report.partial} with files still on the destination.`,
        );
      }
      reload();
    } catch (cause) {
      setSweep(null);
      setError(
        cause instanceof ApiError ? cause.message : "The retention sweep could not be run.",
      );
    } finally {
      setSweeping(false);
    }
  }

  const chips: { key: string; label: string; value: number }[] = [
    { key: "", label: "All", value: total },
    ...STATUSES.map((key) => ({
      key,
      label: key,
      value: (counts?.[key as keyof BackupStatusCounts] as number | undefined) ?? 0,
    })),
  ];

  return (
    <div className="space-y-4" data-testid="backups-overview">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="max-w-2xl text-[12.5px] text-muted">
          A backup is five parts written to one destination and accounted for in a manifest.
          The card below is the only number that matters when something has gone wrong: how
          long ago the last run that actually produced artifacts finished.
        </p>
        <button
          type="button"
          onClick={() => setCreating(true)}
          disabled={busy}
          data-testid="backup-create"
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Plus className="h-3.5 w-3.5" />}
          Create backup
        </button>
      </div>

      {error ? (
        <p
          role="alert"
          className="rounded-lg border border-danger/30 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          className="rounded-lg border border-ok/30 bg-ok/5 px-3 py-2 text-[12.5px] text-ok"
        >
          {notice}
        </p>
      ) : null}

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Card
          label="Last successful backup"
          value={age(status?.last_successful_at ?? null)}
          tone={status?.last_successful_at ? undefined : "bad"}
          hint={
            status?.last_successful_at
              ? "The newest run that produced every artifact it asked for."
              : "No run has ever produced every artifact it asked for."
          }
        />
        <Card
          label="Next scheduled"
          value={status?.next_scheduled_at ? age(status.next_scheduled_at).replace(" ago", "") : "none"}
          hint={
            status?.next_scheduled_at
              ? "A schedule is enabled and waiting."
              : "No enabled schedule. Backups only happen when you press the button."
          }
        />
        <Card
          label="On the destination"
          value={formatBytes(status?.total_size_bytes ?? 0)}
          hint={`${status?.protected ?? 0} protected from pruning.`}
        />
        <Card
          label="Destination"
          value={status?.destination.writable ? "writable" : "not writable"}
          tone={status?.destination.writable ? "ok" : "bad"}
          hint={status?.destination.message ?? "Reading the destination…"}
        />
      </div>

      {/*
        The retention strip, below the cards rather than as a fifth one.

        A fifth card would push the grid to five columns and shrink every number on the page
        to make room for a button, and the button is the only part of this that is not a
        fact about the destination. It sits under the numbers it acts on instead.
      */}
      <div
        className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line bg-panel px-4 py-3"
        data-testid="backup-retention"
      >
        <div className="min-w-0">
          <p className="text-[13px] font-medium">Retention</p>
          <p className="mt-0.5 text-[12.5px] text-muted">
            Expired backups are swept off the destination every six hours, and the newest
            successful run plus anything protected are never swept. You can run it now.
          </p>
        </div>
        <button
          type="button"
          onClick={runSweep}
          disabled={sweeping}
          data-testid="backup-sweep"
          className="inline-flex shrink-0 items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium text-muted disabled:opacity-60"
        >
          {sweeping ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <Unplug className="h-3.5 w-3.5" />
          )}
          {sweeping ? "Sweeping…" : "Run retention now"}
        </button>
      </div>

      {sweep ? (
        <div
          className="rounded-xl border border-line bg-panel px-4 py-3"
          data-testid="backup-sweep-report"
          role="status"
        >
          <p className="text-[12.5px] font-medium">
            The sweep looked at {sweep.candidates} expired backup
            {sweep.candidates === 1 ? "" : "s"} and removed {sweep.removed} from{" "}
            {status?.destination.local_root ?? "the destination"}.
          </p>
          {sweep.partial > 0 ? (
            <p className="mt-1 text-[12.5px] text-caution">
              {sweep.partial} row{sweep.partial === 1 ? " is" : "s are"} gone but
              {sweep.partial === 1 ? " its" : " their"} artifacts could not all be removed.
              They are still on the destination and have to be cleared by hand.
            </p>
          ) : null}
          {sweep.stranded.length > 0 ? (
            <ul className="mt-2 space-y-1" data-testid="backup-sweep-stranded">
              {sweep.stranded.map((stranded) => (
                <li key={`${stranded.backup_id}-${stranded.path}`} className="text-[12px]">
                  <span className="font-mono text-muted">{stranded.path}</span>
                  <span className="text-muted"> — {stranded.reason}</span>
                </li>
              ))}
            </ul>
          ) : null}
          <p className="mt-2 text-[12px] text-muted">
            Run at {sweep.at}. Nothing protected and no newer successful backup was touched.
          </p>
        </div>
      ) : null}

      {status && status.destination.encryption === "none" ? (
        <p
          className="flex items-start gap-2 rounded-lg border border-caution/30 bg-caution/5 px-3 py-2 text-[12.5px] text-caution"
          data-testid="backup-encryption-warning"
        >
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>
            Archives are stored <strong>unencrypted</strong>. Anyone who can read the
            destination root can read every page, every file and every setting this platform
            holds. Encryption arrives with slice 4.
          </span>
        </p>
      ) : null}

      {creating ? (
        <div
          className="rounded-xl border border-line bg-panel px-4 py-3"
          data-testid="backup-create-drawer"
        >
          <p className="text-[13px] font-medium">Take a backup of all five parts</p>
          <p className="mt-1 text-[12.5px] text-muted">
            The run happens now and this screen waits for it. A part that cannot be produced is
            reported by name rather than rolled into the others.
          </p>
          <ul className="mt-2 space-y-1">
            {PARTS.map(({ name, blurb, Icon }) => (
              <li key={name} className="flex items-start gap-2 text-[12.5px]">
                <Icon className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted" />
                <span>
                  <strong className="font-medium">{name}</strong>
                  <span className="text-muted"> — {blurb}</span>
                </span>
              </li>
            ))}
          </ul>
          <div className="mt-3 flex gap-2">
            <button
              type="button"
              onClick={runNow}
              disabled={busy}
              data-testid="backup-create-confirm"
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
            >
              {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Play className="h-3.5 w-3.5" />}
              {busy ? "Running…" : "Run now"}
            </button>
            <button
              type="button"
              onClick={() => setCreating(false)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
            >
              Cancel
            </button>
          </div>
        </div>
      ) : null}

      {detail ? <BackupDetailPanel detail={detail} onVerify={verify} busy={busy} onClose={() => setDetail(null)} /> : null}

      <div className="flex flex-wrap items-center gap-1.5">
        {chips.map((chip) => (
          <button
            key={chip.key || "all"}
            type="button"
            onClick={() => setFilter(chip.key)}
            data-testid={`backup-filter-${chip.key || "all"}`}
            aria-pressed={filter === chip.key}
            className={`rounded-full px-2.5 py-1 text-[11.5px] ${
              filter === chip.key
                ? "bg-accent text-white"
                : "border border-line text-muted hover:text-ink"
            }`}
          >
            {chip.label}
            <span className="ml-1 opacity-70">{chip.value}</span>
          </button>
        ))}
      </div>

      {loading ? (
        <LoadingTable columns={7} rows={4} />
      ) : runs.length === 0 ? (
        <EmptyState
          title={filter ? `No ${filter} backups` : "No backup has been taken yet"}
          hint={
            filter
              ? "Clear the filter to see the runs in every state — a filter that hides a failed run is how a failed run survives."
              : "The first backup writes five parts: the database, the media library, the shape of every settings table, the themes and the components. It is the only way to get this platform back after the volume dies."
          }
          action={
            filter ? (
              <button
                type="button"
                onClick={() => setFilter("")}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
              >
                Show every run
              </button>
            ) : (
              <button
                type="button"
                onClick={() => setCreating(true)}
                data-testid="backup-empty-create"
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                <Play className="h-3.5 w-3.5" />
                Create the first backup
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full min-w-[860px] text-left text-[12.5px]">
            <thead className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
              <tr>
                <th className="px-3 py-2 font-medium">Created</th>
                <th className="px-3 py-2 font-medium">Parts</th>
                <th className="px-3 py-2 font-medium">Size</th>
                <th className="px-3 py-2 font-medium">Status</th>
                <th className="px-3 py-2 font-medium">Destination</th>
                <th className="px-3 py-2 font-medium">Retain until</th>
                <th className="px-3 py-2 font-medium" />
              </tr>
            </thead>
            <tbody>
              {runs.map((run) => (
                <tr key={run.id} className="border-b border-line last:border-0">
                  <td className="px-3 py-2">
                    <button
                      type="button"
                      onClick={() => open(run.id)}
                      data-testid="backup-row"
                      className="text-left hover:underline"
                    >
                      {run.title}
                    </button>
                    <span className="ml-1.5 text-[11px] text-muted">{run.kind}</span>
                  </td>
                  <td className="px-3 py-2 text-muted">{run.scopes.length}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{formatBytes(run.size_bytes)}</td>
                  <td className="px-3 py-2">
                    <StatePill status={run.status} />
                    {run.protected ? (
                      <span className="ml-1.5 inline-flex items-center gap-1 text-[11px] text-muted">
                        <ShieldAlert className="h-3 w-3" />
                        protected
                      </span>
                    ) : null}
                  </td>
                  <td className="px-3 py-2 text-muted">{run.destination}</td>
                  <td className="px-3 py-2 text-muted">
                    {run.retain_until ? run.retain_until.slice(0, 10) : "—"}
                  </td>
                  <td className="px-3 py-2 text-right">
                    {confirming === run.id ? (
                      <span className="inline-flex items-center gap-1.5">
                        <button
                          type="button"
                          onClick={() => remove(run.id)}
                          disabled={busy}
                          data-testid="backup-delete-confirm"
                          title={
                            "Removes this run and its artifacts from the destination. This cannot be undone."
                          }
                          className="rounded-lg bg-danger px-2 py-1 text-[11.5px] font-medium text-white disabled:opacity-60"
                        >
                          Remove “{run.title}” and its artifacts
                        </button>
                        <button
                          type="button"
                          onClick={() => setConfirming(null)}
                          className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
                        >
                          Keep
                        </button>
                      </span>
                    ) : (
                      <button
                        type="button"
                        onClick={() => setConfirming(run.id)}
                        data-testid="backup-delete"
                        aria-label={`Delete ${run.title}`}
                        className="rounded-lg border border-line p-1.5 text-muted hover:text-danger"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="px-3 py-2 text-[11.5px] text-muted">
            Showing {runs.length} of {total}
          </p>
        </div>
      )}
    </div>
  );
}

/** The detail panel: the parts table with the manifest checksum beside it. */
function BackupDetailPanel({
  detail,
  onVerify,
  busy,
  onClose,
}: {
  detail: BackupDetail;
  onVerify: (id: string) => void;
  busy: boolean;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="rounded-xl border border-line bg-panel px-4 py-3" data-testid="backup-detail">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <p className="text-[13.5px] font-medium">{detail.backup.title}</p>
          <p className="mt-0.5 text-[12px] text-muted">
            {detail.backup.scopes.length} part(s) · {formatBytes(detail.backup.size_bytes)} ·{" "}
            {detail.backup.destination} ·{" "}
            {detail.backup.checksum
              ? `manifest ${detail.backup.checksum.slice(0, 16)}…`
              : "no manifest yet"}
          </p>
          {detail.backup.error ? (
            <p className="mt-1 text-[12px] text-danger">{detail.backup.error}</p>
          ) : null}
        </div>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            onClick={() => {
              void navigator.clipboard?.writeText(
                JSON.stringify(detail.manifest ?? {}, null, 2),
              );
              setCopied(true);
            }}
            data-testid="backup-copy-manifest"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
          >
            {copied ? <CheckCircle2 className="h-3 w-3" /> : <ClipboardCopy className="h-3 w-3" />}
            {copied ? "Copied" : "Copy manifest"}
          </button>
          <button
            type="button"
            onClick={() => onVerify(detail.backup.id)}
            disabled={busy}
            data-testid="backup-verify"
            className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted disabled:opacity-60"
          >
            {busy ? <Loader2 className="h-3 w-3 animate-spin" /> : <ShieldAlert className="h-3 w-3" />}
            Verify
          </button>
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
          >
            Close
          </button>
        </div>
      </div>

      <table className="mt-3 w-full text-left text-[12px]">
        <thead className="text-[11px] uppercase tracking-wide text-muted">
          <tr>
            <th className="py-1 font-medium">Part</th>
            <th className="py-1 font-medium">Status</th>
            <th className="py-1 text-right font-medium">Items</th>
            <th className="py-1 text-right font-medium">Size</th>
            <th className="py-1 font-medium">Note</th>
          </tr>
        </thead>
        <tbody>
          {detail.parts.map((part: BackupPart) => (
            <tr key={part.part} data-testid="backup-part-row">
              <td className="py-1 font-medium">{part.part}</td>
              <td className="py-1">
                <StatePill status={part.status} />
              </td>
              <td className="py-1 text-right tabular-nums">{part.item_count}</td>
              <td className="py-1 text-right tabular-nums">{formatBytes(part.size_bytes)}</td>
              <td className="py-1 text-muted">
                {part.error ??
                  (part.item_count === 0 && part.status === "done"
                    ? "nothing to record — this is a result, not a gap"
                    : (part.checksum?.slice(0, 12) ?? "—"))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
