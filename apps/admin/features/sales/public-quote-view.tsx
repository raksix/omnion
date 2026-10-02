"use client";

/**
 * The customer's copy of a quote (REQ-052, slice 2): `/q/{token}`.
 *
 * The one screen in the sales module a **customer** sees, so the rules are different in kind from
 * the panel's and worth stating:
 *
 * * **No sign-in, and no sign-up offer.** A page that shows a logged-out customer a "create an
 *   account to view this" box has told them the document is not for them.
 * * **The two buttons say what they do and the decline asks why.** "Accept" commits the customer;
 *   a decline with no reason is the one answer a seller cannot act on, so the field is required
 *   before the button does anything — and the requirement is stated on the screen, not discovered
 *   from a 400.
 * * **An expired or consumed link is a sentence, not an error page.** The customer is not doing
 *   anything wrong when a link lapses, and "this link is no longer valid" with the quote's number
 *   on it lets them call the seller; a 404 with a request id does not.
 * * **Nothing internal is here.** The payload the API sends is already the document — the
 *   customer, the lines, the totals, the validity — and this screen renders exactly that.
 */
import { useCallback, useEffect, useState } from "react";

import { Check, CircleX, Loader2 } from "lucide-react";

import { formatMoney } from "@/lib/sales";
import { fetchPublicSalesQuote, type SalesPublicQuote } from "@/lib/sales-quotes";

/** What the page shows, as a decision rather than as three booleans. */
type PageState =
  | { kind: "loading" }
  | { kind: "unavailable"; number?: string }
  | { kind: "ready"; quote: SalesPublicQuote; reason: string };

