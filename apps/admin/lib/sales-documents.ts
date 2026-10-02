/**
 * Downloading a quote or an order as a PDF.
 *
 * This is the second file download in the sales module, and it follows the report export's shape
 * exactly (`lib/sales-reports.ts`): fetch, check, then write the blob. An `<a href>` would be
 * two lines shorter and wrong in a way that only shows up for the person who most needs the
 * file — the API answers a failed request with a JSON body, and a browser handed that body as a
 * download gives the seller a file called `quote-Q-2026-0001.pdf` whose contents are
 * `{"error": …}`. So the refusal is caught, and nothing is written unless the bytes are a PDF.
 */

/** What a download produced, and the one thing about it a screen may need to warn about. */
export type DownloadedDocument = {
  /** The name the browser saved it under. */
  filename: string;
  /**
   * Characters the document could not draw exactly, as the server counted them.
   *
   * A base-14 PDF font has no glyph for the Turkish dotless `ı` or for `ğ`, and none at all for
   * CJK, so a document containing them prints a readable near-miss or a `?`. The server counts
   * that and says so in a header, and this carries it to the screen: the *sender* is the only
   * person who can fix it, and a file that has already been emailed cannot be.
   */
  degradedCharacters: number;
};

/** Download a quote as a PDF. */
export function downloadQuotePdf(quoteId: string): Promise<DownloadedDocument> {
  return downloadPdf(`/api/v1/sales/quotes/${encodeURIComponent(quoteId)}/pdf`, "quote.pdf");
}

/** Download an order as a PDF. */
export function downloadOrderPdf(orderId: string): Promise<DownloadedDocument> {
  return downloadPdf(`/api/v1/sales/orders/${encodeURIComponent(orderId)}/pdf`, "order.pdf");
}

/** The header the server uses to say the document is degraded. */
const DEGRADED_HEADER = "x-omnion-document-degraded";

async function downloadPdf(path: string, fallbackName: string): Promise<DownloadedDocument> {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: { Accept: "application/pdf" },
  });
  if (!response.ok) {
    throw await readFailure(response);
  }
  // Checked before the blob is written, not after: a 200 that is not a PDF is either a proxy
  // error page or a route wired to the wrong handler, and either way the seller must not be left
  // with a file they will only find out is wrong when the customer says they cannot open it.
  const contentType = response.headers.get("content-type") ?? "";
  if (!contentType.toLowerCase().includes("application/pdf")) {
    throw new Error(
      `the server answered with ${contentType || "no content type"} instead of a PDF, so nothing was saved`,
    );
  }
  const blob = await response.blob();
  const filename = filenameFrom(response.headers.get("content-disposition")) ?? fallbackName;
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Revoked on a delay rather than immediately: a browser that has not started the download yet
  // cancels it, and the seller is left with no file and no error.
  window.setTimeout(() => URL.revokeObjectURL(url), 30_000);

  const raw = Number.parseInt(response.headers.get(DEGRADED_HEADER) ?? "0", 10);
  return {
    filename,
    degradedCharacters: Number.isFinite(raw) && raw > 0 ? raw : 0,
  };
}

/** The refusal body, turned into a message a screen can print. */
async function readFailure(response: Response): Promise<Error> {
  try {
    const body = (await response.json()) as { error?: { message?: string; request_id?: string } };
    const message = body?.error?.message ?? `the request failed (${response.status})`;
    // The request id is the one thing that makes a refusal reportable, and the server now stamps
    // it on every response — a message with no id sends an operator hunting for the exchange.
    const requestId = body?.error?.request_id;
    return new Error(requestId ? `${message} (request ${requestId})` : message);
  } catch {
    return new Error(`the request failed (${response.status})`);
  }
}

/**
 * The name the API asked for, read out of `Content-Disposition`.
 *
 * The RFC 5987 `filename*` form is preferred when present, because a Turkish organization's name
 * is not ASCII and the plain form is an all-underscore fallback: saving "Sirkket.pdf" for a
 * company called "Şirket" would be the same substitution the document layer reports, reintroduced
 * in the file name. It is percent-decoded, and anything that is not a plain name is refused.
 */
function filenameFrom(header: string | null): string | null {
  if (!header) return null;
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(header);
  if (encoded) {
    try {
      const value = decodeURIComponent(encoded[1].trim());
      if (/^[A-Za-z0-9._ -]+$/.test(value)) return value;
    } catch {
      // A malformed escape is not a name; fall through to the plain form.
    }
  }
  const match = /filename="([^"]+)"/.exec(header) ?? /filename=([^;]+)/.exec(header);
  if (!match) return null;
  const value = match[1].trim();
  // Only a plain, path-free name: a download name is attacker-influenced in the general case, and
  // a `../../` in one is a thing no legitimate document produces.
  return /^[A-Za-z0-9._-]+$/.test(value) ? value : null;
}

/**
 * The sentence a screen shows after a download, or `null` when there is nothing to say.
 *
 * Exported so the wording lives in one place: the degraded case is the one a person has to
 * understand before sending the file on, and two screens with two phrasings of it is how one of
 * them ends up saying "1 characters".
 */
export function documentNotice(document: DownloadedDocument): string | null {
  if (document.degradedCharacters === 0) return null;
  const count = document.degradedCharacters;
  return (
    `Downloaded, but ${count} character${count === 1 ? "" : "s"} could not be printed exactly` +
    ` — the built-in PDF fonts have no glyph for ${count === 1 ? "it" : "them"}.` +
    ` Check the customer's name before you send the file.`
  );
}
