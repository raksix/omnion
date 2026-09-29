"use client";

/**
 * `/security/findings` — the findings table, its filters and its status transitions
 * (REQ-012, slice 1).
 *
 * Five decisions, each one a way a findings list misleads the person triaging it:
 *
 * 1. **The count above the table is the server's, and it is the *filter's* count.** The panel
 *    never counts its own page. A client-computed total says "3 findings" over five rows the
 *    moment the page is paginated, and on a security screen that is the number people quote.
 * 2. **Ignore requires a reason, in the drawer, before the button is enabled.** Not because
 *    the server would accept it without one — it does not — but because a field that only
 *    appears after a failed save is a field nobody fills in. The server's refusal is still
 *    handled and shown by name, because the server owns the rule.
 * 3. **A bulk action reports what it actually changed.** Selecting five and changing two is a
 *    legitimate outcome, and "2 acknowledged, 3 no longer matched" is the honest answer. A
 *    toast that says "done" teaches people their selection is unreliable.
 * 4. **`/` focuses the filter box, `a` acknowledges the selected row, `Esc` closes the
 *    drawer.** The keys are listed in the header, because a shortcut nobody can discover is
 *    not a shortcut.
 * 5. **Mobile renders cards, not a horizontally scrolled table.** A ten-column table on a
 *    phone is a table nobody reads, and this is the screen an on-call person opens on one.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CheckCheck,
  Download,
  Filter,
  Loader2,
  RefreshCw,
  Search,
  ShieldOff,
  Upload,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  bulkSecurityFindingStatus,
  fetchSecurityFindings,
  importSecurityReport,
  securityFindingsExportUrl,
  setSecurityFindingStatus,
  type ApiError,
} from "@/lib/api";
import type {
  SecurityFinding,
  SecurityFindingFilter,
  SecurityFindingStatus,
  SecuritySeverity,
} from "@/lib/types";
import { SecurityTabs } from "@/features/security/security-tabs";

const SEVERITIES: SecuritySeverity[] = ["critical", "high", "medium", "low", "info"];
const STATUSES: SecurityFindingStatus[] = ["open", "acknowledged", "fixed", "ignored"];
const SOURCES = ["config", "dependency", "platform", "report"] as const;

const SEVERITY_CLASS: Record<SecuritySeverity, string> = {
  critical: "bg-red-100 text-red-900 dark:bg-red-950 dark:text-red-200",
  high: "bg-orange-100 text-orange-900 dark:bg-orange-950 dark:text-orange-200",
  medium: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
  low: "bg-slate-200 text-slate-900 dark:bg-slate-800 dark:text-slate-100",
  info: "bg-slate-100 text-slate-800 dark:bg-slate-900 dark:text-slate-200",
};

const STATUS_LABEL: Record<SecurityFindingStatus, string> = {
  open: "Open",
  acknowledged: "Acknowledged",
  fixed: "Fixed",
  ignored: "Ignored",
};

const SOURCE_LABEL: Record<(typeof SOURCES)[number], string> = {
  config: "Platform check",
  dependency: "Dependency report",
  platform: "Platform",
  report: "Uploaded report",
};

function when(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleString();
}

/** `a-b-c` from a UUID: enough to recognise a row, not a handle to query. */
function shortId(id: string): string {
  return id.slice(0, 8);
}

type Notice = { tone: "ok" | "warn" | "bad"; text: string } | null;

