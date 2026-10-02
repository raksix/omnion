"use client";

/**
 * The quote detail (REQ-052, slice 2): `/sales/quotes/{id}`.
 *
 * The screen a seller opens after the customer answers, and it is where the module's two hard rules
 * become visible rather than merely enforced:
 *
 * * **A sent quote's lines are read-only, and the screen says why** — with the way out next to the
 *   sentence. "Duplicate into a new draft" is offered as a button rather than mentioned in a
 *   tooltip, because a 409 that says "duplicate it" and a screen with no duplicate button is a
 *   seller who cannot act on the message.
 * * **The public link is issued, not shown.** The token comes back exactly once, from the API, and
 *   the screen copies it. There is no "reveal the link" for a link issued earlier, because the
 *   server stores the hash and cannot show it again — the honest affordance is *re-issue*, and the
 *   honest consequence of re-issuing is that the old link stops working, which the button says.
 *
 * The version list is the customer's history: each row is what the customer read at that version,
 * with the totals as they stood, and the restore action is deliberately **absent** — restoring a
 * version would have to write into a frozen document, so a version is read here and copied by
 * duplicating. Slice 4's "restore as new draft" would be a new draft, not a mutation.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";

import {
  Ban,
  Copy,
  Download,
  ExternalLink,
  Link2,
  Loader2,
  Pencil,
  RotateCcw,
  Send,
} from "lucide-react";

import { ErrorState, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { formatMoney } from "@/lib/sales";
import { documentNotice, downloadQuotePdf } from "@/lib/sales-documents";
import {
  cancelSalesQuote,
  duplicateSalesQuote,
  fetchSalesQuote,
  issueSalesQuoteLink,
  sendSalesQuote,
  quoteStatusTone,
  quoteValidityTone,
  type SalesQuoteDetail,
  type SalesQuoteVocabulary,
} from "@/lib/sales-quotes";

import { useSales } from "./sales-parts";

/** What `GET /sales/quotes/{id}` returns when the quote is still a working document. */
const EDITABLE = ["draft", "pending_approval", "approved"];

