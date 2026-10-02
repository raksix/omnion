"use client";

/**
 * The quote list (REQ-052, slice 2): `/sales/quotes`.
 *
 * A list, and the half of a quote's lifecycle that belongs on it: open a draft, send one, cancel
 * one, duplicate one into a new draft. Four decisions worth naming, because each of them is a way
 * this screen could have lied to a seller:
 *
 * * **The totals shown are the server's.** Every amount in this file is the decimal text the API
 *   sent; the client never adds anything up. A list that computed its own column would disagree
 *   with the document the moment a line had an awkward fraction, and the seller would be the one
 *   explaining it to a customer.
 * * **The validity hint is amber, then red, and the red is not decoration.** `quoteValidityTone`
 *   compares at UTC midnight because the column is a `date`: doing it in the browser's zone makes
 *   a quote read as expired a day early for half the world, and a red badge on a live quote is the
 *   kind of thing a customer notices.
 * * **`Send` and `Cancel` are on the row, and both say what they do.** Sending freezes the
 *   document, so it asks first; cancelling is permanent, so it asks for a reason and shows it. A
 *   button that destroys something without saying so is the failure mode this module keeps
 *   refusing everywhere else.
 * * **A sent quote's row still opens.** It opens into a read-only detail with a "duplicate into
 *   a new draft" action, because the alternative — a row that stops being clickable — is how a
 *   seller concludes the quote vanished.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";

import {
  Ban,
  Copy,
  Loader2,
  Plus,
  RotateCcw,
  Send,
  FileText,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { formatMoney } from "@/lib/sales";
import {
  cancelSalesQuote,
  duplicateSalesQuote,
  fetchSalesQuoteVocabulary,
  fetchSalesQuotes,
  quoteStatusTone,
  quoteValidityTone,
  sendSalesQuote,
  type SalesQuote,
  type SalesQuoteStatus,
  type SalesQuoteVocabulary,
} from "@/lib/sales-quotes";

import { SalesShortcutSheet, SalesToolbar, salesRowCursor, useSales, useSalesKeyboard } from "./sales-parts";

/** The columns, in the order the table draws them. */
const COLUMNS = [
  "Number",
  "Customer",
  "Title",
  "Amount",
  "Status",
  "Valid until",
  "Version",
  "Updated",
  "",
];

/** The status tabs, `All` first and then the pipeline in the order a quote travels it. */
const TABS: { value: string; label: string }[] = [
  { value: "", label: "All" },
  { value: "draft", label: "Draft" },
  { value: "pending_approval", label: "Awaiting approval" },
  { value: "sent", label: "Sent" },
  { value: "accepted", label: "Accepted" },
  { value: "declined", label: "Declined" },
  { value: "expired", label: "Expired" },
  { value: "cancelled", label: "Cancelled" },
];