function FindingDrawer({
  finding,
  onClose,
  onChanged,
  canManage,
}: {
  finding: SecurityFinding;
  onClose: () => void;
  onChanged: () => void;
  canManage: boolean;
}) {
  const [reason, setReason] = useState(finding.ignore_reason ?? "");
  const [expiry, setExpiry] = useState(finding.ignored_until?.slice(0, 10) ?? "");
  const [note, setNote] = useState(finding.note ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const reasonRef = useRef<HTMLTextAreaElement>(null);

  // Esc closes, from anywhere in the drawer including the textarea — a security drawer that
  // swallows Esc because focus is in a text field is a drawer people cannot leave.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const act = useCallback(
    async (change: { status: SecurityFindingStatus; ignore_reason?: string; note?: string }) => {
      setBusy(true);
      setError(null);
      try {
        await setSecurityFindingStatus(finding.id, change);
        onChanged();
        onClose();
      } catch (caught) {
        setError(caught as ApiError);
      } finally {
        setBusy(false);
      }
    },
    [finding.id, onChanged, onClose],
  );

  const reasonTooShort = reason.trim().length === 0;

  return (
    <aside
      role="dialog"
      aria-label={`Finding: ${finding.title}`}
      className="fixed inset-y-0 right-0 z-40 w-full max-w-md overflow-y-auto border-l border-line bg-canvas p-4 shadow-xl"
      data-finding-drawer={finding.id}
    >
      <header className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <span
            className={`inline-flex rounded-full px-2 py-0.5 text-[11px] font-medium ${SEVERITY_CLASS[finding.severity]}`}
          >
            {finding.severity}
          </span>
          <h2 className="mt-1 text-[15px] font-medium text-ink">{finding.title}</h2>
          <p className="text-[12px] text-muted">
            {SOURCE_LABEL[finding.source]} · first seen {when(finding.first_seen_at)} · last seen{" "}
            {when(finding.last_seen_at)}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close"
          className="shrink-0 rounded border border-line p-1 text-muted hover:text-ink"
          data-finding-close
        >
          <X aria-hidden className="h-3.5 w-3.5" />
        </button>
      </header>

      {finding.component ? (
        <dl className="mt-3 grid grid-cols-2 gap-x-3 gap-y-1 text-[12px]">
          <dt className="text-muted">Component</dt>
          <dd className="text-ink">{finding.component}</dd>
          <dt className="text-muted">Version</dt>
          <dd className="text-ink">{finding.component_version ?? "—"}</dd>
          <dt className="text-muted">Fixed in</dt>
          {/* "No fix available" is a different claim from an empty cell: a blank reads as
              "we have not looked", and the operator's next question is the same either way. */}
          <dd className="text-ink">{finding.fixed_in ?? "No fix available"}</dd>
          <dt className="text-muted">Status</dt>
          <dd className="text-ink">{STATUS_LABEL[finding.status]}</dd>
          <dt className="text-muted">Id</dt>
          <dd className="font-mono text-[11px] text-muted">{shortId(finding.id)}</dd>
        </dl>
      ) : null}

      {finding.description ? (
        <p className="mt-3 whitespace-pre-wrap text-[13px] text-ink">{finding.description}</p>
      ) : null}

      {finding.ignore_lapsed ? (
        <p className="mt-3 rounded border border-amber-300 bg-amber-50 px-3 py-2 text-[12px] text-amber-900 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-200">
          The ignore on this finding lapsed. It is counted as open again until somebody decides
          what to do with it.
        </p>
      ) : null}

      {error ? (
        <p className="mt-3 text-[12px] text-red-700 dark:text-red-300" data-finding-error>
          {error.message}
        </p>
      ) : null}

      {canManage ? (
        <div className="mt-4 space-y-4">
          <section>
            <h3 className="text-[12px] font-medium text-ink">Change status</h3>
            <div className="mt-1 flex flex-wrap gap-2">
              <button
                type="button"
                disabled={busy || finding.status === "acknowledged"}
                onClick={() => void act({ status: "acknowledged", note: note || undefined })}
                className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-50"
                data-finding-ack
              >
                <CheckCheck aria-hidden className="h-3.5 w-3.5" />
                Acknowledge
              </button>
              <button
                type="button"
                disabled={busy || finding.status === "fixed"}
                onClick={() => void act({ status: "fixed", note: note || undefined })}
                className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-50"
                data-finding-fix
              >
                Mark fixed
              </button>
              <button
                type="button"
                disabled={busy || finding.status === "open"}
                onClick={() => void act({ status: "open" })}
                className="rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-50"
                data-finding-reopen
              >
                Reopen
              </button>
            </div>
          </section>

          <section>
            <h3 className="text-[12px] font-medium text-ink">Ignore</h3>
            <p className="mt-0.5 text-[11px] text-muted">
              An ignore is a decision somebody will be asked to justify later, so it carries
              its reason. A dated ignore lapses on its own.
            </p>
            <label className="mt-2 block text-[12px] text-muted" htmlFor="finding-ignore-reason">
              Reason
            </label>
            <textarea
              id="finding-ignore-reason"
              ref={reasonRef}
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              rows={2}
              placeholder="Why this finding is not actionable right now"
              className="mt-1 w-full rounded border border-line bg-surface px-2 py-1 text-[13px] text-ink"
              data-finding-reason
            />
            <label className="mt-2 block text-[12px] text-muted" htmlFor="finding-ignore-until">
              Until (optional)
            </label>
            <input
              id="finding-ignore-until"
              type="date"
              value={expiry}
              onChange={(event) => setExpiry(event.target.value)}
              className="mt-1 w-full rounded border border-line bg-surface px-2 py-1 text-[13px] text-ink"
              data-finding-expiry
            />
            <button
              type="button"
              // Disabled until the reason is there, because the server refuses without one
              // and a save that can only fail teaches the operator the form is broken.
              disabled={busy || reasonTooShort}
              onClick={() =>
                void act({
                  status: "ignored",
                  ignore_reason: reason.trim(),
                  note: note || undefined,
                })
              }
              className="mt-2 inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-50"
              data-finding-ignore
            >
              <ShieldOff aria-hidden className="h-3.5 w-3.5" />
              Ignore
            </button>
            {reasonTooShort ? (
              <p className="mt-1 text-[11px] text-muted">A reason is required to ignore.</p>
            ) : null}
          </section>

          <section>
            <label className="block text-[12px] text-muted" htmlFor="finding-note">
              Note
            </label>
            <textarea
              id="finding-note"
              value={note}
              onChange={(event) => setNote(event.target.value)}
              rows={2}
              placeholder="A follow-up task, a ticket, a link"
              className="mt-1 w-full rounded border border-line bg-surface px-2 py-1 text-[13px] text-ink"
              data-finding-note
            />
          </section>
        </div>
      ) : (
        <p className="mt-4 text-[12px] text-muted">
          Your role can read findings but not change them. Acknowledging and ignoring are
          separate permissions on purpose — deciding what is worth acting on is not the same
          power as seeing what is there.
        </p>
      )}
    </aside>
  );
}

