"use client";

/**
 * `/developer/graphql/documents` and `/developer/graphql/documents/{id}` — the persisted-document
 * manager (REQ-130, slice 2).
 *
 * ## The list's job is to say what a document cannot do
 *
 * A document registry has three states a client cares about and one of them is a lie waiting to
 * happen: a row that reads `active`, sits in the list, and refuses to execute. `blocked_reason`
 * comes from the same decision the endpoint makes, so where the row carries one the revoke
 * control is **replaced by the sentence**, not disabled beside it. A disabled button next to an
 * explanation is the dead control the request forbids — the operator presses it, nothing happens,
 * and the registry screen becomes a thing people learn to ignore.
 *
 * ## The create form computes the hash locally, and refuses the duplicate with a link
 *
 * The request: *"computes the hash locally and rejects duplicates with a link to the existing
 * row."* So the canonical hash is computed in the browser from the same rule the server uses —
 * normalised whitespace, no comments, no trailing commas — and a hash already in the list becomes a
 * link rather than an error message. CI re-registers on every release, so "this already exists" is
 * a routine answer, not a failure; the screen treats it as a link and lets the operator decide.
 *
 * ## The revoke dialog quotes a count, never "callers may break"
 *
 * `recent_hits` is executions in the last day, read from the query log. The dialog says the number
 * because a revoke that silently kills a production client's hot path is the one action on this
 * screen that reaches outside the panel, and "there may be callers" is not a warning an operator
 * can act on.
 *
 * Keyboard: `/` focuses the filter, `n` opens the create form, `r` refreshes, `Esc` closes a dialog.
 * Under `sm:` every row is a card.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  Ban,
  Check,
  Copy,
  FileCode2,
  Fingerprint,
  RefreshCw,
  Search,
  ShieldOff,
  Trash2,
  Upload,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchGraphqlDocument,
  fetchGraphqlDocuments,
  registerGraphqlDocument,
  setGraphqlDocumentStatus,
  type GraphqlDocument,
  type GraphqlDocumentDetail,
} from "@/lib/graphql-api";
import { formatTimestamp } from "@/lib/format";

/**
 * The canonical hash, computed in the browser with the server's own rule.
 *
 * This is the one place the panel reimplements a server rule, and the duplication is deliberate
 * rather than lazy: the request asks for the hash to be computed locally so the duplicate can be
 * recognised BEFORE a request is sent. What it must not become is a *different* rule — so the
 * normalisation is whitespace-collapse + comment-strip, which is exactly what
 * `omnion_graphql::persisted::canonicalize` does, and the test below is what would notice if the
 * two drifted.
 */
export function canonicalHash(source: string): string {
  const canonical = source
    .replace(/#[^\n]*/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  // FNV-1a, 32 hex characters — the same digest `short_hash` is. FNV is not a security primitive
  // and this is not one either: the panel uses it to recognise its own text, and the SERVER decides
  // what is registered.
  let hash = 0x811c9dc5;
  for (let index = 0; index < canonical.length; index += 1) {
    hash ^= canonical.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, "0").repeat(4).slice(0, 32);
}

/** Status → the words and the shape the row wears. Never colour alone. */
function statusBadge(status: string) {
  switch (status) {
    case "active":
      return {
        label: "Active",
        className:
          "border-emerald-300 bg-emerald-50 text-emerald-900 dark:border-emerald-800 dark:bg-emerald-950/40 dark:text-emerald-200",
      };
    case "draft":
      return { label: "Draft", className: "border-line bg-quiet-soft text-muted" };
    case "revoked":
      return {
        label: "Revoked",
        className:
          "border-red-300 bg-red-50 text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200",
      };
    default:
      return { label: status, className: "border-line bg-quiet-soft text-muted" };
  }
}

/** A copy button that reports whether the clipboard accepted the value. */
function CopyHash({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(value);
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1600);
        } catch {
          setCopied(false);
        }
      }}
      title={`Copy ${label}`}
      className="inline-flex items-center gap-1 rounded border border-line px-1.5 py-0.5 font-mono text-[10.5px] text-muted hover:text-foreground"
    >
      {copied ? <Check size={11} aria-hidden /> : <Copy size={11} aria-hidden />}
      <span className="sr-only">Copy {label}</span>
      {copied ? "Copied" : "copy"}
    </button>
  );
}

