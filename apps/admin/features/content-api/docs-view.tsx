"use client";

/**
 * `/content-api/docs` — the Docs tab (REQ-019, slice 2).
 *
 * Rendered from the OpenAPI document the server serves, not from a hand-written list beside it: a
 * second copy of the route table is a second answer to "what does this API accept", and the copy
 * that goes stale is the one on screen. Every heading below comes from the document, so the only
 * way an endpoint can be missing from this tab is for it to be missing from the API.
 *
 * Four things this screen has to do that a list of endpoints cannot:
 *
 * 1. **Explain the pagination, not just list the parameter.** `cursor` is opaque, and an
 *    integrator's first question is "what do I do with `next_cursor`". There is therefore a
 *    working two-request example, generated from the document's own paths.
 *
 * 2. **Show the error contract in one place.** The codes a caller must branch on are the product;
 *    they are printed from the document's error schema rather than typed into this file, for the
 *    same reason the endpoint list is not typed in either.
 *
 * 3. **Be able to leave with the document.** JSON and YAML downloads plus a copyable base URL —
 *    because the screen a developer reads last is the one whose URL they paste somewhere.
 *
 * 4. **Say the thing the request text got wrong.** The brief lists `/api/v1/media`; that path is
 *    the panel's session-authenticated media CRUD API, and a frontend wired to it will get the
 *    panel's semantics instead of the headless surface's. The document carries that note and this
 *    tab surfaces it rather than burying it in a spec.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertCircle,
  Check,
  Copy,
  Download,
  FileJson,
  FileText,
  KeyRound,
  Loader2,
  RefreshCw,
} from "lucide-react";

import {
  ApiError,
  downloadContentApiOpenApi,
  fetchContentApiOpenApi,
  saveBlob,
} from "@/lib/api";
import type { OpenApiDocument, OpenApiOperation } from "@/lib/types";

/** One row of the endpoint table, flattened out of the document's two levels. */
type EndpointRow = {
  path: string;
  method: string;
  operation: OpenApiOperation;
};

/**
 * Flatten `paths` into rows, in the document's own order.
 *
 * A path entry can hold several methods, so this is a loop over both levels — and the sort is the
 * document's insertion order (a JS object preserves string-key order), which is the order the
 * server decided an integrator should read them in.
 */
function rowsOf(document: OpenApiDocument): EndpointRow[] {
  const rows: EndpointRow[] = [];
  for (const [path, methods] of Object.entries(document.paths ?? {})) {
    for (const [method, operation] of Object.entries(methods)) {
      if (!operation || typeof operation !== "object" || !operation.operationId) continue;
      rows.push({ path, method, operation });
    }
  }
  return rows;
}

/** The first path in the document that is a *real* endpoint rather than the `/api/v1/media` note. */
function primaryContentPath(rows: EndpointRow[]): string | null {
  const list = rows.find((row) => row.operation.operationId === "pages.list");
  if (list) return list.path;
  return rows.find((row) => row.path.startsWith("/api/v1/content/"))?.path ?? null;
}

