"use client";

/**
 * The document index (REQ-055, slice 4b) — `/hr/documents`.
 *
 * This screen exists for one question: **whose paperwork expires next, and what do we already
 * hold?** That is a cross-employee question, which is why it is a route of its own behind
 * `hr.documents.read` rather than a tab on the employee detail — a person looking at one contract
 * should not need the organization's whole filing cabinet to be open in front of them.
 *
 * Five decisions the screen had to make, and what it does instead:
 *
 * 1. **The header counts the FILTERED rows, not the organization's.** The server sends `totals`
 *    over the same query the table shows; a header counting everything above a filtered table is a
 *    screen lying in the one place somebody is looking for a number.
 * 2. **The status badge is text from the server, never a colour decided here.** `expired`,
 *    `expiring` and `valid` are three rows with three colours, and a screen that derives them
 *    client-side eventually shows an expired passport in green.
 * 3. **"Expiring soon" is a toggle, not a separate page.** The window is the module's thirty days
 *    and it lives on the server, because a second definition here is a second number that can
 *    disagree with the sweep's.
 * 4. **Delete says what it deletes.** The route drops the **reference**; the bytes stay in the
 *    media pipeline. A button labelled "delete" on a screen of contracts is the moment somebody
 *    clicks it without reading, so the row action names the reference and the confirmation repeats
 *    it.
 * 5. **The sweep is a button with a receipt.** It is the one write here that touches everybody's
 *    documents, so it reports what it considered and what it announced — including the second run
 *    saying "nothing left", which is the only way an operator can tell a working sweep from a
 *    broken one.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import {
  AlertTriangle,
  BellRing,
  FileText,
  Filter,
  Search,
  Trash2,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ErrorState,
  describeError,
  toScreenError,
  type ScreenErrorValue,
} from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";

import { HrModuleNav } from "@/features/hr/module-nav";

import {
  DOCUMENT_KINDS,
  deleteDocument,
  fetchDocuments,
  sweepDocuments,
  type DocumentFilters,
  type DocumentStatus,
  type HrDocument,
  type SweepResult,
} from "@/lib/hr";

/** The four kinds, as a person reads them. A row whose kind is not here renders its raw text. */
const KIND_LABEL: Record<string, string> = {
  contract: "Contract",
  id: "ID",
  certificate: "Certificate",
  other: "Other",
};

/**
 * The status, as a badge.
 *
 * Expired and expiring are both "act on this", and they are **different words** rather than two
 * shades of the same amber: an expired passport and one that lapses in nine days call for
 * different actions, and a screen that renders them the same colour has thrown away the
 * distinction it was given for free.
 */
function DocumentStatusBadge({ status }: { status: DocumentStatus }) {
  return <StatusBadge status={status} />;
}

/** How long until expiry, as a person reads it — `in 9 days`, `12 days ago`, `—`. */
function expiryText(document: HrDocument): string {
  const days = document.days_until_expiry;
  if (days === null) {
    return "—";
  }
  if (days < 0) {
    const past = Math.abs(days);
    return `${past} day${past === 1 ? "" : "s"} ago`;
  }
  if (days === 0) {
    return "today";
  }
  return `in ${days} day${days === 1 ? "" : "s"}`;
}