/** `/sales/quotes`: the list, its filters, and the row actions. */
export function QuotesView() {
  const router = useRouter();
  const params = useSearchParams();
  const { organizationId } = useSales();

  const [page, setPage] = useState<{ items: SalesQuote[]; total_estimate: number } | null>(null);
  const [vocabulary, setVocabulary] = useState<SalesQuoteVocabulary | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [selected, setSelected] = useState(0);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<{ quote: SalesQuote; action: "send" | "cancel" } | null>(
    null,
  );
  const [cancelReason, setCancelReason] = useState("");
  const searchRef = useRef<HTMLInputElement | null>(null);

  const status = params.get("status") ?? "";
  const search = params.get("search") ?? "";
  const expiring = params.get("expiring") ?? "";
  const filtered = status !== "" || search !== "" || expiring !== "";

  const query = useCallback(
    () => ({
      search: search || undefined,
      status: status || undefined,
      expiring_in_days: expiring === "" ? undefined : Number(expiring),
      organization_id: organizationId ?? undefined,
      limit: 100,
    }),
    [search, status, expiring, organizationId],
  );

  useEffect(() => {
    setError(null);
    fetchSalesQuotes(query())
      .then((loaded) => {
        setPage(loaded);
        setSelected((current) => (current < loaded.items.length ? current : 0));
      })
      .catch((problem) => setError(toScreenError(problem, "The quotes could not be loaded.")));
  }, [query, reloadToken]);

  useEffect(() => {
    fetchSalesQuoteVocabulary(organizationId)
      .then(setVocabulary)
      .catch(() => setVocabulary(null));
  }, [organizationId]);

  const items = page?.items ?? [];

  const run = useCallback(
    async (quote: SalesQuote, action: "send" | "cancel" | "duplicate") => {
      setBusyId(quote.id);
      setActionError(null);
      setNotice(null);
      try {
        if (action === "send") {
          await sendSalesQuote(quote.id, organizationId);
          setNotice(`Sent ${quote.number}. The customer can accept it from the link you issue next.`);
        } else if (action === "duplicate") {
          const copy = await duplicateSalesQuote(quote.id, organizationId);
          setNotice(`Copied ${quote.number} into ${copy.quote.number}.`);
          router.push(`/sales/quotes/${copy.quote.id}`);
          return;
        } else {
          await cancelSalesQuote(quote.id, cancelReason.trim(), organizationId);
          setNotice(`Cancelled ${quote.number}.`);
        }
        setReloadToken((token) => token + 1);
      } catch (problem) {
        setActionError(
          problem instanceof Error ? problem.message : "That action could not be completed.",
        );
      } finally {
        setBusyId(null);
        setConfirming(null);
        setCancelReason("");
      }
    },
    [cancelReason, organizationId, router],
  );

  const { shortcutsOpen, setShortcutsOpen } = useSalesKeyboard(
    {
      count: items.length,
      selected,
      onSelect: setSelected,
      onOpen: (index) => {
        const quote = items[index];
        if (quote) router.push(`/sales/quotes/${quote.id}`);
      },
      onEdit: (index) => {
        const quote = items[index];
        if (quote) router.push(`/sales/quotes/${quote.id}`);
      },
      onNew: () => router.push("/sales/quotes/new"),
    },
    searchRef,
  );

  // The tabs' counts, so a seller can see where the work is without clicking each one. Counted from
  // the page rather than asked for separately: a list of 100 rows cannot count what it is not
  // showing, and a tab that says "0" because it only saw the first page is worse than no count.
  const tabCounts = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const quote of items) {
      counts[quote.status] = (counts[quote.status] ?? 0) + 1;
    }
    return counts;
  }, [items]);

  return (
    <div className="space-y-3">
      <header className="flex flex-wrap items-end justify-between gap-2">
        <div>
          <h1 className="text-[15px] font-semibold">Quotes</h1>
          <p className="text-[12.5px] text-muted">
            A sent quote is frozen: the lines the customer read are kept as a version, and changing
            them means duplicating it into a new draft.
          </p>
        </div>
        <button
          type="button"
          onClick={() => router.push("/sales/quotes/new")}
          data-qa-sales-new-quote
          className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          New quote
        </button>
      </header>

      <SalesToolbar
        search={search}
        onSearchChange={(value) => {
          const next = new URLSearchParams(params.toString());
          if (value) next.set("search", value);
          else next.delete("search");
          const text = next.toString();
          router.replace(text ? `/sales/quotes?${text}` : "/sales/quotes", { scroll: false });
        }}
        searchRef={searchRef}
        count={items.length}
        filtered={filtered}
      >
        <div className="flex flex-wrap items-center gap-1" role="tablist" aria-label="Quote status">
          {TABS.map((tab) => {
            const active = status === tab.value;
            const count = tab.value === "" ? items.length : (tabCounts[tab.value] ?? 0);
            return (
              <button
                key={tab.value || "all"}
                type="button"
                role="tab"
                aria-selected={active}
                onClick={() => {
                  const next = new URLSearchParams(params.toString());
                  if (tab.value) next.set("status", tab.value);
                  else next.delete("status");
                  const text = next.toString();
                  router.replace(text ? `/sales/quotes?${text}` : "/sales/quotes", { scroll: false });
                }}
                data-qa-sales-tab={tab.value || "all"}
                className={`rounded-md border px-2 py-1 text-[12px] ${
                  active ? "border-ink bg-ink text-panel" : "border-line text-muted"
                }`}
              >
                {tab.label}
                <span className="ml-1 opacity-70">{count}</span>
              </button>
            );
          })}
        </div>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          Expiring in
          <select
            value={expiring}
            onChange={(event) => {
              const next = new URLSearchParams(params.toString());
              if (event.target.value) next.set("expiring", event.target.value);
              else next.delete("expiring");
              const text = next.toString();
              router.replace(text ? `/sales/quotes?${text}` : "/sales/quotes", { scroll: false });
            }}
            data-qa-sales-expiring
            className="rounded-md border border-line bg-canvas px-1.5 py-1 text-[12px]"
          >
            <option value="">any time</option>
            <option value="0">already lapsed</option>
            <option value="7">7 days</option>
            <option value="14">14 days</option>
            <option value="30">30 days</option>
          </select>
        </label>
      </SalesToolbar>

      {notice ? (
        <p data-qa-sales-notice className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {actionError ? (
        <p data-qa-sales-action-error className="rounded-md border border-negative/40 px-3 py-2 text-[12.5px] text-negative">
          {actionError}
        </p>
      ) : null}

      {confirming ? (
        <div
          data-qa-sales-confirm
          className="rounded-lg border border-line bg-panel p-3 text-[12.5px] shadow-sm"
        >
          {confirming.action === "send" ? (
            <p>
              Send <strong>{confirming.quote.number}</strong> to the customer? The lines freeze and
              the document can only be changed by duplicating it.
            </p>
          ) : (
            <div className="space-y-2">
              <p>
                Cancel <strong>{confirming.quote.number}</strong>? The customer cannot accept it
                afterwards, and this cannot be undone.
              </p>
              <label className="block">
                <span className="mb-1 block text-muted">Why (shown in the timeline)</span>
                <input
                  value={cancelReason}
                  onChange={(event) => setCancelReason(event.target.value)}
                  data-qa-sales-cancel-reason
                  placeholder="Lost to a competitor, budget frozen, …"
                  className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
                />
              </label>
            </div>
          )}
          <div className="mt-2 flex items-center gap-2">
            <button
              type="button"
              onClick={() => run(confirming.quote, confirming.action)}
              disabled={busyId !== null}
              data-qa-sales-confirm-yes
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busyId ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              {confirming.action === "send" ? "Send it" : "Cancel the quote"}
            </button>
            <button
              type="button"
              onClick={() => {
                setConfirming(null);
                setCancelReason("");
              }}
              data-qa-sales-confirm-no
              className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              Keep working on it
            </button>
          </div>
        </div>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />
      ) : !page ? (
        <LoadingTable columns={COLUMNS.length} />
      ) : items.length === 0 ? (
        <EmptyState
          title={filtered ? "Nothing matches that filter" : "No quotes yet"}
          hint={
            filtered
              ? "There are quotes, just not the ones this filter names."
              : "A quote is a document a customer can read and accept. Start one, or open a deal in the CRM to fill a quote from it."
          }
          action={
            filtered ? (
              <button
                type="button"
                onClick={() => router.replace("/sales/quotes")}
                data-qa-sales-empty-clear
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
              >
                <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                Clear the filters
              </button>
            ) : (
              <button
                type="button"
                onClick={() => router.push("/sales/quotes/new")}
                data-qa-sales-empty-new
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel"
              >
                <Plus className="h-3.5 w-3.5" aria-hidden />
                Write your first quote
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-lg border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                {COLUMNS.map((column) => (
                  <th key={column} scope="col" className="px-3 py-2 font-medium">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {items.map((quote, index) => {
                const cursor = salesRowCursor(index === selected);
                const validity = quoteValidityTone(quote.valid_until);
                const canSend = quote.status === "draft" || quote.status === "approved";
                const canCancel =
                  quote.status !== "accepted" &&
                  quote.status !== "declined" &&
                  quote.status !== "cancelled" &&
                  quote.status !== "expired";
                return (
                  <tr
                    key={quote.id}
                    data-qa-sales-quote-row={quote.number}
                    className={`border-b border-line last:border-b-0 ${cursor.className}`}
                    onClick={() => router.push(`/sales/quotes/${quote.id}`)}
                  >
                    <td className="px-3 py-2 font-medium">
                      <span className="inline-flex items-center gap-1.5">
                        {quote.version === 0 ? (
                          <FileText className="h-3 w-3 text-muted" aria-hidden />
                        ) : null}
                        {quote.number}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      {quote.customer.name || <span className="text-muted">—</span>}
                    </td>
                    <td className="max-w-[16rem] truncate px-3 py-2 text-muted">
                      {quote.title || <span className="text-muted">Untitled</span>}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {formatMoney(quote.totals.grand_total, quote.currency)}
                    </td>
                    <td className="px-3 py-2">
                      <StatusBadge status={quote.status} label={labelFor(vocabulary, quote.status)} />
                    </td>
                    <td className="px-3 py-2">
                      <span
                        data-qa-sales-validity={validity ?? "ok"}
                        className={
                          validity === "expired"
                            ? "text-negative"
                            : validity === "soon"
                              ? "text-warn"
                              : "text-muted"
                        }
                      >
                        {quote.valid_until}
                        {validity === "expired" ? " · lapsed" : null}
                        {validity === "soon" ? " · this week" : null}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {quote.version === 0 ? "—" : `v${quote.version}`}
                    </td>
                    <td className="px-3 py-2 text-muted">{quote.updated_at.slice(0, 10)}</td>
                    <td className="px-3 py-2">
                      <span className="flex items-center justify-end gap-1">
                        {canSend ? (
                          <button
                            type="button"
                            title={`Send ${quote.number}`}
                            aria-label={`Send ${quote.number}`}
                            data-qa-sales-row-send
                            onClick={(event) => {
                              event.stopPropagation();
                              setConfirming({ quote, action: "send" });
                            }}
                            className="rounded-md border border-line p-1.5 hover:bg-quiet-soft"
                          >
                            <Send className="h-3.5 w-3.5" aria-hidden />
                          </button>
                        ) : null}
                        <button
                          type="button"
                          title={`Duplicate ${quote.number}`}
                          aria-label={`Duplicate ${quote.number}`}
                          data-qa-sales-row-duplicate
                          onClick={(event) => {
                            event.stopPropagation();
                            void run(quote, "duplicate");
                          }}
                          disabled={busyId === quote.id}
                          className="rounded-md border border-line p-1.5 hover:bg-quiet-soft disabled:opacity-50"
                        >
                          <Copy className="h-3.5 w-3.5" aria-hidden />
                        </button>
                        {canCancel ? (
                          <button
                            type="button"
                            title={`Cancel ${quote.number}`}
                            aria-label={`Cancel ${quote.number}`}
                            data-qa-sales-row-cancel
                            onClick={(event) => {
                              event.stopPropagation();
                              setConfirming({ quote, action: "cancel" });
                            }}
                            className="rounded-md border border-line p-1.5 hover:bg-quiet-soft"
                          >
                            <Ban className="h-3.5 w-3.5" aria-hidden />
                          </button>
                        ) : null}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <p className="text-[12px] text-muted">
        {page ? `${items.length} of ${page.total_estimate} quotes` : ""}
        {vocabulary
          ? ` · a line over ${vocabulary.discount_approval_threshold}% needs a manager before it can be sent`
          : ""}
      </p>

      <SalesShortcutSheet onClose={() => setShortcutsOpen(false)} />
    </div>
  );
}

/** The status badge. The label comes from the API's vocabulary so the tab and the row agree. */
function StatusBadge({ status, label }: { status: SalesQuoteStatus; label: string }) {
  return (
    <span
      data-qa-sales-status={status}
      className={`inline-block rounded-md border px-1.5 py-0.5 text-[11.5px] ${quoteStatusTone(status)}`}
    >
      {label}
    </span>
  );
}

/** The vocabulary's label for a status, or a readable fallback while it loads. */
function labelFor(vocabulary: SalesQuoteVocabulary | null, status: SalesQuoteStatus): string {
  return vocabulary?.statuses.find((entry) => entry.value === status)?.label ?? status;
}