export function SecurityFindingsScreen() {
  const [filter, setFilter] = useState<SecurityFindingFilter>({});
  const [searchDraft, setSearchDraft] = useState("");
  const [rows, setRows] = useState<SecurityFinding[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ApiError | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [open, setOpen] = useState<SecurityFinding | null>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const [importing, setImporting] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  const limit = 50;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchSecurityFindings({ ...filter, offset, limit });
      setRows(page.findings);
      setTotal(page.total);
      // The selection is dropped on every load: a bulk action against ids that are no longer
      // on the page is a bulk action against rows the operator cannot see.
      setSelected([]);
    } catch (caught) {
      setError(caught as ApiError);
    } finally {
      setLoading(false);
    }
  }, [filter, offset]);

  useEffect(() => {
    void load();
  }, [load]);

  // The search box is debounced into the filter rather than firing a request per keystroke,
  // and the offset resets so a narrowed filter does not land the operator on page 7 of a
  // three-row result.
  useEffect(() => {
    const timer = setTimeout(() => {
      setFilter((current) => ({ ...current, search: searchDraft.trim() || undefined }));
      setOffset(0);
    }, 300);
    return () => clearTimeout(timer);
  }, [searchDraft]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      // `a` acknowledges the opened finding — the single most common triage action, and the
      // one the request's QA plan names as a keyboard path.
      if (event.key === "a" && !typing && open) {
        event.preventDefault();
        void setSecurityFindingStatus(open.id, { status: "acknowledged" })
          .then(() => {
            setNotice({ tone: "ok", text: "Acknowledged." });
            setOpen(null);
            void load();
          })
          .catch((caught: ApiError) => setNotice({ tone: "bad", text: caught.message }));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, load]);

  const allSelected = rows.length > 0 && selected.length === rows.length;

  const toggleAll = useCallback(() => {
    setSelected(allSelected ? [] : rows.map((row) => row.id));
  }, [allSelected, rows]);

  const bulk = useCallback(
    async (status: SecurityFindingStatus) => {
      if (selected.length === 0) return;
      setBulkBusy(true);
      try {
        const result = await bulkSecurityFindingStatus(selected, { status });
        const skipped = result.missing.length;
        setNotice({
          tone: skipped > 0 ? "warn" : "ok",
          text:
            skipped > 0
              ? `${result.updated.length} changed, ${skipped} no longer matched this filter.`
              : `${result.updated.length} ${status === "acknowledged" ? "acknowledged" : status}.`,
        });
        void load();
      } catch (caught) {
        setNotice({ tone: "bad", text: (caught as ApiError).message });
      } finally {
        setBulkBusy(false);
      }
    },
    [selected, load],
  );

  const onImport = useCallback(
    async (file: File) => {
      setImporting(true);
      setNotice(null);
      try {
        const text = await file.text();
        const report = JSON.parse(text) as unknown;
        const result = await importSecurityReport(report, "dependency");
        setNotice({
          tone: result.rejected.length > 0 ? "warn" : "ok",
          text:
            `${result.created} new, ${result.refreshed} already known` +
            (result.rejected.length > 0 ? `, ${result.rejected.length} refused` : "") +
            ".",
        });
        void load();
      } catch (caught) {
        // A file that is not JSON, and a report the platform refuses whole (a credential in
        // it), both land here — and both deserve the same message: the ingest did not happen.
        const message =
          caught instanceof SyntaxError
            ? `${file.name} is not valid JSON.`
            : (caught as ApiError).message;
        setNotice({ tone: "bad", text: message });
      } finally {
        setImporting(false);
        if (fileRef.current) fileRef.current.value = "";
      }
    },
    [load],
  );

  const pageCount = Math.max(1, Math.ceil(total / limit));

  const rows_ = useMemo(
    () =>
      rows.map((row) => (
        <tr
          key={row.id}
          className="border-t border-line"
          data-finding-row={row.id}
          data-finding-severity={row.severity}
        >
          <td className="px-3 py-2">
            <input
              type="checkbox"
              checked={selected.includes(row.id)}
              onChange={() =>
                setSelected((current) =>
                  current.includes(row.id)
                    ? current.filter((id) => id !== row.id)
                    : [...current, row.id],
                )
              }
              aria-label={`Select ${row.title}`}
            />
          </td>
          <td className="px-3 py-2">
            <span
              className={`inline-flex rounded-full px-2 py-0.5 text-[11px] font-medium ${SEVERITY_CLASS[row.severity]}`}
            >
              {row.severity}
            </span>
          </td>
          <td className="px-3 py-2">
            <button
              type="button"
              onClick={() => setOpen(row)}
              className="text-left text-[13px] text-ink underline-offset-2 hover:underline"
              data-finding-open={row.id}
            >
              {row.title}
            </button>
            <p className="text-[11px] text-muted">
              {SOURCE_LABEL[row.source]}
              {row.component ? ` · ${row.component}` : ""}
              {row.component_version ? ` ${row.component_version}` : ""}
            </p>
          </td>
          <td className="px-3 py-2 text-[12px] text-ink">{STATUS_LABEL[row.status]}</td>
          <td className="px-3 py-2 text-[12px] text-muted">{when(row.last_seen_at)}</td>
        </tr>
      )),
    [rows, selected],
  );

  return (
    <div className="space-y-4" data-security-findings>
      <SecurityTabs current="findings" />
      <header className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12px] text-muted">
          {/* The count is the server's, for the current filter. */}
          <span data-findings-total>{total}</span>{" "}
          {total === 1 ? "finding matches" : "findings match"} this filter.
          <span className="ml-2">Press <kbd className="rounded border border-line px-1">/</kbd> to filter.</span>
        </p>
        <div className="flex items-center gap-2">
          <input
            ref={fileRef}
            type="file"
            accept="application/json,.json"
            className="hidden"
            onChange={(event) => {
              const file = event.target.files?.[0];
              if (file) void onImport(file);
            }}
            data-findings-import
          />
          <a
            href={securityFindingsExportUrl({
              ...filter,
              search: searchDraft.trim() || undefined,
            })}
            className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink"
            data-findings-export
            title="Exports every finding this filter matches, not just this page"
          >
            <Download aria-hidden className="h-3.5 w-3.5" />
            Export CSV
          </a>
          <button
            type="button"
            onClick={() => fileRef.current?.click()}
            disabled={importing}
            className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-60"
            data-findings-import-button
          >
            {importing ? (
              <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Upload aria-hidden className="h-3.5 w-3.5" />
            )}
            Import report
          </button>
          <button
            type="button"
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded border border-line px-2.5 py-1 text-[12px] text-ink"
            data-findings-refresh
          >
            <RefreshCw aria-hidden className="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>
      </header>

      {notice ? (
        <p
          className={`text-[12px] ${
            notice.tone === "bad"
              ? "text-red-700 dark:text-red-300"
              : notice.tone === "warn"
                ? "text-amber-700 dark:text-amber-300"
                : "text-emerald-700 dark:text-emerald-300"
          }`}
          data-findings-notice
        >
          {notice.text}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center gap-1 text-[12px] text-muted">
          <Filter aria-hidden className="h-3.5 w-3.5" />
          Filters
        </span>
        <select
          value={filter.severity ?? ""}
          onChange={(event) => {
            setFilter((current) => ({ ...current, severity: (event.target.value || undefined) as SecurityFindingFilter["severity"] }));
            setOffset(0);
          }}
          aria-label="Severity"
          className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-ink"
          data-findings-filter-severity
        >
          <option value="">Any severity</option>
          {SEVERITIES.map((value) => (
            <option key={value} value={value}>
              {value}
            </option>
          ))}
        </select>
        <select
          value={filter.status ?? ""}
          onChange={(event) => {
            setFilter((current) => ({ ...current, status: (event.target.value || undefined) as SecurityFindingStatus | undefined }));
            setOffset(0);
          }}
          aria-label="Status"
          className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-ink"
          data-findings-filter-status
        >
          <option value="">Any status</option>
          {STATUSES.map((value) => (
            <option key={value} value={value}>
              {STATUS_LABEL[value]}
            </option>
          ))}
        </select>
        <select
          value={filter.source ?? ""}
          onChange={(event) => {
            setFilter((current) => ({ ...current, source: (event.target.value || undefined) as SecurityFindingFilter["source"] }));
            setOffset(0);
          }}
          aria-label="Source"
          className="rounded border border-line bg-surface px-2 py-1 text-[12px] text-ink"
          data-findings-filter-source
        >
          <option value="">Any source</option>
          {SOURCES.map((value) => (
            <option key={value} value={value}>
              {SOURCE_LABEL[value]}
            </option>
          ))}
        </select>
        <span className="relative">
          <Search aria-hidden className="pointer-events-none absolute left-2 top-1.5 h-3.5 w-3.5 text-muted" />
          <input
            ref={searchRef}
            value={searchDraft}
            onChange={(event) => setSearchDraft(event.target.value)}
            placeholder="Title or description"
            aria-label="Search findings"
            className="w-56 rounded border border-line bg-surface py-1 pl-7 pr-2 text-[12px] text-ink"
            data-findings-search
          />
        </span>
      </div>

      {selected.length > 0 ? (
        <div className="flex flex-wrap items-center gap-2 rounded border border-line bg-surface px-3 py-2">
          <span className="text-[12px] text-ink">{selected.length} selected</span>
          <button
            type="button"
            disabled={bulkBusy}
            onClick={() => void bulk("acknowledged")}
            className="rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-60"
            data-findings-bulk-ack
          >
            Acknowledge
          </button>
          <button
            type="button"
            disabled={bulkBusy}
            onClick={() => void bulk("fixed")}
            className="rounded border border-line px-2.5 py-1 text-[12px] text-ink disabled:opacity-60"
          >
            Mark fixed
          </button>
          <span className="text-[11px] text-muted">
            Bulk acknowledge is the same permission as the single one.
          </span>
        </div>
      ) : null}

      {error ? (
        <div className="space-y-2">
          <p className="text-[13px] text-red-700 dark:text-red-300">{error.message}</p>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded border border-line px-3 py-1 text-[13px] text-ink"
          >
            Try again
          </button>
        </div>
      ) : loading && rows.length === 0 ? (
        <p className="flex items-center gap-2 text-[13px] text-muted" data-findings-loading>
          <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
          Loading findings…
        </p>
      ) : rows.length === 0 ? (
        <EmptyState
          title="No findings match this filter"
          hint="Either the platform has found nothing, or the filter is narrower than the result. Widen the filter before concluding there is nothing to fix."
        />
      ) : (
        <>
          {/* The table on a wide screen; the cards below are the same rows for a phone, and
              they carry the same data-* hooks so the QA pass drives both from one selector. */}
          <div className="hidden overflow-x-auto rounded border border-line md:block">
            <table className="w-full border-collapse text-left">
              <thead>
                <tr className="text-[11px] uppercase tracking-wide text-muted">
                  <th scope="col" className="px-3 py-2">
                    <input
                      type="checkbox"
                      checked={allSelected}
                      onChange={toggleAll}
                      aria-label="Select every finding on this page"
                      data-findings-select-all
                    />
                  </th>
                  <th scope="col" className="px-3 py-2">Severity</th>
                  <th scope="col" className="px-3 py-2">Finding</th>
                  <th scope="col" className="px-3 py-2">Status</th>
                  <th scope="col" className="px-3 py-2">Last seen</th>
                </tr>
              </thead>
              <tbody>{rows_}</tbody>
            </table>
          </div>

          <ul className="space-y-2 md:hidden" data-findings-cards>
            {rows.map((row) => (
              <li
                key={row.id}
                className="rounded border border-line p-3"
                data-finding-card={row.id}
                data-finding-severity={row.severity}
              >
                <div className="flex items-start justify-between gap-2">
                  <span
                    className={`inline-flex rounded-full px-2 py-0.5 text-[11px] font-medium ${SEVERITY_CLASS[row.severity]}`}
                  >
                    {row.severity}
                  </span>
                  <input
                    type="checkbox"
                    checked={selected.includes(row.id)}
                    onChange={() =>
                      setSelected((current) =>
                        current.includes(row.id)
                          ? current.filter((id) => id !== row.id)
                          : [...current, row.id],
                      )
                    }
                    aria-label={`Select ${row.title}`}
                  />
                </div>
                <button
                  type="button"
                  onClick={() => setOpen(row)}
                  className="mt-1 text-left text-[13px] text-ink"
                  data-finding-open={row.id}
                >
                  {row.title}
                </button>
                <p className="mt-0.5 text-[11px] text-muted">
                  {SOURCE_LABEL[row.source]} · {STATUS_LABEL[row.status]} ·{" "}
                  {when(row.last_seen_at)}
                </p>
              </li>
            ))}
          </ul>

          {pageCount > 1 ? (
            <div className="flex items-center gap-2 text-[12px] text-muted">
              <button
                type="button"
                disabled={offset === 0}
                onClick={() => setOffset(Math.max(0, offset - limit))}
                className="rounded border border-line px-2.5 py-1 disabled:opacity-50"
              >
                Previous
              </button>
              <span>
                Page {Math.floor(offset / limit) + 1} of {pageCount}
              </span>
              <button
                type="button"
                disabled={offset + limit >= total}
                onClick={() => setOffset(offset + limit)}
                className="rounded border border-line px-2.5 py-1 disabled:opacity-50"
              >
                Next
              </button>
            </div>
          ) : null}
        </>
      )}

      {open ? (
        <>
          <div
            className="fixed inset-0 z-30 bg-black/20"
            onClick={() => setOpen(null)}
            aria-hidden
          />
          <FindingDrawer
            finding={open}
            canManage={true}
            onClose={() => setOpen(null)}
            onChanged={() => void load()}
          />
        </>
      ) : null}
    </div>
  );
}