export function DocumentsView() {
  const router = useRouter();
  const search = useSearchParams();

  const [documents, setDocuments] = useState<HrDocument[] | null>(null);
  const [totals, setTotals] = useState<{ total: number; expired: number; expiring: number; permanent: number } | null>(null);
  const [error, setError] = useState<ScreenErrorValue | null>(null);
  const [busy, setBusy] = useState(false);

  // Filters live in the URL. A filter that is not in the address bar is a filter the operator
  // cannot send to a colleague, and a refresh that throws it away is a screen that lies about
  // what it is showing.
  const [searchText, setSearchText] = useState(search.get("q") ?? "");
  const [kind, setKind] = useState(search.get("kind") ?? "");
  const [onlyExpiring, setOnlyExpiring] = useState(search.get("expiring") === "1");

  const [confirming, setConfirming] = useState<HrDocument | null>(null);
  const [sweep, setSweep] = useState<SweepResult | null>(null);
  const [sweepError, setSweepError] = useState<string | null>(null);
  const [sweeping, setSweeping] = useState(false);

  const filters = useMemo<DocumentFilters>(
    () => ({
      search: search.get("q") ?? undefined,
      kind: search.get("kind") || undefined,
      expiring: search.get("expiring") === "1" ? true : undefined,
    }),
    [search],
  );

  const load = useCallback(async () => {
    setError(null);
    try {
      const page = await fetchDocuments(filters);
      setDocuments(page.items);
      setTotals(page.totals);
    } catch (cause) {
      setError(toScreenError(cause, "The document list could not be read."));
    }
  }, [filters]);

  useEffect(() => {
    void load();
  }, [load]);

  // The typed text is debounced into the URL, so every keystroke does not become a request and a
  // fast typist still ends up with one search rather than nine.
  useEffect(() => {
    const current = search.get("q") ?? "";
    if (current === searchText) {
      return;
    }
    const timer = setTimeout(() => {
      const next = new URLSearchParams(search.toString());
      if (searchText) {
        next.set("q", searchText);
      } else {
        next.delete("q");
      }
      router.replace(`/hr/documents${next.toString() ? `?${next}` : ""}`, { scroll: false });
    }, 300);
    return () => clearTimeout(timer);
  }, [searchText, search, router]);

  const setFilter = (key: string, value: string | null) => {
    const next = new URLSearchParams(search.toString());
    if (value) {
      next.set(key, value);
    } else {
      next.delete(key);
    }
    router.replace(`/hr/documents${next.toString() ? `?${next}` : ""}`, { scroll: false });
  };

  const filtersActive = Boolean(search.get("q")) || Boolean(search.get("kind")) || search.get("expiring") === "1";

  const clearFilters = () => {
    setSearchText("");
    router.replace("/hr/documents", { scroll: false });
  };

  const runSweep = async () => {
    setSweeping(true);
    setSweepError(null);
    try {
      const result = await sweepDocuments();
      setSweep(result);
      // The sweep flips rows into `acknowledged`, which is one of the three filters' inputs, so the
      // table has to be re-read. A sweep that announces and leaves a stale table on screen is
      // exactly the "nothing happened" the receipt was supposed to rule out.
      await load();
    } catch (cause) {
      setSweepError(describeError(toScreenError(cause, "The expiry sweep was refused.")).message);
    } finally {
      setSweeping(false);
    }
  };

  const remove = async (document: HrDocument) => {
    setBusy(true);
    setSweepError(null);
    try {
      await deleteDocument(document.id);
      setConfirming(null);
      await load();
    } catch (cause) {
      setSweepError(describeError(toScreenError(cause, "The expiry sweep was refused.")).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-4" data-qa-hr-documents>
      <HrModuleNav />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-[17px] font-semibold">Documents</h1>
          <p className="text-[12.5px] text-muted">
            Every employee&rsquo;s contracts, IDs and certificates in one place — the screen that
            answers &ldquo;whose expires next&rdquo;.
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={runSweep}
            disabled={sweeping}
            data-qa-hr-documents-sweep
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-50"
          >
            <BellRing className="h-3.5 w-3.5" aria-hidden />
            {sweeping ? "Sweeping…" : "Run expiry sweep"}
          </button>
        </div>
      </header>

      {/* The counts, over the filtered rows. */}
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4" data-qa-hr-documents-totals>
        {[
          { label: "In this view", value: totals?.total, tone: "text-foreground" },
          { label: "Expired", value: totals?.expired, tone: "text-red-600" },
          { label: "Expiring in 30 days", value: totals?.expiring, tone: "text-amber-600" },
          { label: "No expiry", value: totals?.permanent, tone: "text-muted" },
        ].map((tile) => (
          <div key={tile.label} className="rounded-lg border border-line px-3 py-2">
            <p className="text-[11px] uppercase tracking-wide text-muted">{tile.label}</p>
            <p className={`text-[19px] font-semibold tabular-nums ${tile.tone}`} data-qa-hr-documents-total={tile.label}>
              {tile.value ?? "—"}
            </p>
          </div>
        ))}
      </div>

      {/* The filters. */}
      <div className="flex flex-wrap items-center gap-2">
        <label className="relative flex min-w-[13rem] flex-1 items-center">
          <Search className="pointer-events-none absolute left-2.5 h-3.5 w-3.5 text-muted" aria-hidden />
          <span className="sr-only">Search documents</span>
          <input
            value={searchText}
            onChange={(event) => setSearchText(event.target.value)}
            placeholder="Search title or employee…"
            data-qa-hr-documents-search
            className="h-8 w-full rounded-md border border-line bg-background pl-8 pr-2 text-[12.5px] outline-none focus:border-foreground/40"
          />
        </label>

        <label className="inline-flex items-center gap-1.5 text-[12.5px] text-muted">
          <Filter className="h-3.5 w-3.5" aria-hidden />
          <span className="sr-only">Filter by kind</span>
          <select
            value={kind}
            onChange={(event) => setFilter("kind", event.target.value || null)}
            data-qa-hr-documents-kind
            className="h-8 rounded-md border border-line bg-background px-2 text-[12.5px]"
          >
            <option value="">All kinds</option>
            {DOCUMENT_KINDS.map((value) => (
              <option key={value} value={value}>
                {KIND_LABEL[value]}
              </option>
            ))}
          </select>
        </label>

        <button
          type="button"
          onClick={() => setFilter("expiring", onlyExpiring ? null : "1")}
          aria-pressed={onlyExpiring}
          data-qa-hr-documents-expiring
          className={`inline-flex h-8 items-center gap-1.5 rounded-md border px-2.5 text-[12.5px] ${
            onlyExpiring ? "border-foreground/40 bg-quiet-soft" : "border-line"
          }`}
        >
          <AlertTriangle className="h-3.5 w-3.5" aria-hidden />
          Expiring soon
        </button>

        {filtersActive ? (
          <button
            type="button"
            onClick={clearFilters}
            data-qa-hr-documents-clear
            className="inline-flex h-8 items-center gap-1 rounded-md px-2 text-[12.5px] text-muted hover:text-foreground"
          >
            <X className="h-3.5 w-3.5" aria-hidden />
            Clear
          </button>
        ) : null}
      </div>

      {sweep ? (
        <div
          role="status"
          data-qa-hr-documents-sweep-result
          className="rounded-lg border border-line bg-quiet-soft/40 px-3 py-2 text-[12.5px]"
        >
          Swept {sweep.considered} document{sweep.considered === 1 ? "" : "s"} inside the window,
          announced {sweep.notified}.
          {sweep.notified === 0 ? (
            <span className="text-muted">
              {" "}
              Nothing left to announce — every document in the window has already been swept, which
              is what a second run is supposed to say.
            </span>
          ) : null}
        </div>
      ) : null}

      {sweepError ? (
        <div role="alert" data-qa-hr-documents-sweep-error className="rounded-lg border border-red-300 bg-red-50/60 px-3 py-2 text-[12.5px] text-red-700">
          {sweepError}
        </div>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : documents === null ? (
        <LoadingTable columns={5} />
      ) : documents.length === 0 ? (
        <div className="rounded-lg border border-line">
          {filtersActive ? (
            <EmptyState
              title="No documents match these filters"
              hint="Nothing here is an error — the filters are just narrower than the filing cabinet. Clear them to see everything."
              action={
                <button
                  type="button"
                  onClick={clearFilters}
                  className="inline-flex h-8 items-center rounded-md border border-line px-3 text-[12.5px] hover:bg-quiet-soft"
                >
                  Clear filters
                </button>
              }
            />
          ) : (
            <EmptyState
              title="No documents yet"
              hint="Attach a contract, ID or certificate from an employee's record and it lands here with its expiry date."
            />
          )}
        </div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-4 py-2.5 font-medium">Employee</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Document</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Kind</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Expires</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Status</th>
                <th scope="col" className="px-4 py-2.5 font-medium"><span className="sr-only">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              {documents.map((document) => (
                <tr key={document.id} className="border-b border-line last:border-b-0" data-qa-hr-document-row={document.id}>
                  <td className="px-4 py-3">
                    <span className="font-medium">{document.employee_name}</span>
                    {document.department_name ? (
                      <span className="block text-[11.5px] text-muted">{document.department_name}</span>
                    ) : null}
                  </td>
                  <td className="px-4 py-3">
                    <span className="inline-flex items-center gap-1.5">
                      <FileText className="h-3.5 w-3.5 shrink-0 text-muted" aria-hidden />
                      {document.title}
                    </span>
                    {document.acknowledged ? (
                      <span className="block text-[11.5px] text-muted">Expiry announced</span>
                    ) : null}
                  </td>
                  <td className="px-4 py-3 text-muted">{KIND_LABEL[document.kind] ?? document.kind}</td>
                  <td className="px-4 py-3">
                    {document.expires_on ? (
                      <>
                        <span className="tabular-nums">{document.expires_on}</span>
                        <span className="block text-[11.5px] text-muted">{expiryText(document)}</span>
                      </>
                    ) : (
                      <span className="text-muted">Never</span>
                    )}
                  </td>
                  <td className="px-4 py-3">
                    <DocumentStatusBadge status={document.status} />
                  </td>
                  <td className="px-4 py-3 text-right">
                    {confirming?.id === document.id ? (
                      <span className="inline-flex items-center gap-1.5">
                        <span className="text-[11.5px] text-muted">Drop the reference?</span>
                        <button
                          type="button"
                          onClick={() => void remove(document)}
                          disabled={busy}
                          data-qa-hr-documents-confirm
                          className="inline-flex h-7 items-center rounded-md bg-red-600 px-2 text-[11.5px] text-white hover:bg-red-700 disabled:opacity-50"
                        >
                          Yes
                        </button>
                        <button
                          type="button"
                          onClick={() => setConfirming(null)}
                          className="inline-flex h-7 items-center rounded-md border border-line px-2 text-[11.5px]"
                        >
                          Cancel
                        </button>
                      </span>
                    ) : (
                      <button
                        type="button"
                        onClick={() => setConfirming(document)}
                        data-qa-hr-documents-remove={document.id}
                        aria-label={`Remove the reference to ${document.title}`}
                        className="inline-flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-quiet-soft hover:text-red-600"
                      >
                        <Trash2 className="h-3.5 w-3.5" aria-hidden />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