export function ContentApiDocsView() {
  const [document, setDocument] = useState<OpenApiDocument | null>(null);
  const [error, setError] = useState<{ message: string; code: string } | null>(null);
  const [loading, setLoading] = useState(true);
  const [notice, setNotice] = useState<string | null>(null);
  const [openPath, setOpenPath] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const value = await fetchContentApiOpenApi();
      setDocument(value);
      // The first row opens by default, so the tab shows *how* an endpoint looks rather than a
      // list a reader has to click into to learn the shape of the thing.
      setOpenPath((current) => current ?? Object.keys(value.paths ?? {})[0] ?? null);
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { message: cause.message, code: cause.code }
          : { message: "The API document could not be loaded.", code: "unknown_error" },
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const rows = useMemo(() => (document ? rowsOf(document) : []), [document]);
  const baseUrl = document?.servers?.[0]?.url ?? "";
  const contentPath = primaryContentPath(rows);
  const errorSchema = document?.components?.schemas?.Error;
  const errorCodes =
    typeof errorSchema?.description === "string" ? [] : extractCodes(errorSchema?.properties);

  const copy = useCallback(async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setNotice(`${label} copied`);
    } catch {
      setNotice("The clipboard is not available in this browser — select the text and copy it.");
    }
  }, []);

  const download = useCallback(
    async (format: "json" | "yaml") => {
      setNotice(null);
      try {
        const file = await downloadContentApiOpenApi(format);
        saveBlob(file.blob, file.filename);
        setNotice(`Downloaded ${file.filename}`);
      } catch (cause) {
        setNotice(
          cause instanceof ApiError ? cause.message : "The document could not be downloaded.",
        );
      }
    },
    [],
  );

  // `g` then `d` jumps to the downloads, `c` copies the base URL. Ignored while a field has focus,
  // for the same reason every other screen in the panel does it: a form is not a keyboard surface
  // for the screen behind it.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        target?.isContentEditable === true;
      if (event.metaKey || event.ctrlKey || event.altKey || typing) return;
      if (event.key === "c" && baseUrl) {
        event.preventDefault();
        void copy(baseUrl, "Base URL");
      }
      if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [baseUrl, copy, load]);

  if (loading) {
    return (
      <div data-content-api-docs className="flex flex-col gap-4">
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          Loading the API document…
        </p>
        <div className="h-40 rounded-xl border border-line bg-quiet-soft" />
      </div>
    );
  }

  if (error || document === null) {
    return (
      <div data-content-api-docs className="flex flex-col gap-3">
        <p
          data-content-api-docs-error
          className="flex items-start gap-2 rounded-xl border border-red-500/40 px-3 py-2 text-[12.5px] text-red-700 dark:text-red-400"
        >
          <AlertCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {error?.message ?? "The API document could not be loaded."}
            {error?.code ? (
              <span className="ml-1.5 font-mono text-[11px] opacity-80">{error.code}</span>
            ) : null}
          </span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          data-content-api-docs-retry
          className="flex w-fit items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div data-content-api-docs className="flex flex-col gap-4">
      <header className="rounded-xl border border-line bg-surface px-4 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-[13.5px] font-medium">{document.info.title}</h2>
          <span className="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-muted">
            OpenAPI {document.openapi}
          </span>
          <span className="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-muted">
            v{document.info.version}
          </span>
          <div className="ml-auto flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => void download("json")}
              data-content-api-download-json
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
            >
              <FileJson className="size-3.5" aria-hidden />
              Download OpenAPI (JSON)
            </button>
            <button
              type="button"
              onClick={() => void download("yaml")}
              data-content-api-download-yaml
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
            >
              <Download className="size-3.5" aria-hidden />
              Download OpenAPI (YAML)
            </button>
          </div>
        </div>
        <p className="mt-2 max-w-3xl text-[12.5px] text-muted">{document.info.description}</p>
        <div className="mt-2 flex flex-wrap items-center gap-2 text-[12px]">
          <span className="text-muted">Base URL</span>
          <code data-content-api-base-url className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11.5px]">
            {baseUrl || "—"}
          </code>
          <button
            type="button"
            onClick={() => void copy(baseUrl, "Base URL")}
            data-content-api-copy-base
            className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink transition hover:bg-quiet-soft"
          >
            <Copy className="size-3" aria-hidden />
            Copy base URL
          </button>
          {notice ? (
            <span data-content-api-docs-notice className="flex items-center gap-1.5 text-[11.5px] text-muted">
              <Check className="size-3" aria-hidden />
              {notice}
            </span>
          ) : null}
        </div>
      </header>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Endpoints</h3>
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full min-w-[640px] text-left text-[12.5px]">
            <thead className="border-b border-line bg-quiet-soft text-[11px] tracking-wide text-muted uppercase">
              <tr>
                <th scope="col" className="px-3 py-2 font-medium">Endpoint</th>
                <th scope="col" className="px-3 py-2 font-medium">Scope</th>
                <th scope="col" className="px-3 py-2 font-medium">What it is for</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const expanded = openPath === row.path;
                return (
                  <tr
                    key={`${row.method} ${row.path}`}
                    data-content-api-endpoint={row.operation.operationId}
                    className="border-b border-line last:border-0"
                  >
                    <td className="px-3 py-2 align-top">
                      <button
                        type="button"
                        onClick={() => setOpenPath(expanded ? null : row.path)}
                        aria-expanded={expanded}
                        className="flex items-center gap-2 text-left"
                      >
                        <span className="rounded border border-line px-1 py-0.5 font-mono text-[10.5px] uppercase">
                          {row.method}
                        </span>
                        <code className="font-mono text-[11.5px] break-all">{row.path}</code>
                      </button>
                      {row.operation["x-experimental"] ? (
                        <span className="mt-1 inline-block rounded border border-amber-500/40 px-1.5 py-0.5 text-[10.5px] text-amber-700 dark:text-amber-400">
                          experimental
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2 align-top font-mono text-[11.5px] text-muted">
                      {row.operation["x-required-scope"] ?? "valid token"}
                    </td>
                    <td className="px-3 py-2 align-top text-muted">{row.operation.summary}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </section>

      {openPath
        ? rows
            .filter((row) => row.path === openPath)
            .map((row) => (
              <OperationDetail key={`detail-${row.method} ${row.path}`} row={row} baseUrl={baseUrl} />
            ))
        : null}

      {contentPath ? <PaginationGuide path={contentPath} baseUrl={baseUrl} /> : null}

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Errors</h3>
        <p className="max-w-3xl text-[12.5px] text-muted">
          Every failure uses the same envelope. Branch on <code className="font-mono">code</code> —
          never on the message, which is written for a human and will be reworded.
        </p>
        <ul className="grid gap-1.5 sm:grid-cols-2">
          {errorCodes.map((entry) => (
            <li
              key={entry.code}
              data-content-api-error-code={entry.code}
              className="rounded-lg border border-line px-3 py-2"
            >
              <code className="font-mono text-[11.5px]">{entry.code}</code>
              <span className="ml-2 text-[12px] text-muted">{entry.meaning}</span>
            </li>
          ))}
        </ul>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Rate limiting</h3>
        <p className="max-w-3xl text-[12.5px] text-muted">
          Every token has its own per-minute ceiling — Standard is 120/min, Elevated 600/min, set on
          the token. Past it the API answers <code className="font-mono">429 rate_limited</code> with
          a <code className="font-mono">Retry-After</code> header in seconds; wait that long rather
          than retrying immediately, or the limiter keeps saying no for the same reason.
        </p>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Rebuild on change</h3>
        <p className="max-w-3xl text-[12.5px] text-muted">
          Polling is the wrong shape for a static frontend. Subscribe a build hook to
          <code className="font-mono"> page.published</code>,{" "}
          <code className="font-mono">page.updated</code>,{" "}
          <code className="font-mono">media.updated</code> and{" "}
          <code className="font-mono">translation.published</code> under Webhooks, then rebuild. To
          catch up on what you missed, ask for only what changed:
        </p>
        <pre
          data-content-api-rebuild-example
          className="overflow-x-auto rounded-xl border border-line bg-quiet-soft px-3 py-2 font-mono text-[11.5px] whitespace-pre-wrap"
        >
          {`# everything that changed since the last successful build\ncurl -H "Authorization: Bearer ${"omn_<prefix>_<secret>"}" \\\n  "${baseUrl}/content/pages?updated_since=2026-01-31T09:00:00Z"}`}
        </pre>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Schemas</h3>
        <div className="grid gap-2 sm:grid-cols-2">
          {Object.entries(document.components?.schemas ?? {}).map(([name, schema]) => (
            <article key={name} className="rounded-xl border border-line px-3 py-2">
              <h4 className="font-mono text-[12px]">{name}</h4>
              <p className="mt-1 text-[12px] text-muted">
                {schema.description ?? "No description in the document."}
              </p>
              {schema.required?.length ? (
                <p className="mt-1 font-mono text-[11px] text-muted">
                  required: {schema.required.join(", ")}
                </p>
              ) : null}
            </article>
          ))}
        </div>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12.5px] font-medium">Authentication</h3>
        <p className="flex items-start gap-2 text-[12.5px] text-muted">
          <KeyRound className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            <code className="font-mono">omn_&lt;prefix&gt;_&lt;secret&gt;</code> in the{" "}
            <code className="font-mono">Authorization: Bearer</code> header. The plaintext is shown
            once when the token is created and stored hashed, so it cannot be recovered — rotate to
            get a new one. Drafts and revision history are never served here; preview access is a
            separate signed link.
          </span>
        </p>
      </section>
    </div>
  );
}

/** The one operation's parameters and responses, expanded. */
function OperationDetail({ row, baseUrl }: { row: EndpointRow; baseUrl: string }) {
  const parameters = row.operation.parameters ?? [];
  const responses = Object.entries(row.operation.responses ?? {});
  return (
    <section
      data-content-api-detail={row.operation.operationId}
      className="flex flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3"
    >
      <h3 className="font-mono text-[12.5px]">
        {row.method.toUpperCase()} {row.path}
      </h3>
      <p className="text-[12.5px] text-muted">{row.operation.summary}</p>
      {parameters.length ? (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[520px] text-left text-[12px]">
            <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
              <tr>
                <th scope="col" className="px-2 py-1.5 font-medium">Parameter</th>
                <th scope="col" className="px-2 py-1.5 font-medium">In</th>
                <th scope="col" className="px-2 py-1.5 font-medium">Meaning</th>
              </tr>
            </thead>
            <tbody>
              {parameters.map((parameter) => (
                <tr key={`${parameter.in}-${parameter.name}`} className="border-b border-line last:border-0">
                  <td className="px-2 py-1.5 font-mono">
                    {parameter.name}
                    {parameter.required ? <span className="ml-1 text-red-600">*</span> : null}
                  </td>
                  <td className="px-2 py-1.5 text-muted">{parameter.in}</td>
                  <td className="px-2 py-1.5 text-muted">{parameter.description}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <p className="text-[12px] text-muted">This endpoint takes no parameters.</p>
      )}
      <div>
        <h4 className="text-[11px] tracking-wide text-muted uppercase">Responses</h4>
        <ul className="mt-1 flex flex-wrap gap-2">
          {responses.map(([code, response]) => (
            <li
              key={code}
              data-content-api-response={code}
              className="rounded-lg border border-line px-2 py-1 text-[11.5px]"
            >
              <span className="font-mono">{code}</span>
              <span className="ml-1.5 text-muted">{response.description}</span>
            </li>
          ))}
        </ul>
      </div>
      <p className="text-[11.5px] text-muted">
        Example: <code className="font-mono">{`curl -H "Authorization: Bearer omn_…" ${baseUrl}${row.path.replace("{slug}", "example-slug")}`}</code>
      </p>
    </section>
  );
}

/**
 * The cursor walk, with real requests to paste.
 *
 * The parameter table says `cursor` is "opaque"; that is not enough for the one question an
 * integrator actually has. This is the two-request loop, written out, with the two lines that
 * matter marked: the response's `next_cursor` going *into* the next request, and its absence
 * meaning the last page rather than an empty page.
 */
function PaginationGuide({ path, baseUrl }: { path: string; baseUrl: string }) {
  const [copied, setCopied] = useState(false);
  const example = `1. first page\n   curl -H "Authorization: Bearer omn_…" "${baseUrl}${path}?limit=2"\n\n2. next page — pass the cursor the response gave you\n   curl -H "Authorization: Bearer omn_…" "${baseUrl}${path}?limit=2&cursor=<next_cursor>"\n\n3. stop when next_cursor is null — that is the last page, not an empty one.`;
  return (
    <section data-content-api-pagination className="flex flex-col gap-2 rounded-xl border border-line px-4 py-3">
      <div className="flex items-center gap-2">
        <h3 className="text-[12.5px] font-medium">Pagination</h3>
        <button
          type="button"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(example);
              setCopied(true);
              window.setTimeout(() => setCopied(false), 2_000);
            } catch {
              setCopied(false);
            }
          }}
          data-content-api-pagination-copy
          className="ml-auto flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink transition hover:bg-quiet-soft"
        >
          {copied ? <Check className="size-3" aria-hidden /> : <Copy className="size-3" aria-hidden />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <p className="text-[12.5px] text-muted">
        Every list answers <code className="font-mono">items</code>,{" "}
        <code className="font-mono">next_cursor</code> and <code className="font-mono">count</code>.
        The cursor is a signed keyset over the sort column and the row id, so a row deleted between
        two pages shifts nothing — which is the reason it is opaque and must not be parsed.
      </p>
      <pre className="overflow-x-auto rounded-lg bg-quiet-soft px-3 py-2 font-mono text-[11.5px] whitespace-pre-wrap">
        {example}
      </pre>
      <p className="flex items-start gap-2 text-[11.5px] text-muted">
        <FileText className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        <span>
          Under a <code className="font-mono">fields</code> projection the identity keys survive —
          <code className="font-mono"> id</code>, <code className="font-mono">slug</code>,{" "}
          <code className="font-mono">type</code>, <code className="font-mono">locale</code>,{" "}
          <code className="font-mono">updated_at</code> and <code className="font-mono">etag</code> —
          because a page a client cannot key or revalidate is a page it cannot cache.
        </span>
      </p>
    </section>
  );
}

/** One line of the error-code list, split out of the document's own prose. */
const ERROR_MEANINGS: Record<string, string> = {
  invalid_parameter: "a query parameter is not acceptable; `details.field` names it",
  invalid_token: "no token, or a token that matches nothing",
  token_expired: "the token's `expires_at` has passed",
  token_revoked: "the token was revoked",
  insufficient_scope: "the token lacks this endpoint's scope",
  not_found: "no such item, or it is not published",
  rate_limited: "past the token's per-minute ceiling; retry after `Retry-After`",
  internal_error: "something failed on the server",
};

/**
 * The error codes, read out of the document rather than typed here.
 *
 * The schema's description is one long sentence with the codes in it; splitting on the backticks
 * picks them up, and the `ERROR_MEANINGS` map gives each its own line. Anything the document names
 * and this map does not know still appears — with no invented meaning, which is the honest way to
 * render a contract that grew.
 */
function extractCodes(properties: Record<string, unknown> | undefined): { code: string; meaning: string }[] {
  const described = properties?.error as { properties?: { code?: { description?: string } } } | undefined;
  const sentence = described?.properties?.code?.description;
  if (!sentence) {
    return Object.entries(ERROR_MEANINGS).map(([code, meaning]) => ({ code, meaning }));
  }
  const found = [...sentence.matchAll(/`([a-z_]+)`/g)].map((match) => match[1]);
  const codes = found.length ? found : Object.keys(ERROR_MEANINGS);
  return codes.map((code) => ({ code, meaning: ERROR_MEANINGS[code] ?? "" }));
}