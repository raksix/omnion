"use client";

/**
 * `/deployment/exports` — anonymised support exports (REQ-129, slice 4).
 *
 * ## The four states are the screen, not the buttons
 *
 * An export is queued, running, ready, expired, revoked, downloaded or failed, and most of the
 * time it is NOT ready for a download. The single-use guarantee is implemented by the API
 * incrementing `download_count` inside the claim statement, so a second press gets a bare
 * `410` whose message is one of four different sentences. A panel that only knows "download
 * failed" turns that into a support ticket. So each row says which of the four it is BEFORE
 * anyone presses, and the download control is replaced by the explanation where it cannot
 * work — the same rule the seeds screen follows for a production load.
 *
 * ## The unclassified list is the headline
 *
 * The builder fails closed: a selected column with no classification row refuses the whole
 * export, naming every missing column at once. On a fresh installation that means nothing can be
 * exported until somebody classifies the columns — so the count sits at the top as the number
 * that decides whether this feature works at all, and each entry is classifiable in place.
 *
 * Keyboard: `r` refreshes, `Esc` closes a dialog.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import {
  AlertTriangle,
  Ban,
  CheckCircle2,
  Download,
  FileWarning,
  KeyRound,
  Lock,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  classifyColumn,
  createExport,
  listClassifications,
  listExports,
  revokeExport,
  runExport,
  exportDownloadUrl,
  type ClassificationList,
  type ExportList,
  type SupportExport,
} from "@/lib/deployment-api";
import { formatBytes, formatTimestamp } from "@/lib/format";

/**
 * Why a download cannot happen, derived from the row the API already sent.
 *
 * Deliberately the same four cases `download_export` names, in the same order, so the panel and
 * the 410 can never disagree about why. `expired` is computed rather than read: the API flips
 * ready rows to `expired` inside `GET /exports`, so a row whose `expires_at` has passed but whose
 * sweep has not run yet is still `ready` — and telling an operator "ready, download it" when the
 * claim statement will refuse it is the exact failure this avoids.
 */
function blockedReason(
  entry: SupportExport,
  now: number,
): { code: string; detail: string } | null {
  if (entry.revoked_at !== null || entry.status === "revoked") {
    return {
      code: "revoked",
      detail: `Revoked${entry.revoked_at ? ` on ${formatTimestamp(entry.revoked_at)}` : ""}. The link is dead and a revoke cannot be undone.`,
    };
  }
  if (entry.status === "expired" || new Date(entry.expires_at).getTime() <= now) {
    return {
      code: "expired",
      detail: `Expired on ${formatTimestamp(entry.expires_at)}. Ask for a new export rather than reusing this one.`,
    };
  }
  if (entry.download_count > 0) {
    return {
      code: "downloaded",
      detail: `Downloaded once${entry.last_downloaded_at ? ` on ${formatTimestamp(entry.last_downloaded_at)}` : ""}. Single-use by design — the file cannot be fetched a second time.`,
    };
  }
  if (entry.status !== "ready") {
    return { code: entry.status, detail: `This export is \`${entry.status}\`, not \`ready\`.` };
  }
  return null;
}

/** Status → the words and the shape the row wears. Never colour alone: every cell has a word. */
function statusBadge(status: string) {
  switch (status) {
    case "ready":
      return { label: "Ready", className: "border-emerald-300 bg-emerald-50 text-emerald-900 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-200" };
    case "queued":
      return { label: "Queued", className: "border-line bg-quiet-soft text-muted" };
    case "running":
      return { label: "Running", className: "border-blue-300 bg-blue-50 text-blue-900 dark:border-blue-800 dark:bg-blue-950/40 dark:text-blue-200" };
    case "failed":
      return { label: "Failed", className: "border-red-300 bg-red-50 text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200" };
    case "expired":
    case "revoked":
      return { label: status === "expired" ? "Expired" : "Revoked", className: "border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200" };
    default:
      return { label: status, className: "border-line bg-quiet-soft text-muted" };
  }
}