/** `/sales/quotes/{id}`: the document, its versions, and everything that can be done to it. */
export function QuoteDetailView({ quoteId }: { quoteId: string }) {
  const router = useRouter();
  const { organizationId } = useSales();

  const [detail, setDetail] = useState<SalesQuoteDetail | null>(null);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [reloadToken, setReloadToken] = useState(0);
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [link, setLink] = useState<string | null>(null);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [reason, setReason] = useState("");

  const load = useCallback(() => {
    setError(null);
    fetchSalesQuote(quoteId, organizationId)
      .then(setDetail)
      .catch((problem) => setError(toScreenError(problem, "That quote could not be opened.")));
  }, [quoteId, organizationId]);

  useEffect(load, [load, reloadToken]);

  const act = useCallback(
    async (name: string, action: () => Promise<unknown>, success: string) => {
      setBusy(name);
      setActionError(null);
      setNotice(null);
      try {
        await action();
        setNotice(success);
        setReloadToken((token) => token + 1);
      } catch (problem) {
        setActionError(
          problem instanceof Error ? problem.message : "That action could not be completed.",
        );
      } finally {
        setBusy(null);
        setConfirmCancel(false);
        setReason("");
      }
    },
    [],
  );

  /**
   * The PDF download, deliberately **not** routed through `act`.
   *
   * `act` reloads the detail after the action, because every other button here changes the
   * document. A download changes nothing, and reloading would make the screen flicker and throw
   * away the scroll position of somebody who is reading the quote they just printed. It also has
   * its own failure message: a download that fails is a download that did not happen, which is
   * not the same sentence as "that action could not be completed".
   */
  const onDownload = useCallback(async () => {
    setBusy("pdf");
    setActionError(null);
    setNotice(null);
    try {
      const document = await downloadQuotePdf(quoteId);
      setNotice(documentNotice(document) ?? `Downloaded ${document.filename}.`);
    } catch (problem) {
      setActionError(
        problem instanceof Error
          ? problem.message
          : "The PDF could not be prepared, so nothing was saved.",
      );
    } finally {
      setBusy(null);
    }
  }, [quoteId]);

  if (error) {
    return <ErrorState error={error} onRetry={() => setReloadToken((token) => token + 1)} />;
  }
  if (!detail) {
    return <LoadingTable columns={6} />;
  }

  const { quote, lines, versions } = detail;
  const editable = EDITABLE.includes(quote.status);
  const validity = quoteValidityTone(quote.valid_until);
  const absoluteLink = link
    ? typeof window === "undefined"
      ? link
      : `${window.location.origin}${link}`
    : null;

  return (
    <div className="space-y-3">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div>
          <div className="flex items-center gap-2">
            <h1 className="text-[15px] font-semibold">{quote.number}</h1>
            <span
              data-qa-sales-detail-status={quote.status}
              className={`rounded-md border px-1.5 py-0.5 text-[11.5px] ${quoteStatusTone(quote.status)}`}
            >
              {quote.status.replace(/_/g, " ")}
            </span>
            {quote.version > 0 ? (
              <span className="text-[12px] text-muted">version {quote.version}</span>
            ) : null}
          </div>
          <p className="text-[12.5px] text-muted">
            {quote.title || "Untitled"} · {quote.customer.name || "no customer"} ·{" "}
            <span className={validity === "expired" ? "text-negative" : validity === "soon" ? "text-warn" : ""}>
              valid until {quote.valid_until}
              {validity === "expired" ? " (lapsed)" : ""}
            </span>
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {editable ? (
            <button
              type="button"
              onClick={() => router.push(`/sales/quotes/${quote.id}/edit`)}
              data-qa-sales-detail-edit
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <Pencil className="h-3.5 w-3.5" aria-hidden />
              Edit the draft
            </button>
          ) : null}
          {quote.status === "draft" || quote.status === "approved" ? (
            <button
              type="button"
              onClick={() =>
                void act(
                  "send",
                  () => sendSalesQuote(quote.id, organizationId),
                  `${quote.number} sent. Its lines are now frozen.`,
                )
              }
              disabled={busy !== null}
              data-qa-sales-detail-send
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busy === "send" ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
              ) : (
                <Send className="h-3.5 w-3.5" aria-hidden />
              )}
              Send
            </button>
          ) : null}
          <button
            type="button"
            onClick={() => void onDownload()}
            disabled={busy !== null}
            data-qa-sales-detail-pdf
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
          >
            {busy === "pdf" ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
            ) : (
              <Download className="h-3.5 w-3.5" aria-hidden />
            )}
            PDF
          </button>
          <button
            type="button"
            onClick={() =>
              void act(
                "duplicate",
                () => duplicateSalesQuote(quote.id, organizationId),
                `Copied ${quote.number} into a new draft.`,
              )
            }
            disabled={busy !== null}
            data-qa-sales-detail-duplicate
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-60"
          >
            <Copy className="h-3.5 w-3.5" aria-hidden />
            Duplicate
          </button>
          {editable ? (
            <button
              type="button"
              onClick={() => setConfirmCancel(true)}
              data-qa-sales-detail-cancel
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <Ban className="h-3.5 w-3.5" aria-hidden />
              Cancel
            </button>
          ) : null}
        </div>
      </header>

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
      {!editable ? (
        <p className="rounded-md border border-line bg-canvas px-3 py-2 text-[12.5px]">
          {quote.number} is {quote.status.replace(/_/g, " ")}, so its lines are frozen. Duplicate it
          into a new draft to change anything — what the customer read stays exactly as it was.
        </p>
      ) : null}
      {detail.decline_reason ? (
        <p data-qa-sales-decline className="rounded-md border border-negative/40 px-3 py-2 text-[12.5px]">
          The customer declined: <em>{detail.decline_reason}</em>
        </p>
      ) : null}
      {detail.cancel_reason ? (
        <p data-qa-sales-cancel-reason className="rounded-md border border-line px-3 py-2 text-[12.5px]">
          Cancelled: {detail.cancel_reason}
        </p>
      ) : null}

      {confirmCancel ? (
        <div className="rounded-lg border border-line bg-panel p-3 text-[12.5px]">
          <p>
            Cancel {quote.number}? The customer cannot accept it afterwards, and this cannot be
            undone.
          </p>
          <label className="mt-2 block">
            <span className="mb-1 block text-muted">Why (shown in the timeline)</span>
            <input
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              data-qa-sales-cancel-reason
              className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
            />
          </label>
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              onClick={() =>
                void act(
                  "cancel",
                  () => cancelSalesQuote(quote.id, reason.trim(), organizationId),
                  `${quote.number} cancelled.`,
                )
              }
              disabled={busy !== null}
              data-qa-sales-cancel-confirm
              className="inline-flex items-center gap-1.5 rounded-md bg-ink px-2.5 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
            >
              {busy === "cancel" ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
              Cancel the quote
            </button>
            <button
              type="button"
              onClick={() => setConfirmCancel(false)}
              data-qa-sales-cancel-abort
              className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              Keep it
            </button>
          </div>
        </div>
      ) : null}

      <div className="grid gap-3 lg:grid-cols-[1fr_20rem]">
        <section className="space-y-2 rounded-lg border border-line p-3">
          <h2 className="text-[13px] font-medium">Lines</h2>
          <div className="overflow-x-auto">
            <table className="w-full min-w-[40rem] border-collapse text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                  <th scope="col" className="w-8 px-2 py-1.5">#</th>
                  <th scope="col" className="px-2 py-1.5">Description</th>
                  <th scope="col" className="w-20 px-2 py-1.5 text-right">Qty</th>
                  <th scope="col" className="w-24 px-2 py-1.5 text-right">Unit price</th>
                  <th scope="col" className="w-16 px-2 py-1.5 text-right">Disc %</th>
                  <th scope="col" className="w-16 px-2 py-1.5 text-right">Tax %</th>
                  <th scope="col" className="w-28 px-2 py-1.5 text-right">Line total</th>
                </tr>
              </thead>
              <tbody>
                {lines.map((line) => (
                  <tr key={line.id} className="border-b border-line last:border-b-0">
                    <td className="px-2 py-1.5 text-muted">{line.position}</td>
                    <td className="px-2 py-1.5">
                      {line.description || line.product?.name || "—"}
                      {line.product ? (
                        <span className="ml-1.5 text-[11.5px] text-muted">{line.product.sku}</span>
                      ) : null}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {line.quantity} {line.unit}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {formatMoney(line.unit_price, quote.currency)}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {line.discount_percent > 0 ? `${line.discount_percent}%` : "—"}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {line.tax_percent > 0 ? `${line.tax_percent}%` : "—"}
                    </td>
                    <td className="px-2 py-1.5 text-right tabular-nums">
                      {formatMoney(line.line_total, quote.currency)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <dl className="ml-auto w-64 space-y-1 border-t border-line pt-2 text-[12.5px]">
            <TotalRow label="Subtotal" value={formatMoney(quote.totals.subtotal, quote.currency)} />
            <TotalRow label="Discount" value={formatMoney(quote.totals.discount_total, quote.currency)} />
            <TotalRow label="Tax" value={formatMoney(quote.totals.tax_total, quote.currency)} />
            <TotalRow label="Total" value={formatMoney(quote.totals.grand_total, quote.currency)} strong />
          </dl>
        </section>

        <aside className="space-y-3">
          <section className="space-y-2 rounded-lg border border-line p-3" data-qa-sales-customer>
            <h2 className="text-[13px] font-medium">Customer</h2>
            <p className="text-[13px]">{quote.customer.name || "—"}</p>
            <p className="text-[12px] text-muted">
              {quote.customer.kind === "contact" ? "Contact" : "Company"}
              {quote.customer.id ? "" : " · removed from the CRM, the name was kept on the quote"}
            </p>
            {quote.customer.id ? (
              <a
                href={`/crm/${quote.customer.kind === "contact" ? "contacts" : "companies"}/${quote.customer.id}`}
                className="inline-flex items-center gap-1 text-[12.5px] underline"
                data-qa-sales-customer-link
              >
                Open in the CRM
                <ExternalLink className="h-3 w-3" aria-hidden />
              </a>
            ) : null}
            {detail.notes ? (
              <p className="border-t border-line pt-2 text-[12.5px] text-muted">{detail.notes}</p>
            ) : null}
            {detail.reference ? (
              <p className="text-[12.5px] text-muted">Their reference: {detail.reference}</p>
            ) : null}
          </section>

          <section className="space-y-2 rounded-lg border border-line p-3" data-qa-sales-link>
            <h2 className="text-[13px] font-medium">Customer link</h2>
            {detail.has_public_link && !link ? (
              <p className="text-[12.5px] text-muted">
                A link was issued. Only its hash is stored, so it cannot be shown again — re-issue to
                get a new one. The previous link stops working the moment you do.
              </p>
            ) : null}
            {link && absoluteLink ? (
              <div className="space-y-1.5">
                <code
                  data-qa-sales-link-url
                  className="block break-all rounded-md border border-line bg-canvas px-2 py-1.5 text-[11.5px]"
                >
                  {absoluteLink}
                </code>
                <div className="flex gap-2">
                  <button
                    type="button"
                    onClick={() => {
                      void navigator.clipboard?.writeText(absoluteLink);
                      setNotice("The link is on the clipboard.");
                    }}
                    data-qa-sales-link-copy
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px]"
                  >
                    <Copy className="h-3 w-3" aria-hidden />
                    Copy
                  </button>
                  <a
                    href={absoluteLink}
                    target="_blank"
                    rel="noreferrer"
                    data-qa-sales-link-open
                    className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px]"
                  >
                    <ExternalLink className="h-3 w-3" aria-hidden />
                    Open it
                  </a>
                </div>
              </div>
            ) : null}
            <button
              type="button"
              onClick={() =>
                void act(
                  "link",
                  async () => {
                    const issued = await issueSalesQuoteLink(quote.id, organizationId);
                    setLink(issued.url);
                  },
                  link
                    ? "A new link was issued — the previous one no longer works."
                    : "The customer link is ready.",
                )
              }
              disabled={busy !== null || (quote.status !== "sent" && quote.status !== "accepted" && quote.status !== "approved")}
              data-qa-sales-link-issue
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12px] disabled:opacity-50"
            >
              {busy === "link" ? (
                <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
              ) : (
                <Link2 className="h-3 w-3" aria-hidden />
              )}
              {link || detail.has_public_link ? "Re-issue the link" : "Issue the link"}
            </button>
            {quote.status !== "sent" && quote.status !== "accepted" && quote.status !== "approved" ? (
              <p className="text-[11.5px] text-muted">Send the quote first — a draft has no link.</p>
            ) : null}
          </section>

          <section className="space-y-2 rounded-lg border border-line p-3" data-qa-sales-versions>
            <h2 className="text-[13px] font-medium">Versions the customer read</h2>
            {versions.length === 0 ? (
              <p className="text-[12.5px] text-muted">
                None yet. Sending snapshots everything the customer will see, and the snapshot is
                never edited afterwards.
              </p>
            ) : (
              <ol className="space-y-1.5">
                {versions.map((version) => (
                  <li key={version.version} className="flex items-center justify-between text-[12.5px]">
                    <span>
                      v{version.version} ·{" "}
                      <span className="text-muted">{version.sent_at.slice(0, 10)}</span>
                    </span>
                    <span className="tabular-nums">
                      {formatMoney(String(version.totals.grand_total ?? "0"), version.currency)}
                    </span>
                  </li>
                ))}
              </ol>
            )}
          </section>
        </aside>
      </div>

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => router.push("/sales/quotes")}
          data-qa-sales-detail-back
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          <RotateCcw className="h-3.5 w-3.5" aria-hidden />
          Back to the quotes
        </button>
      </div>
    </div>
  );
}

function TotalRow({ label, value, strong }: { label: string; value: string; strong?: boolean }) {
  return (
    <div className="flex items-center justify-between">
      <dt className={strong ? "font-medium" : "text-muted"}>{label}</dt>
      <dd className={strong ? "font-semibold tabular-nums" : "tabular-nums"}>{value}</dd>
    </div>
  );
}