/** `/q/{token}`: the document, the two buttons, and the sentence a lapsed link gets. */
export function PublicQuoteView({ token }: { token: string }) {
  const [state, setState] = useState<PageState>({ kind: "loading" });
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState<"accept" | "decline" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    setState({ kind: "loading" });
    fetchPublicSalesQuote(token)
      .then((quote) => setState({ kind: "ready", quote, reason: "" }))
      .catch(() => setState({ kind: "unavailable" }));
  }, [token]);

  useEffect(load, [load]);

  const decide = useCallback(
    async (action: "accept" | "decline") => {
      setBusy(action);
      setError(null);
      try {
        const response = await fetch(`/api/v1/sales/public/quotes/${token}/${action}`, {
          method: "POST",
          credentials: "same-origin",
          headers: { "content-type": "application/json", accept: "application/json" },
          body: JSON.stringify({ note: reason }),
        });
        if (!response.ok) {
          const body = (await response.json().catch(() => null)) as
            | { error?: { message?: string } }
            | null;
          // A refusal that means the link is gone takes the customer to the expired sentence
          // rather than showing "this link is no longer valid" under a button they can retry.
          if (response.status === 404) {
            setState({ kind: "unavailable" });
            return;
          }
          setError(body?.error?.message ?? "That could not be recorded. Please try again.");
          return;
        }
        setState({ kind: "ready", quote: (await response.json()) as SalesPublicQuote, reason: "" });
      } catch {
        setError("The server could not be reached. Please try again.");
      } finally {
        setBusy(null);
      }
    },
    [reason, token],
  );

  if (state.kind === "loading") {
    return (
      <main className="mx-auto flex min-h-[60vh] max-w-3xl items-center justify-center" aria-busy="true">
        <Loader2 className="h-5 w-5 animate-spin text-muted" aria-hidden />
        <span className="sr-only">Loading the quote</span>
      </main>
    );
  }

  if (state.kind === "unavailable") {
    return (
      <main className="mx-auto max-w-3xl px-4 py-16 text-center" data-qa-public-quote-unavailable>
        <CircleX className="mx-auto h-6 w-6 text-muted" aria-hidden />
        <h1 className="mt-3 text-[16px] font-semibold">This link is no longer valid</h1>
        <p className="mx-auto mt-2 max-w-md text-[13px] text-muted">
          The quote it pointed at has been withdrawn, has already been answered, or has passed its
          validity date. If you expected to see it, reply to the sender and ask for a fresh link.
        </p>
      </main>
    );
  }

  const { quote } = state;
  const decided = quote.decided || quote.status !== "sent";

  return (
    <main className="mx-auto max-w-3xl px-4 py-10" data-qa-public-quote>
      <article className="rounded-lg border border-line bg-panel p-6">
        <header className="flex flex-wrap items-start justify-between gap-3 border-b border-line pb-4">
          <div>
            <p className="text-[12px] uppercase tracking-wide text-muted">Quote</p>
            <h1 className="text-[18px] font-semibold" data-qa-public-quote-number>
              {quote.number}
            </h1>
            {quote.title ? <p className="text-[13px] text-muted">{quote.title}</p> : null}
          </div>
          <div className="text-right text-[12.5px] text-muted">
            <p>Prepared for {quote.customer_name || "you"}</p>
            <p data-qa-public-quote-valid>Valid until {quote.valid_until}</p>
          </div>
        </header>

        {decided ? (
          <p
            data-qa-public-quote-decided
            className="mt-4 rounded-md border border-line bg-canvas px-3 py-2 text-[13px]"
          >
            {quote.status === "accepted"
              ? "You accepted this quote. Thank you — the sender will be in touch about the next step."
              : quote.status === "declined"
                ? "You declined this quote. The sender has been told."
                : quote.status === "expired"
                  ? "This quote's validity date has passed. Ask the sender for a fresh one."
                  : "This quote has been withdrawn."}
          </p>
        ) : null}

        <div className="mt-5 overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-2 py-2 font-medium">Item</th>
                <th scope="col" className="px-2 py-2 text-right font-medium">Qty</th>
                <th scope="col" className="px-2 py-2 text-right font-medium">Unit price</th>
                <th scope="col" className="px-2 py-2 text-right font-medium">Discount</th>
                <th scope="col" className="px-2 py-2 text-right font-medium">Total</th>
              </tr>
            </thead>
            <tbody>
              {quote.lines.map((line, index) => (
                <tr key={index} className="border-b border-line last:border-b-0" data-qa-public-quote-line={index + 1}>
                  <td className="px-2 py-2">{line.description || "—"}</td>
                  <td className="px-2 py-2 text-right tabular-nums">
                    {line.quantity} {line.unit}
                  </td>
                  <td className="px-2 py-2 text-right tabular-nums">
                    {formatMoney(line.unit_price, quote.currency)}
                  </td>
                  <td className="px-2 py-2 text-right tabular-nums">
                    {line.discount_percent > 0 ? `${line.discount_percent}%` : "—"}
                  </td>
                  <td className="px-2 py-2 text-right tabular-nums">
                    {formatMoney(line.line_total, quote.currency)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <dl className="ml-auto mt-4 w-64 space-y-1 border-t border-line pt-3 text-[13px]">
          <div className="flex justify-between">
            <dt className="text-muted">Subtotal</dt>
            <dd className="tabular-nums">{formatMoney(quote.totals.subtotal, quote.currency)}</dd>
          </div>
          <div className="flex justify-between">
            <dt className="text-muted">Discount</dt>
            <dd className="tabular-nums">
              {formatMoney(quote.totals.discount_total, quote.currency)}
            </dd>
          </div>
          <div className="flex justify-between">
            <dt className="text-muted">Tax</dt>
            <dd className="tabular-nums">{formatMoney(quote.totals.tax_total, quote.currency)}</dd>
          </div>
          <div className="flex justify-between border-t border-line pt-1 text-[14px] font-semibold">
            <dt>Total</dt>
            <dd className="tabular-nums" data-qa-public-quote-total>
              {formatMoney(quote.totals.grand_total, quote.currency)}
            </dd>
          </div>
        </dl>

        {quote.notes ? (
          <p className="mt-4 border-t border-line pt-3 text-[13px] text-muted">{quote.notes}</p>
        ) : null}

        {decided ? null : (
          <section className="mt-6 border-t border-line pt-4" data-qa-public-quote-actions>
            <label className="block text-[12.5px]">
              <span className="mb-1 block text-muted">
                A note for the sender (required if you decline, so they know why)
              </span>
              <textarea
                value={reason}
                onChange={(event) => setReason(event.target.value)}
                rows={2}
                data-qa-public-quote-note
                className="w-full rounded-md border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
              />
            </label>
            {error ? (
              <p data-qa-public-quote-error className="mt-2 text-[12.5px] text-negative">
                {error}
              </p>
            ) : null}
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <button
                type="button"
                onClick={() => void decide("accept")}
                disabled={busy !== null}
                data-qa-public-quote-accept
                className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] font-medium text-panel disabled:opacity-60"
              >
                {busy === "accept" ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                ) : (
                  <Check className="h-3.5 w-3.5" aria-hidden />
                )}
                Accept this quote
              </button>
              <button
                type="button"
                onClick={() => void decide("decline")}
                disabled={busy !== null}
                data-qa-public-quote-decline
                className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-60"
              >
                {busy === "decline" ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                ) : (
                  <CircleX className="h-3.5 w-3.5" aria-hidden />
                )}
                Decline
              </button>
            </div>
          </section>
        )}
      </article>
    </main>
  );
}