export function ExportsView() {
  const [list, setList] = useState<ExportList | null>(null);
  const [map, setMap] = useState<ClassificationList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  // The column being classified, and the form for it. `null` is the single closed value.
  const [asking, setAsking] = useState<{ table: string; column: string } | null>(null);
  const [classChoice, setClassChoice] = useState("");
  const [actionChoice, setActionChoice] = useState("");
  const [notes, setNotes] = useState("");
  const [formError, setFormError] = useState<string | null>(null);
  const notesRef = useRef<HTMLTextAreaElement | null>(null);

  // The plan form. Tables are typed rather than picked from a list because the API reads
  // information_schema itself and refuses anything it cannot classify; a curated picker would be
  // a second source of truth that drifts from the tables actually present.
  const [planning, setPlanning] = useState(false);
  const [reason, setReason] = useState("");
  const [tables, setTables] = useState("");
  const [rowLimit, setRowLimit] = useState("");
  const [planError, setPlanError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const [exports, classifications] = await Promise.all([listExports(), listClassifications()]);
      setList(exports);
      setMap(classifications);
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The exports could not be loaded.",
      );
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
      if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.tagName === "SELECT") return;
      if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "Escape" && asking) {
        event.preventDefault();
        setAsking(null);
        setFormError(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, asking]);

  useEffect(() => {
    if (asking) notesRef.current?.focus();
  }, [asking]);

  /** Submit a classification. The API refuses a blank note; the form refuses it first. */
  const submitClassification = useCallback(async () => {
    if (!asking) return;
    if (!notes.trim()) {
      setFormError(
        "A classification needs a reason: it is what the next reviewer reads to decide whether this still holds.",
      );
      return;
    }
    setFormError(null);
    setBusy(`${asking.table}.${asking.column}`);
    try {
      await classifyColumn({
        table_name: asking.table,
        column_name: asking.column,
        class: classChoice,
        default_action: actionChoice,
        notes: notes.trim(),
      });
      setNotice(`Classified ${asking.table}.${asking.column} as ${classChoice} → ${actionChoice}.`);
      setAsking(null);
      setNotes("");
      await load();
    } catch (caught) {
      setFormError(
        caught instanceof ApiError ? caught.message : "The column could not be classified.",
      );
    } finally {
      setBusy(null);
    }
  }, [asking, classChoice, actionChoice, notes, load]);

  const planExport = useCallback(async () => {
    if (!reason.trim()) {
      setPlanError("An export needs a reason: it is the only sentence a reviewer reads later.");
      return;
    }
    const chosen = tables
      .split(/[\s,]+/)
      .map((name) => name.trim())
      .filter(Boolean);
    if (chosen.length === 0) {
      setPlanError("Name at least one table. A table with an unclassified column refuses the export.");
      return;
    }
    const limit = rowLimit.trim() ? Number(rowLimit.trim()) : null;
    if (limit !== null && (!Number.isFinite(limit) || limit <= 0)) {
      setPlanError("The row limit must be a positive number, or left empty.");
      return;
    }
    setPlanError(null);
    setBusy("plan");
    try {
      const created = await createExport({
        reason: reason.trim(),
        tables: chosen,
        row_limit: limit,
      });
      setNotice(
        `Export planned: ${created.plan.columns.length} columns across ${created.plan.tables.length} table(s) — ${created.plan.removed} removed, ${created.plan.hashed} hashed, ${created.plan.synthetic} synthetic, ${created.plan.kept} kept.`,
      );
      setPlanning(false);
      setReason("");
      setTables("");
      setRowLimit("");
      await load();
    } catch (caught) {
      setPlanError(caught instanceof ApiError ? caught.message : "The export could not be planned.");
    } finally {
      setBusy(null);
    }
  }, [reason, tables, rowLimit, load]);

  const produce = useCallback(
    async (entry: SupportExport) => {
      setBusy(entry.id);
      setNotice(null);
      try {
        const result = await runExport(entry.id);
        setNotice(
          `File produced: ${formatBytes(result.file_size)}, checksum ${result.checksum.slice(0, 16)}…`,
        );
        await load();
      } catch (caught) {
        setNotice(
          caught instanceof ApiError
            ? `${caught.message} — the export is marked failed and its error is on the row.`
            : "The file could not be produced.",
        );
        await load();
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  const revoke = useCallback(
    async (entry: SupportExport) => {
      setBusy(entry.id);
      setNotice(null);
      try {
        await revokeExport(entry.id);
        setNotice(`Export revoked. The audit row stays; the link is dead.`);
        await load();
      } catch (caught) {
        setNotice(caught instanceof ApiError ? caught.message : "The export could not be revoked.");
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  if (loading) return <LoadingTable columns={5} rows={3} />;

  const unclassified = map?.unclassified ?? [];
  const rows = (list?.exports ?? []).filter((entry) =>
    filter ? entry.status === filter : true,
  );
  // One `now` for the whole render: comparing `expires_at` against a clock read per row would
  // let one row say "expired" and its neighbour "ready" inside the same frame.
  const now = Date.now();

  return (
    <div className="flex flex-col gap-5" data-view="deployment-exports">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>
            {error}{" "}
            <button type="button" onClick={() => void load()} className="inline-flex underline">
              Try again
            </button>
          </span>
        </div>
      ) : null}

      {notice ? (
        <p role="status" className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      {/* The number that decides whether this feature works at all. */}
      <div
        className={`flex items-start gap-2.5 rounded-md border px-3 py-2.5 text-[12.5px] ${
          unclassified.length === 0
            ? "border-emerald-300 bg-emerald-50 text-emerald-900 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-200"
            : "border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
        }`}
      >
        {unclassified.length === 0 ? (
          <CheckCircle2 size={15} aria-hidden className="mt-0.5 shrink-0" />
        ) : (
          <ShieldCheck size={15} aria-hidden className="mt-0.5 shrink-0" />
        )}
        <span>
          <span className="font-medium text-foreground">
            {unclassified.length === 0
              ? "Every public column is classified."
              : `${unclassified.length} unclassified column${unclassified.length === 1 ? "" : "s"}.`}
          </span>{" "}
          {unclassified.length === 0
            ? "An export can be planned for any table in this installation."
            : "An export that touches one of them is refused, and the refusal names every missing column at once. Classify them below."}
        </span>
      </div>

      {/* The classification map. Only the unclassified half is actionable here; the reviewed half
          is what makes the map an audit surface, so it is shown with its reviewer. */}
      {map ? (
        <section aria-labelledby="classify-heading" className="flex flex-col gap-2">
          <h2 id="classify-heading" className="text-[13px] font-medium">
            Column classifications
          </h2>
          {unclassified.length === 0 ? (
            <p className="text-[12.5px] text-muted">
              {map.classifications.length} column{map.classifications.length === 1 ? "" : "s"} reviewed.
            </p>
          ) : (
            <ul className="flex flex-wrap gap-1.5">
              {unclassified.map((name) => {
                const [table, column] = name.split(".");
                return (
                  <li key={name}>
                    <button
                      type="button"
                      onClick={() => {
                        setAsking({ table, column });
                        setClassChoice("personal");
                        setActionChoice("hash");
                        setNotes("");
                        setFormError(null);
                      }}
                      className="inline-flex items-center gap-1.5 rounded-md border border-amber-300 bg-amber-50 px-2.5 py-1.5 font-mono text-[11.5px] text-amber-900 hover:bg-amber-100 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
                    >
                      <KeyRound size={12} aria-hidden />
                      {name}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}

          {map.classifications.length > 0 ? (
            <details className="rounded-md border border-line bg-surface px-3 py-2">
              <summary className="cursor-pointer text-[12.5px] font-medium">
                {map.classifications.length} reviewed column
                {map.classifications.length === 1 ? "" : "s"} (who decided, and when)
              </summary>
              <div className="mt-2 overflow-x-auto">
                <table className="w-full border-collapse text-left text-[12px]">
                  <thead>
                    <tr className="border-b border-line text-[11.5px] text-muted">
                      <th className="px-2 py-1.5 font-normal">Column</th>
                      <th className="px-2 py-1.5 font-normal">Class</th>
                      <th className="px-2 py-1.5 font-normal">Action</th>
                      <th className="px-2 py-1.5 font-normal">Reviewed by</th>
                      <th className="px-2 py-1.5 font-normal">When</th>
                    </tr>
                  </thead>
                  <tbody>
                    {map.classifications.map((row) => (
                      <tr key={`${row.table}.${row.column}`} className="border-b border-line">
                        <td className="px-2 py-1.5 font-mono">{row.table}.{row.column}</td>
                        <td className="px-2 py-1.5">{row.class}</td>
                        <td className="px-2 py-1.5">{row.default_action}</td>
                        <td className="px-2 py-1.5 text-muted">{row.reviewed_by ?? "—"}</td>
                        <td className="px-2 py-1.5 text-muted">
                          {row.reviewed_at ? formatTimestamp(row.reviewed_at) : "—"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </details>
          ) : null}
        </section>
      ) : null}
      {/* The exports themselves. */}
      <section aria-labelledby="exports-heading" className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="exports-heading" className="text-[13px] font-medium">
            Exports
          </h2>
          <div className="flex items-center gap-2">
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <span className="sr-only">Filter by state</span>
              <select
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                className="rounded-md border border-line bg-background px-2 py-1.5 text-[12px]"
              >
                <option value="">Every state</option>
                <option value="queued">Queued</option>
                <option value="running">Running</option>
                <option value="ready">Ready</option>
                <option value="expired">Expired</option>
                <option value="revoked">Revoked</option>
                <option value="failed">Failed</option>
              </select>
            </label>
            <button
              type="button"
              onClick={() => void load()}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
            >
              <RefreshCw size={13} aria-hidden /> Refresh <kbd className="text-[10.5px]">r</kbd>
            </button>
            <button
              type="button"
              onClick={() => setPlanning((open) => !open)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
            >
              Plan an export
            </button>
          </div>
        </div>

        {list && list.expired_by_this_request > 0 ? (
          <p className="text-[12px] text-muted">
            {list.expired_by_this_request} export{list.expired_by_this_request === 1 ? "" : "s"}{" "}
            expired when this page loaded.
          </p>
        ) : null}

        {planning ? (
          <div className="rounded-md border border-line bg-surface p-4">
            <h3 className="text-[13px] font-medium">Plan an anonymised export</h3>
            <p className="mt-1 text-[12px] text-muted">
              Planning does not produce the file — it resolves every selected column against the
              classification map and records the decision. A column with no classification refuses
              the whole export here, before anything is read.
            </p>
            <label className="mt-3 block">
              <span className="text-[12px] font-medium">Reason</span>
              <input
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                placeholder="Why this export exists — the sentence a reviewer reads later"
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 text-[13px] outline-none focus:border-accent"
              />
            </label>
            <label className="mt-3 block">
              <span className="text-[12px] font-medium">Tables</span>
              <input
                value={tables}
                onChange={(event) => setTables(event.target.value)}
                placeholder="users, orders — separated by comma or space"
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 font-mono text-[13px] outline-none focus:border-accent"
              />
            </label>
            <label className="mt-3 block">
              <span className="text-[12px] font-medium">
                Row limit per table{" "}
                <span className="font-normal text-muted">
                  (optional, up to {list?.limits.max_rows.toLocaleString() ?? "—"})
                </span>
              </span>
              <input
                value={rowLimit}
                onChange={(event) => setRowLimit(event.target.value)}
                inputMode="numeric"
                className="mt-1 w-40 rounded-md border border-line bg-background px-3 py-2 text-[13px] outline-none focus:border-accent"
              />
            </label>
            {planError ? (
              <p role="alert" className="mt-2 text-[12px] text-red-700 dark:text-red-300">
                {planError}
              </p>
            ) : null}
            <div className="mt-3 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setPlanning(false)}
                className="rounded-md border border-line px-3 py-2 text-[12.5px]"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={busy === "plan"}
                onClick={() => void planExport()}
                className="rounded-md bg-accent px-3 py-2 text-[12.5px] text-white disabled:opacity-60"
              >
                {busy === "plan" ? "Planning…" : "Plan this export"}
              </button>
            </div>
          </div>
        ) : null}

        {rows.length === 0 ? (
          <EmptyState
            title={
              list && list.exports.length > 0
                ? `No export is \`${filter}\``
                : "No export has been planned"
            }
            hint={
              list && list.exports.length > 0
                ? "Clear the filter to see the other states."
                : "An export is a support dump: tables you choose, columns anonymised by classification, one download before it expires."
            }
          />
        ) : (
          <ul className="flex flex-col gap-2">
            {rows.map((entry) => {
              const badge = statusBadge(entry.status);
              const blocked = blockedReason(entry, now);
              const canProduce = entry.status === "queued";
              const canRevoke = entry.status !== "revoked" && entry.revoked_at === null;
              return (
                <li
                  key={entry.id}
                  className="flex flex-col gap-2 rounded-md border border-line bg-surface p-4"
                >
                  <div className="flex flex-wrap items-start justify-between gap-2">
                    <div className="min-w-0">
                      <p className="text-[13.5px] font-medium">{entry.reason}</p>
                      <p className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-muted">
                        <span className="font-mono">{entry.tables.join(", ") || "no tables"}</span>
                        <span>{entry.requested_by_name}</span>
                        <span>{formatTimestamp(entry.created_at)}</span>
                      </p>
                    </div>
                    <span
                      className={`shrink-0 rounded-md border px-2 py-1 text-[11.5px] ${badge.className}`}
                    >
                      {badge.label}
                    </span>
                  </div>

                  {/* The watermark is the first line OF THE FILE, so showing it here is showing
                      what a recipient sees when they open it. */}
                  <p className="truncate rounded border border-line bg-quiet-soft px-2 py-1.5 font-mono text-[10.5px] text-muted">
                    {entry.watermark}
                  </p>

                  {entry.error ? (
                    <p className="flex items-start gap-2 rounded border border-red-300 bg-red-50 px-2 py-1.5 text-[11.5px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
                      <FileWarning size={13} aria-hidden className="mt-0.5 shrink-0" />
                      <span>{entry.error}</span>
                    </p>
                  ) : null}

                  {blocked ? (
                    <p
                      className="flex items-start gap-2 rounded border border-amber-300 bg-amber-50 px-2 py-1.5 text-[11.5px] text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
                    >
                      <Lock size={13} aria-hidden className="mt-0.5 shrink-0" />
                      <span>
                        <span className="font-medium">Download unavailable — {blocked.code}.</span>{" "}
                        {blocked.detail}
                      </span>
                    </p>
                  ) : null}

                  <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-[11.5px] sm:grid-cols-4">
                    <div>
                      <dt className="text-muted">Size</dt>
                      <dd>{entry.file_size === null ? "—" : formatBytes(entry.file_size)}</dd>
                    </div>
                    <div>
                      <dt className="text-muted">Expires</dt>
                      <dd>{formatTimestamp(entry.expires_at)}</dd>
                    </div>
                    <div>
                      <dt className="text-muted">Downloads</dt>
                      <dd>
                        {entry.download_count} of 1
                      </dd>
                    </div>
                    <div className="min-w-0">
                      <dt className="text-muted">Salt</dt>
                      <dd className="truncate font-mono">{entry.salt_fingerprint ?? "—"}</dd>
                    </div>
                  </dl>

                  <div className="mt-auto flex flex-wrap items-center justify-end gap-2 pt-1">
                    {canProduce ? (
                      <button
                        type="button"
                        disabled={busy === entry.id}
                        onClick={() => void produce(entry)}
                        className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-2 text-[12.5px] disabled:opacity-60"
                      >
                        {busy === entry.id ? "Producing…" : "Produce the file"}
                      </button>
                    ) : null}
                    {canRevoke ? (
                      <button
                        type="button"
                        disabled={busy === entry.id}
                        onClick={() => void revoke(entry)}
                        className="inline-flex items-center gap-1.5 rounded-md border border-red-300 px-3 py-2 text-[12.5px] text-red-700 disabled:opacity-60 dark:border-red-800 dark:text-red-300"
                      >
                        <Ban size={13} aria-hidden /> Revoke
                      </button>
                    ) : null}
                    {/* Where the download cannot work, there is no link at all. A control that is
                        there and refuses when pressed is the dead button the request forbids. */}
                    {!blocked ? (
                      <a
                        href={exportDownloadUrl(entry.id)}
                        download
                        className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-2 text-[12.5px] text-white"
                      >
                        <Download size={13} aria-hidden /> Download (spends the one download)
                      </a>
                    ) : null}
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {/* The classification dialog. Rendered only when open, so its inputs are never in the tab
          order of a screen that has nothing to classify. */}
      {asking ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          role="dialog"
          aria-modal="true"
          aria-labelledby="classify-heading"
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-surface p-5 shadow-xl">
            <h3 id="classify-heading" className="text-[15px] font-medium">
              Classify <span className="font-mono">{asking.table}.{asking.column}</span>
            </h3>
            <p className="mt-1.5 text-[12.5px] text-muted">
              What this column holds decides what an export does with it. A credential classified
              <code> keep</code> is refused — the API will not write that combination.
            </p>

            <label className="mt-4 block">
              <span className="text-[12px] font-medium">Class</span>
              <select
                value={classChoice}
                onChange={(event) => setClassChoice(event.target.value)}
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 text-[13px]"
              >
                {(map?.classes ?? ["personal", "secret", "identifier", "safe"]).map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
            </label>

            <label className="mt-3 block">
              <span className="text-[12px] font-medium">Action</span>
              <select
                value={actionChoice}
                onChange={(event) => setActionChoice(event.target.value)}
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 text-[13px]"
              >
                {(map?.actions ?? ["remove", "hash", "synthetic", "keep"]).map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
            </label>

            <label className="mt-3 block">
              <span className="text-[12px] font-medium">Why this classification</span>
              <textarea
                ref={notesRef}
                value={notes}
                onChange={(event) => {
                  setNotes(event.target.value);
                  setFormError(null);
                }}
                rows={3}
                placeholder="What the column holds, or that you checked it"
                className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 text-[13px] outline-none focus:border-accent"
              />
            </label>

            {formError ? (
              <p role="alert" className="mt-2 text-[12px] text-red-700 dark:text-red-300">
                {formError}
              </p>
            ) : null}

            <div className="mt-4 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setAsking(null)}
                className="rounded-md border border-line px-3 py-2 text-[13px]"
              >
                Cancel <kbd className="text-[10.5px] text-muted">Esc</kbd>
              </button>
              <button
                type="button"
                disabled={busy === `${asking.table}.${asking.column}`}
                onClick={() => void submitClassification()}
                className="rounded-md bg-accent px-3 py-2 text-[13px] text-white disabled:opacity-60"
              >
                Save classification
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