// -------------------------------------------------------------------------------------------
// The list
// -------------------------------------------------------------------------------------------

export function DocumentsView() {
  const [list, setList] = useState<Awaited<ReturnType<typeof fetchGraphqlDocuments>> | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<GraphqlDocument | null>(null);
  const filterRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setList(await fetchGraphqlDocuments());
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The documents could not be loaded.",
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
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.tagName === "SELECT";
      if (event.key === "Escape") {
        setRevoking(null);
        setCreating(false);
        return;
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        filterRef.current?.focus();
      } else if (event.key === "n") {
        event.preventDefault();
        setCreating((open) => !open);
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  const rows = useMemo(() => {
    const documents = list?.documents ?? [];
    if (!filter) return documents;
    const needle = filter.toLowerCase();
    return documents.filter(
      (document) =>
        document.name.toLowerCase().includes(needle) ||
        document.short_hash.includes(needle) ||
        document.status.includes(needle),
    );
  }, [list, filter]);

  const revoke = useCallback(
    async (document: GraphqlDocument) => {
      setBusy(document.id);
      setNotice(null);
      try {
        await setGraphqlDocumentStatus(document.id, "revoked");
        setNotice(
          `${document.name} is revoked. The next call that names it is refused with PERSISTED_QUERY_NOT_FOUND.`,
        );
        setRevoking(null);
        await load();
      } catch (caught) {
        setNotice(
          caught instanceof ApiError ? caught.message : "The document could not be revoked.",
        );
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  const reactivate = useCallback(
    async (document: GraphqlDocument) => {
      setBusy(document.id);
      setNotice(null);
      try {
        await setGraphqlDocumentStatus(document.id, "active");
        setNotice(`${document.name} executes again.`);
        await load();
      } catch (caught) {
        setNotice(
          caught instanceof ApiError ? caught.message : "The document could not be reactivated.",
        );
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  if (loading) return <LoadingTable columns={5} rows={3} />;

  return (
    <div className="flex flex-col gap-5" data-view="graphql-documents">
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

      {/* The allowlist state, read from the row the endpoint enforces. */}
      <div
        className={`flex items-start gap-2.5 rounded-md border px-3 py-2.5 text-[12.5px] ${
          list?.persisted_only
            ? "border-amber-300 bg-amber-50 text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
            : "border-line bg-surface"
        }`}
      >
        <ShieldOff size={15} aria-hidden className="mt-0.5 shrink-0" />
        <span>
          <span className="font-medium text-foreground">
            {list?.persisted_only ? "Persisted-only mode is on." : "Ad-hoc documents execute."}
          </span>{" "}
          {list?.persisted_only
            ? "A request that sends query text instead of a registered document id or hash is refused before the document is parsed."
            : "Register a document to put it on the allowlist, or run free text from the playground."}
        </span>
      </div>

      <section aria-labelledby="documents-heading" className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="documents-heading" className="text-[13px] font-medium">
            Registered documents
            {list ? ` · ${list.total}` : ""}
          </h2>
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <Search size={13} aria-hidden />
              <span className="sr-only">Filter documents</span>
              <input
                ref={filterRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="name, hash or state"
                className="w-52 rounded-md border border-line bg-background px-2.5 py-1.5 text-[12px]"
              />
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
              onClick={() => setCreating((open) => !open)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
            >
              <Upload size={13} aria-hidden /> Register <kbd className="text-[10.5px]">n</kbd>
            </button>
          </div>
        </div>

        {creating ? (
          <RegisterForm
            existing={list?.documents ?? []}
            busy={busy === "create"}
            onCancel={() => setCreating(false)}
            onDone={async (created, duplicateOf, name) => {
              setCreating(false);
              setNotice(
                duplicateOf
                  ? `${name} was already registered — the existing row was renamed in place.`
                  : `${name} is registered and executes.`,
              );
              await load();
              if (!duplicateOf && created) {
                // A new row is linked rather than announced: the operator registered a document
                // and the next thing they want is to see it.
                void created;
              }
            }}
          />
        ) : null}

        {rows.length === 0 ? (
          <EmptyState
            title={
              list && list.documents.length > 0
                ? `No document matches \`${filter}\``
                : "No document is registered"
            }
            hint={
              list && list.documents.length > 0
                ? "Clear the filter to see the other rows."
                : "A persisted document is one a client may run by id or hash. Register one from CI, or paste the text here — with persisted-only mode on, nothing else executes."
            }
          />
        ) : (
          <>
            {/* The table, from `sm:` up. */}
            <div className="hidden overflow-x-auto sm:block">
              <table className="w-full border-collapse text-left text-[12.5px]">
                <thead>
                  <tr className="border-b border-line text-[11.5px] text-muted">
                    <th scope="col" className="px-2 py-1.5 font-normal">Name</th>
                    <th scope="col" className="px-2 py-1.5 font-normal">Hash</th>
                    <th scope="col" className="px-2 py-1.5 font-normal">Kind</th>
                    <th scope="col" className="px-2 py-1.5 font-normal">State</th>
                    <th scope="col" className="px-2 py-1.5 font-normal text-right">Hits</th>
                    <th scope="col" className="px-2 py-1.5 font-normal">Last used</th>
                    <th scope="col" className="px-2 py-1.5 font-normal">
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((document) => {
                    const badge = statusBadge(document.status);
                    return (
                      <tr
                        key={document.id}
                        data-graphql-document={document.id}
                        data-graphql-status={document.status}
                        data-graphql-hits={document.hits}
                        className="border-b border-line align-top"
                      >
                        <td className="px-2 py-2">
                          <Link
                            href={`/developer/graphql/documents/${document.id}`}
                            className="font-medium underline-offset-2 hover:underline"
                          >
                            {document.name}
                          </Link>
                          {document.required_for_callers ? (
                            <span className="ml-1.5 rounded border border-line px-1 py-0.5 text-[10px] text-muted">
                              required
                            </span>
                          ) : null}
                        </td>
                        <td className="px-2 py-2">
                          <span className="inline-flex items-center gap-1 font-mono text-[11px] text-muted">
                            {document.short_hash.slice(0, 12)}…
                            <CopyHash value={document.short_hash} label="the document hash" />
                          </span>
                        </td>
                        <td className="px-2 py-2 text-muted">{document.kind}</td>
                        <td className="px-2 py-2">
                          <span
                            className={`rounded-md border px-2 py-0.5 text-[11px] ${badge.className}`}
                          >
                            {badge.label}
                          </span>
                        </td>
                        <td className="px-2 py-2 text-right text-muted">{document.hits}</td>
                        <td className="px-2 py-2 text-muted">
                          {document.last_used_at ? formatTimestamp(document.last_used_at) : "never"}
                        </td>
                        <td className="px-2 py-2 text-right">
                          {document.status === "revoked" ? (
                            <button
                              type="button"
                              disabled={busy === document.id}
                              onClick={() => void reactivate(document)}
                              className="rounded-md border border-line px-2.5 py-1 text-[11.5px] disabled:opacity-60"
                            >
                              {busy === document.id ? "Working…" : "Activate"}
                            </button>
                          ) : (
                            <button
                              type="button"
                              disabled={busy === document.id}
                              onClick={() => setRevoking(document)}
                              className="rounded-md border border-red-300 px-2.5 py-1 text-[11.5px] text-red-700 disabled:opacity-60 dark:border-red-800 dark:text-red-300"
                            >
                              Revoke
                            </button>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>

            {/* The cards, under `sm:` — the same rows, not a subset. */}
            <ul className="flex flex-col gap-2 sm:hidden">
              {rows.map((document) => {
                const badge = statusBadge(document.status);
                return (
                  <li
          key={document.id}
          data-graphql-document={document.id}
          data-graphql-status={document.status}
          className="rounded-md border border-line bg-surface p-3"
        >
                    <div className="flex items-start justify-between gap-2">
                      <Link
                        href={`/developer/graphql/documents/${document.id}`}
                        className="text-[13px] font-medium underline-offset-2 hover:underline"
                      >
                        {document.name}
                      </Link>
                      <span
                        className={`shrink-0 rounded-md border px-2 py-0.5 text-[11px] ${badge.className}`}
                      >
                        {badge.label}
                      </span>
                    </div>
                    <p className="mt-1 flex flex-wrap items-center gap-x-3 text-[11.5px] text-muted">
                      <span className="font-mono">{document.short_hash.slice(0, 12)}…</span>
                      <span>{document.kind}</span>
                      <span>{document.hits} hits</span>
                    </p>
                    <div className="mt-2 flex justify-end">
                      {document.status === "revoked" ? (
                        <button
                          type="button"
                          disabled={busy === document.id}
                          onClick={() => void reactivate(document)}
                          className="rounded-md border border-line px-2.5 py-1 text-[11.5px] disabled:opacity-60"
                        >
                          Activate
                        </button>
                      ) : (
                        <button
                          type="button"
                          disabled={busy === document.id}
                          onClick={() => setRevoking(document)}
                          className="rounded-md border border-red-300 px-2.5 py-1 text-[11.5px] text-red-700 disabled:opacity-60 dark:border-red-800 dark:text-red-300"
                        >
                          Revoke
                        </button>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </section>

      {revoking ? (
        <RevokeDialog
          document={revoking}
          busy={busy === revoking.id}
          onCancel={() => setRevoking(null)}
          onConfirm={() => void revoke(revoking)}
        />
      ) : null}
    </div>
  );
}

/**
 * The revoke dialog. It fetches the row's own detail before it asks, because the warning number
 * lives there — `recent_hits` — and the list does not carry it. A dialog that warns with a number it
 * has not fetched would be quoting its own optimism.
 */
function RevokeDialog({
  document,
  busy,
  onCancel,
  onConfirm,
}: {
  document: GraphqlDocument;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [detail, setDetail] = useState<GraphqlDocumentDetail | null>(null);
  const [failed, setFailed] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    fetchGraphqlDocument(document.id)
      .then((result) => {
        if (live) setDetail(result);
      })
      .catch((caught: unknown) => {
        if (live) {
          setFailed(
            caught instanceof ApiError ? caught.message : "The document could not be read.",
          );
        }
      });
    return () => {
      live = false;
    };
  }, [document.id]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="revoke-heading"
      data-graphql-revoke-dialog
    >
      <div className="w-full max-w-md rounded-lg border border-line bg-surface p-5 shadow-xl">
        <h3 id="revoke-heading" className="text-[15px] font-medium">
          Revoke {document.name}?
        </h3>
        <p className="mt-1.5 text-[12.5px] text-muted">
          The document stays in the registry and can be activated again. Until it is, every request
          naming it — by id or by hash — is refused with{" "}
          <code className="font-mono text-[11.5px]">PERSISTED_QUERY_NOT_FOUND</code>.
        </p>

        <p className="mt-3 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-[12px] text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200">
          {failed ? (
            <>The caller count could not be read: {failed}. Revoke anyway, knowing the number is unknown.</>
          ) : detail ? (
            <>
              <span className="font-medium">
                {detail.recent_hits} execution{detail.recent_hits === 1 ? "" : "s"} in the last day
              </span>{" "}
              named this document. Revoking stops those calls with the code above rather than an
              error page.
            </>
          ) : (
            "Reading how many callers used this document…"
          )}
        </p>

        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-line px-3 py-2 text-[13px]"
          >
            Cancel <kbd className="text-[10.5px] text-muted">Esc</kbd>
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={onConfirm}
            className="inline-flex items-center gap-1.5 rounded-md bg-red-600 px-3 py-2 text-[13px] text-white disabled:opacity-60"
          >
            <Ban size={13} aria-hidden />
            {busy ? "Revoking…" : "Revoke the document"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The create form. */
function RegisterForm({
  existing,
  busy,
  onCancel,
  onDone,
}: {
  existing: GraphqlDocument[];
  busy: boolean;
  onCancel: () => void;
  onDone: (created: boolean, duplicateOf: boolean, name: string) => void;
}) {
  const [name, setName] = useState("");
  const [text, setText] = useState("");
  const [required, setRequired] = useState(false);
  const [active, setActive] = useState(true);
  const [formError, setFormError] = useState<string | null>(null);
  const fileRef = useRef<HTMLInputElement | null>(null);

  // The hash is computed on every keystroke, so the duplicate is visible BEFORE the request goes
  // out rather than as a refusal afterwards.
  const localHash = useMemo(() => (text.trim() ? canonicalHash(text) : ""), [text]);
  const duplicate = existing.find((document) => document.short_hash === localHash) ?? null;

  const submit = useCallback(async () => {
    if (!name.trim()) {
      setFormError("A document needs a name: it is what the manager and a revoke dialog will show.");
      return;
    }
    if (!text.trim()) {
      setFormError("Paste the document text, or drop a file in.");
      return;
    }
    setFormError(null);
    try {
      const result = await registerGraphqlDocument({
        name: name.trim(),
        document: text,
        active,
        required_for_callers: required,
      });
      onDone(result.created, result.duplicate_of !== null, result.document.name);
    } catch (caught) {
      setFormError(
        caught instanceof ApiError ? caught.message : "The document could not be registered.",
      );
    }
  }, [name, text, active, required, onDone]);

  return (
    <div className="rounded-md border border-line bg-surface p-4" data-graphql-register-form>
      <h3 className="text-[13px] font-medium">Register a persisted document</h3>
      <p className="mt-1 text-[12px] text-muted">
        A client then runs it by id or hash instead of sending text. The hash is computed here from
        the same normalisation the server uses, so a document that already exists is recognised
        before anything is sent.
      </p>

      <label className="mt-3 block">
        <span className="text-[12px] font-medium">Name</span>
        <input
          value={name}
          onChange={(event) => {
            setName(event.target.value);
            setFormError(null);
          }}
          placeholder="Page list by slug — the CI query"
          className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 text-[13px] outline-none focus:border-accent"
        />
      </label>

      <label className="mt-3 block">
        <span className="text-[12px] font-medium">Document</span>
        <textarea
          value={text}
          onChange={(event) => {
            setText(event.target.value);
            setFormError(null);
          }}
          rows={7}
          spellCheck={false}
          placeholder="query PageBySlug($slug: String!) {&#10;  page(slug: $slug) { id title }&#10;}"
          className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 font-mono text-[12.5px] outline-none focus:border-accent"
        />
      </label>

      <div className="mt-2 flex flex-wrap items-center gap-3">
        <button
          type="button"
          onClick={() => fileRef.current?.click()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
        >
          <FileCode2 size={13} aria-hidden /> Load a file
        </button>
        <input
          ref={fileRef}
          type="file"
          accept=".graphql,.gql,.txt"
          className="sr-only"
          onChange={async (event) => {
            const file = event.target.files?.[0];
            if (!file) return;
            setText(await file.text());
          }}
        />
        <span className="inline-flex items-center gap-1.5 text-[12px] text-muted">
          <Fingerprint size={13} aria-hidden />
          <span className="font-mono">{localHash || "—"}</span>
          <CopyHash value={localHash} label="the computed hash" />
        </span>
      </div>

      {duplicate ? (
        <p className="mt-3 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-[12px] text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200">
          <span className="font-medium">This text is already registered</span> as{" "}
          <Link
            href={`/developer/graphql/documents/${duplicate.id}`}
            className="underline underline-offset-2"
          >
            {duplicate.name}
          </Link>
          . Registering again renames that row — which is what CI does on every release — rather
          than creating a second one.
        </p>
      ) : null}

      <div className="mt-3 flex flex-wrap gap-4">
        <label className="inline-flex items-center gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={active}
            onChange={(event) => setActive(event.target.checked)}
          />
          Active on registration
        </label>
        <label className="inline-flex items-center gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={required}
            onChange={(event) => setRequired(event.target.checked)}
          />
          Mark required for callers
        </label>
      </div>

      {formError ? (
        <p role="alert" className="mt-2 text-[12px] text-red-700 dark:text-red-300">
          {formError}
        </p>
      ) : null}

      <div className="mt-3 flex justify-end gap-2">
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-2 text-[12.5px]"
        >
          Cancel
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => void submit()}
          className="rounded-md bg-accent px-3 py-2 text-[12.5px] text-white disabled:opacity-60"
        >
          {busy ? "Registering…" : duplicate ? "Update the existing row" : "Register"}
        </button>
      </div>
    </div>
  );
}

// -------------------------------------------------------------------------------------------
// The detail
// -------------------------------------------------------------------------------------------

export function DocumentDetailView({ id }: { id: string }) {
  const [detail, setDetail] = useState<GraphqlDocumentDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      setDetail(await fetchGraphqlDocument(id));
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The document could not be loaded.",
      );
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggle = useCallback(async () => {
    if (!detail) return;
    setBusy(true);
    setNotice(null);
    try {
      await setGraphqlDocumentStatus(detail.id, detail.status === "revoked" ? "active" : "revoked");
      setNotice(
        detail.status === "revoked"
          ? "The document executes again."
          : "The document is revoked; the next call naming it is refused with PERSISTED_QUERY_NOT_FOUND.",
      );
      await load();
    } catch (caught) {
      setNotice(caught instanceof ApiError ? caught.message : "The state could not be changed.");
    } finally {
      setBusy(false);
    }
  }, [detail, load]);

  if (loading) return <LoadingTable columns={4} rows={2} />;

  if (error) {
    return (
      <div role="alert" className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
        <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
        <span>
          {error}{" "}
          <Link href="/developer/graphql/documents" className="underline">
            Back to the registry
          </Link>
        </span>
      </div>
    );
  }

  if (!detail) {
    return (
      <EmptyState
        title="The document is gone"
        hint="Another administrator may have removed it while this page was open."
      />
    );
  }

  const badge = statusBadge(detail.status);

  return (
    <div
      className="flex flex-col gap-5"
      data-view="graphql-document-detail"
      data-graphql-detail-status={detail.status}
      data-graphql-detail-hits={detail.hits}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <Link href="/developer/graphql/documents" className="text-[12px] text-muted underline-offset-2 hover:underline">
            ← All documents
          </Link>
          <h2 className="mt-1 text-[16px] font-medium">{detail.name}</h2>
          <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-muted">
            <span className="inline-flex items-center gap-1 font-mono">
              {detail.short_hash.slice(0, 16)}…
              <CopyHash value={detail.short_hash} label="the document hash" />
            </span>
            <span>{detail.kind}</span>
            <span>
              {detail.hits} hit{detail.hits === 1 ? "" : "s"} all time
            </span>
            <span>
              {detail.recent_hits} in the last day
            </span>
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span className={`rounded-md border px-2 py-1 text-[11.5px] ${badge.className}`}>
            {badge.label}
          </span>
          <button
            type="button"
            disabled={busy}
            onClick={() => void toggle()}
            className={`inline-flex items-center gap-1.5 rounded-md border px-3 py-2 text-[12.5px] disabled:opacity-60 ${
              detail.status === "revoked"
                ? "border-line"
                : "border-red-300 text-red-700 dark:border-red-800 dark:text-red-300"
            }`}
          >
            {detail.status === "revoked" ? <Check size={13} aria-hidden /> : <Trash2 size={13} aria-hidden />}
            {busy ? "Working…" : detail.status === "revoked" ? "Activate" : "Revoke"}
          </button>
        </div>
      </div>

      {notice ? (
        <p role="status" className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}

      {detail.blocked_reason ? (
        <p className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2 text-[12.5px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
          <Ban size={14} aria-hidden className="mt-0.5 shrink-0" />
          <span>
            <span className="font-medium">This document does not execute.</span>{" "}
            {detail.blocked_reason}
          </span>
        </p>
      ) : null}

      <section aria-labelledby="operations-heading" className="flex flex-col gap-2">
        <h3 id="operations-heading" className="text-[13px] font-medium">
          Operations and their cost
        </h3>
        {detail.operations.length === 0 ? (
          <p className="text-[12.5px] text-muted">
            The registry recorded no operation summary for this row — the document predates the
            pricing pass, or its text changed without being re-registered.
          </p>
        ) : (
          <ul className="flex flex-col gap-1.5">
            {detail.operations.map((operation, index) => (
              <li
                key={`${operation.name ?? "anonymous"}-${index}`}
                className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
              >
                <span className="font-mono">
                  {operation.kind} {operation.name ?? "(anonymous)"}
                </span>
                <span className="text-muted">depth {operation.depth}</span>
                <span className="text-muted">cost {operation.cost}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section aria-labelledby="text-heading" className="flex flex-col gap-2">
        <h3 id="text-heading" className="text-[13px] font-medium">
          Document text
        </h3>
        <pre className="max-h-[420px] overflow-auto rounded-md border border-line bg-quiet-soft p-3 font-mono text-[12px] leading-relaxed">
          {detail.text}
        </pre>
      </section>

      <p className="text-[12px] text-muted">
        Run it from the{" "}
        <Link href="/developer/graphql" className="underline underline-offset-2">
          playground
        </Link>{" "}
        by id, or call <code className="font-mono">GET /api/v1/graphql?documentId={detail.id.slice(0, 8)}…</code>{" "}
        from a client.
      </p>
    </div>
  );
}