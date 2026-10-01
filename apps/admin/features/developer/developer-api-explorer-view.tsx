"use client";

/**
 * `/developer/api-explorer` — browse the platform's own API and send a call as yourself
 * (REQ-033, slice 2).
 *
 * Four decisions on this screen are the slice, and each is written down because the wrong
 * version is a bug nobody reports:
 *
 * - **A failed call is a result, not a page error.** A `404` or a `403` renders in the response
 *   pane with its status, its body and its latency, exactly as a `200` does. A screen that
 *   replaces itself with an error box teaches the developer nothing about the platform they are
 *   trying to learn, and the most common reason to open an Explorer is to see what a refusal
 *   says.
 * - **The snippets carry `$OMNION_API_KEY`, never a real credential.** A snippet is a thing
 *   that gets pasted into a terminal, a ticket and a screenshot. The request file says so twice
 *   — in the visual check and in the risks — so the placeholder is shaped like the real thing
 *   and the panel states what has to be filled in.
 * - **The operation list is filtered by the caller's own permissions**, resolved server-side
 *   from the same set the route guards use. A role that cannot publish a page does not see a
 *   publish operation to be refused by. The *document* itself is not filtered: hiding a path
 *   from a reference is not a security control, and pretending otherwise is how a platform
 *   teaches people that its documentation is authoritative about what exists.
 * - **`Cmd+/` toggles the snippet drawer and `Cmd+Enter` sends.** Both are in the request's
 *   keyboard list. A developer testing an endpoint is not going to reach for a button, and the
 *   shortcut sheet is not discoverable enough to be the only way.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Check,
  ChevronRight,
  Code2,
  Copy,
  Loader2,
  Play,
  RefreshCw,
  Search,
  Send,
  Terminal,
  X,
} from "lucide-react";

import {
  ApiError,
  fetchExplorerOperations,
  runExplorerRequest,
} from "@/lib/api";
import type {
  ExplorerOperation,
  ExplorerParameter,
  ExplorerResult,
  ExplorerRunInput,
  ExplorerSnippet,
} from "@/lib/types";

/** The verbs the document declares, in the order the method picker shows them. */
const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE"] as const;

/** How long a copy confirmation stays visible before it resets. */
const COPY_FEEDBACK_MS = 1800;

/** The permission the route guard checks for the Explorer itself. */
const RUN_PERMISSION = "developer.explorer.run";

/** The tone of a status, as a class name. The number is always beside it, never colour alone. */
function statusTone(status: number): string {
  if (status >= 500) return "text-accent-strong";
  if (status >= 400) return "text-caution";
  if (status >= 200 && status < 300) return "text-positive";
  return "text-muted";
}

/**
 * The value a freshly-built field starts at.
 *
 * An empty string rather than a placeholder-looking zero, because the form is filled from a
 * schema and a `0` in a `uuid` field is a value somebody has to delete before typing. An empty
 * field is visibly empty; a `0` looks like an answer.
 */
function initialValue(parameter: ExplorerParameter): string {
  if (parameter.enum && parameter.enum.length > 0) {
    return parameter.enum[0];
  }
  return "";
}

/**
 * The form state for one operation.
 *
 * Keyed by parameter name rather than position, so picking a different operation and coming
 * back does not silently send the previous operation's id in the new one's slot.
 */
type Draft = {
  path: string[];
  query: Record<string, string>;
  body: string;
};

function draftFor(operation: ExplorerOperation): Draft {
  const path: string[] = [];
  const query: Record<string, string> = {};
  for (const parameter of operation.parameters) {
    if (parameter.in === "path") {
      path.push(initialValue(parameter));
    } else {
      query[parameter.name] = initialValue(parameter);
    }
  }
  // A body form is generated from the schema, so the first field starts empty and the rest are
  // filled by the person. An operation with no body starts with an empty editor rather than
  // `{}`, because a `{}` that gets sent is a `400` a reader will blame on the API.
  return { path, query, body: "" };
}

/**
 * The JSON body the schema form produces.
 *
 * A body with no property filled in is `null` rather than `{}`: sending `{}` to `POST /pages`
 * is a `400` for a missing title, and a form that sends nothing is more honest about not
 * knowing than a form that sends an empty object.
 */
function bodyFrom(operation: ExplorerOperation, values: Record<string, string>): string | undefined {
  const schema = operation.body;
  if (!schema || !schema.properties) {
    return undefined;
  }
  const names = Object.keys(schema.properties);
  const filled = names.filter((name) => (values[name] ?? "").trim() !== "");
  if (filled.length === 0) {
    return undefined;
  }
  const body: Record<string, unknown> = {};
  for (const name of filled) {
    const property = schema.properties[name];
    const raw = values[name].trim();
    body[name] = property?.type === "integer" || property?.type === "number" ? Number(raw) : raw;
  }
  return JSON.stringify(body, null, 2);
}

export function DeveloperApiExplorerView() {
  const [operations, setOperations] = useState<ExplorerOperation[] | null>(null);
  // Whether this caller may send at all. The server decides; see the note on `can_send` in
  // `ExplorerOperations`. The first derivation of it here compared the *selected operation's*
  // permission against the run key, which is a different question and answered "no" for every
  // operation — including the ones an owner can send, so the button was dead for everyone.
  const [canSend, setCanSend] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [bodyValues, setBodyValues] = useState<Record<string, string>>({});

  const [sending, setSending] = useState(false);
  const [result, setResult] = useState<ExplorerResult | null>(null);
  const [snippets, setSnippets] = useState<ExplorerSnippet[]>([]);
  const [sendError, setSendError] = useState<string | null>(null);
  const [showSnippets, setShowSnippets] = useState(false);
  const [copied, setCopied] = useState<string | null>(null);
  const [shortcutSheet, setShortcutSheet] = useState(false);

  const copyTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = useCallback(() => {
    setLoadError(null);
    fetchExplorerOperations()
      .then((response) => {
        setOperations(response.operations);
        setCanSend(response.can_send);
        // Land on something rather than an empty right-hand pane. The first operation in the
        // document is a read, so the screen's first paint is a form somebody can send.
        setSelectedId((current) => current ?? response.operations[0]?.id ?? null);
      })
      .catch((cause: unknown) => {
        setOperations([]);
        setLoadError(
          cause instanceof ApiError
            ? cause.message
            : "The API reference could not be loaded.",
        );
      });
  }, []);

  useEffect(() => {
    load();
  }, [load, reloadToken]);

  const selected = useMemo(
    () => operations?.find((operation) => operation.id === selectedId) ?? null,
    [operations, selectedId],
  );

  // Rebuild the form when the selected operation changes, and clear the previous answer: a
  // `200` from the operation picked five clicks ago sitting under a different operation's form
  // is the kind of thing a person trusts for thirty seconds.
  useEffect(() => {
    if (!selected) {
      setDraft(null);
      return;
    }
    setDraft(draftFor(selected));
    setBodyValues({});
    setResult(null);
    setSnippets([]);
    setSendError(null);
  }, [selected]);

  const tags = useMemo(() => {
    const seen: string[] = [];
    for (const operation of operations ?? []) {
      if (!seen.includes(operation.tag)) {
        seen.push(operation.tag);
      }
    }
    return seen;
  }, [operations]);

  const visible = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (needle.length === 0) {
      return operations ?? [];
    }
    return (operations ?? []).filter(
      (operation) =>
        operation.path.toLowerCase().includes(needle) ||
        operation.summary.toLowerCase().includes(needle) ||
        operation.method.toLowerCase().includes(needle),
    );
  }, [operations, search]);

  // A send is possible when the server says this caller may send. The permission line under
  // the operation title is *information* about the call, not a gate on the button — a reader
  // may browse operations their role cannot act on, and hiding the browser from them would be
  // hiding the answer to "why can I not".
  const runnable = canSend;

  const send = useCallback(async () => {
    if (!selected || !draft || sending) {
      return;
    }
    setSending(true);
    setSendError(null);
    const input: ExplorerRunInput = {
      method: selected.method,
      path: selected.path,
      path_params: draft.path,
      query: Object.entries(draft.query)
        .filter(([, value]) => value.trim() !== "")
        .map(([name, value]) => `${name}=${value.trim()}`),
      body: bodyFrom(selected, bodyValues),
    };
    try {
      const answer = await runExplorerRequest(input);
      setResult(answer.result);
      setSnippets(answer.snippets);
    } catch (cause: unknown) {
      // A refusal from the Explorer *endpoint itself* is the one thing that is a page error:
      // it means the call was never sent. A refusal from the call being sent is a result.
      setResult(null);
      setSnippets([]);
      setSendError(
        cause instanceof ApiError ? cause.message : "The call could not be sent.",
      );
    } finally {
      setSending(false);
    }
  }, [selected, draft, bodyValues, sending]);

  // `Cmd/Ctrl + Enter` sends; `Cmd/Ctrl + /` toggles the snippet drawer. Both are bound on the
  // document rather than on the form so they work from a parameter field without a click into
  // a particular element first.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const meta = event.metaKey || event.ctrlKey;
      if (!meta) {
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        void send();
      } else if (event.key === "/") {
        event.preventDefault();
        setShowSnippets((open) => !open);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [send]);

  const copy = async (label: string, text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(label);
      if (copyTimer.current) {
        clearTimeout(copyTimer.current);
      }
      copyTimer.current = setTimeout(() => setCopied(null), COPY_FEEDBACK_MS);
    } catch {
      // A clipboard the browser refuses is a real state, and the snippet is on screen either
      // way — so this says so rather than showing a confirmation that did not happen.
      setCopied(null);
      setSendError("The clipboard is not available in this browser. Select the text and copy it.");
    }
  };

  if (operations === null) {
    return loadError ? (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{loadError}</p>
        <button
          type="button"
          onClick={() => setReloadToken((token) => token + 1)}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
        >
          Try again
        </button>
      </div>
    ) : (
      <div className="flex items-center gap-2 px-1 py-8 text-[12.5px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Loading the API reference…
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {/* What this screen is, before the person sends anything. */}
      <div className="flex flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted">
        <p>
          Every call below is sent <strong className="text-ink">as you</strong> — with your
          session, your roles and your permissions. A call you could not make from this panel is
          refused here with the same answer, and the refusal names the permission it needs.
        </p>
        <p>
          The copyable snippets read the key from <code className="text-ink">$OMNION_API_KEY</code>.
          Nothing on this screen ever writes a real credential into a snippet, a log or a
          screenshot.
        </p>
        <button
          type="button"
          onClick={() => setShortcutSheet((open) => !open)}
          className="mt-1 self-start text-[12px] underline decoration-dotted underline-offset-2"
        >
          {shortcutSheet ? "Hide shortcuts" : "Keyboard shortcuts"}
        </button>
        {shortcutSheet ? (
          <ul className="mt-1 list-disc pl-5 text-[12px]">
            <li>
              <kbd className="rounded border border-line px-1">Cmd/Ctrl</kbd> +{" "}
              <kbd className="rounded border border-line px-1">Enter</kbd> — send
            </li>
            <li>
              <kbd className="rounded border border-line px-1">Cmd/Ctrl</kbd> +{" "}
              <kbd className="rounded border border-line px-1">/</kbd> — the snippet drawer
            </li>
            <li>
              <kbd className="rounded border border-line px-1">Esc</kbd> — clear the answer
            </li>
          </ul>
        ) : null}
      </div>

      <div className="grid gap-4 lg:grid-cols-[minmax(0,320px)_minmax(0,1fr)]">
        {/* The browser. On mobile it becomes a select — see the `lg:` breakpoint above. */}
        <div className="hidden min-w-0 flex-col gap-3 rounded-xl border border-line bg-surface p-3 lg:flex">
          <div className="flex items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center gap-2 rounded-lg border border-line px-2.5 py-1.5">
              <Search className="size-3.5 shrink-0 text-muted" aria-hidden />
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Filter operations"
                aria-label="Filter operations by path, method or summary"
                className="min-w-0 flex-1 bg-transparent text-[12.5px] outline-none"
              />
            </div>
            <button
              type="button"
              onClick={() => setReloadToken((token) => token + 1)}
              className="rounded-lg border border-line p-1.5"
              aria-label="Reload the reference"
              title="Reload the reference"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
          </div>

          {visible.length === 0 ? (
            <p className="px-1 py-6 text-center text-[12.5px] text-muted">
              No operation matches “{search}”.
            </p>
          ) : (
            <div className="flex max-h-[60vh] min-w-0 flex-col gap-3 overflow-y-auto">
              {tags.map((tag) => {
                const group = visible.filter((operation) => operation.tag === tag);
                if (group.length === 0) {
                  return null;
                }
                return (
                  <div key={tag} className="flex flex-col gap-1">
                    <p className="px-1 text-[11px] uppercase tracking-wide text-muted">{tag}</p>
                    {group.map((operation) => (
                      <button
                        key={operation.id}
                        type="button"
                        onClick={() => setSelectedId(operation.id)}
                        data-testid="explorer-operation"
                        aria-current={operation.id === selectedId}
                        className={`flex min-w-0 items-center gap-2 rounded-lg px-2 py-1.5 text-left text-[12.5px] ${
                          operation.id === selectedId
                            ? "bg-accent-soft text-ink"
                            : "text-muted hover:bg-surface-muted"
                        }`}
                      >
                        <span className="w-14 shrink-0 font-mono text-[11px] text-ink">
                          {operation.method}
                        </span>
                        <span className="min-w-0 flex-1 truncate">{operation.path.replace("/api/v1", "")}</span>
                        {operation.id === selectedId ? (
                          <ChevronRight className="size-3 shrink-0" aria-hidden />
                        ) : null}
                      </button>
                    ))}
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* The mobile picker. A 320px column of paths is unusable on a phone, so below `lg` the
            browser is a select and the rest of the screen keeps its shape. */}
        <div className="flex flex-col gap-2 lg:hidden">
          <label htmlFor="explorer-operation-mobile" className="text-[12px] text-muted">
            Operation
          </label>
          <select
            id="explorer-operation-mobile"
            value={selectedId ?? ""}
            onChange={(event) => setSelectedId(event.target.value)}
            className="min-h-11 rounded-lg border border-line bg-surface px-3 text-[12.5px]"
          >
            {visible.map((operation) => (
              <option key={operation.id} value={operation.id}>
                {operation.method} {operation.path.replace("/api/v1", "")}
              </option>
            ))}
          </select>
        </div>

        <div className="flex min-w-0 flex-col gap-3">
          {!selected ? (
            <div className="rounded-xl border border-line bg-surface px-4 py-10 text-center text-[12.5px] text-muted">
              {loadError ?? "No operation is available to you."}
            </div>
          ) : (
            <>
              <div className="flex min-w-0 flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3">
                <div className="flex min-w-0 flex-wrap items-center gap-2">
                  <span className="rounded-md bg-surface-muted px-2 py-0.5 font-mono text-[11px] text-ink">
                    {selected.method}
                  </span>
                  <code className="min-w-0 break-all font-mono text-[12.5px] text-ink">
                    {selected.path}
                  </code>
                </div>
                <p className="text-[12.5px] text-muted">{selected.summary}</p>
                {selected.permission ? (
                  <p className="text-[12px] text-muted">
                    Needs <code className="text-ink">{selected.permission}</code>.
                  </p>
                ) : (
                  <p className="text-[12px] text-muted">Open to any signed-in account.</p>
                )}
              </div>

              {draft ? (
                <div className="flex min-w-0 flex-col gap-3 rounded-xl border border-line bg-surface px-4 py-3">
                  {selected.parameters.length > 0 ? (
                    <div className="grid gap-3 sm:grid-cols-2">
                      {selected.parameters.map((parameter, index) => {
                        const isPath = parameter.in === "path";
                        const inputId = `explorer-${parameter.in}-${parameter.name}`;
                        return (
                          <div key={inputId} className="flex min-w-0 flex-col gap-1">
                            <label htmlFor={inputId} className="flex items-center gap-1.5 text-[12px] text-muted">
                              {parameter.name}
                              {parameter.in === "path" ? (
                                <span className="rounded bg-surface-muted px-1 text-[10px]">path</span>
                              ) : (
                                <span className="rounded bg-surface-muted px-1 text-[10px]">query</span>
                              )}
                              {parameter.required ? (
                                <span className="text-accent-strong">required</span>
                              ) : null}
                            </label>
                            {parameter.enum && parameter.enum.length > 0 ? (
                              <select
                                id={inputId}
                                value={
                                  isPath
                                    ? (draft.path[index] ?? "")
                                    : (draft.query[parameter.name] ?? "")
                                }
                                onChange={(event) => {
                                  const value = event.target.value;
                                  setDraft((current) => {
                                    if (!current) {
                                      return current;
                                    }
                                    if (isPath) {
                                      const path = [...current.path];
                                      path[index] = value;
                                      return { ...current, path };
                                    }
                                    return {
                                      ...current,
                                      query: { ...current.query, [parameter.name]: value },
                                    };
                                  });
                                }}
                                className="min-h-11 rounded-lg border border-line bg-surface px-2.5 text-[12.5px]"
                              >
                                {parameter.enum.map((value) => (
                                  <option key={value} value={value}>
                                    {value}
                                  </option>
                                ))}
                              </select>
                            ) : (
                              <input
                                id={inputId}
                                value={
                                  isPath
                                    ? (draft.path[index] ?? "")
                                    : (draft.query[parameter.name] ?? "")
                                }
                                onChange={(event) => {
                                  const value = event.target.value;
                                  setDraft((current) => {
                                    if (!current) {
                                      return current;
                                    }
                                    if (isPath) {
                                      const path = [...current.path];
                                      path[index] = value;
                                      return { ...current, path };
                                    }
                                    return {
                                      ...current,
                                      query: { ...current.query, [parameter.name]: value },
                                    };
                                  });
                                }}
                                placeholder={
                                  parameter.format === "uuid"
                                    ? "00000000-0000-0000-0000-000000000000"
                                    : parameter.name
                                }
                                className="min-h-11 rounded-lg border border-line bg-surface px-2.5 font-mono text-[12.5px]"
                              />
                            )}
                            {parameter.description ? (
                              <p className="text-[11.5px] text-muted">{parameter.description}</p>
                            ) : null}
                          </div>
                        );
                      })}
                    </div>
                  ) : null}

                  {selected.body && selected.body.properties ? (
                    <div className="flex flex-col gap-2">
                      <p className="text-[12px] text-muted">
                        Body — filled into a JSON object as you type. Nothing is sent until you
                        press Send.
                      </p>
                      <div className="grid gap-3 sm:grid-cols-2">
                        {Object.keys(selected.body.properties).map((name) => {
                          const inputId = `explorer-body-${name}`;
                          const required = selected.body?.required?.includes(name) ?? false;
                          return (
                            <div key={inputId} className="flex min-w-0 flex-col gap-1">
                              <label htmlFor={inputId} className="flex items-center gap-1.5 text-[12px] text-muted">
                                {name}
                                {required ? <span className="text-accent-strong">required</span> : null}
                              </label>
                              <input
                                id={inputId}
                                value={bodyValues[name] ?? ""}
                                onChange={(event) =>
                                  setBodyValues((current) => ({
                                    ...current,
                                    [name]: event.target.value,
                                  }))
                                }
                                className="min-h-11 rounded-lg border border-line bg-surface px-2.5 text-[12.5px]"
                              />
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  ) : null}

                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      onClick={() => void send()}
                      disabled={sending || !runnable}
                      data-testid="explorer-send"
                      className="flex min-h-11 items-center gap-2 rounded-lg bg-accent px-3.5 text-[12.5px] text-accent-ink disabled:opacity-50"
                    >
                      {sending ? (
                        <Loader2 className="size-3.5 animate-spin" aria-hidden />
                      ) : (
                        <Send className="size-3.5" aria-hidden />
                      )}
                      {sending ? "Sending…" : "Send"}
                    </button>
                    <button
                      type="button"
                      onClick={() => setShowSnippets((open) => !open)}
                      disabled={snippets.length === 0}
                      className="flex min-h-11 items-center gap-2 rounded-lg border border-line px-3 text-[12.5px] disabled:opacity-50"
                    >
                      <Terminal className="size-3.5" aria-hidden />
                      Snippets
                    </button>
                    <span className="text-[11.5px] text-muted">Cmd/Ctrl + Enter</span>
                  </div>

                  {!runnable ? (
                    <p className="text-[12px] text-caution">
                      Your role does not hold <code>{RUN_PERMISSION}</code>, so calls cannot be
                      sent from this screen. The reference above is still yours to read —
                      browsing the API and acting as the person at the screen are two different
                      permissions, and a read-only developer role has the first.
                    </p>
                  ) : null}

                  {sendError ? (
                    <p className="text-[12px] text-accent-strong" role="alert">
                      {sendError}
                    </p>
                  ) : null}
                </div>
              ) : null}

              {/* The answer. A refusal renders here, in place, with its status and its body. */}
              {result ? (
                <div
                  className="flex min-w-0 flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3"
                  data-testid="explorer-result"
                >
                  <div className="flex min-w-0 flex-wrap items-center gap-3">
                    <span
                      className={`font-mono text-[13px] ${statusTone(result.status)}`}
                      data-testid="explorer-status"
                    >
                      {result.status}
                    </span>
                    <span className="text-[12px] text-muted">
                      {result.duration_ms} ms
                    </span>
                    {result.request_id ? (
                      <span className="truncate font-mono text-[11.5px] text-muted">
                        {result.request_id}
                      </span>
                    ) : null}
                    <code className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-muted">
                      {result.sent_path}
                    </code>
                    <button
                      type="button"
                      onClick={() => void copy("body", result.body)}
                      className="flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-2 text-[12px]"
                    >
                      {copied === "body" ? (
                        <Check className="size-3.5" aria-hidden />
                      ) : (
                        <Copy className="size-3.5" aria-hidden />
                      )}
                      Copy body
                    </button>
                  </div>
                  {result.refused ? (
                    <p className="text-[12px] text-caution">
                      This call was refused by a platform layer, not answered by the endpoint.
                      The body below is the refusal itself.
                    </p>
                  ) : null}
                  <pre className="max-h-96 min-w-0 overflow-auto rounded-lg bg-surface-muted p-3 text-[12px] leading-relaxed">
                    {result.body}
                  </pre>
                </div>
              ) : null}

              {showSnippets && snippets.length > 0 ? (
                <div className="flex min-w-0 flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3">
                  <div className="flex items-center justify-between">
                    <p className="flex items-center gap-2 text-[12.5px] text-ink">
                      <Code2 className="size-3.5" aria-hidden />
                      The same call outside the browser
                    </p>
                    <button
                      type="button"
                      onClick={() => setShowSnippets(false)}
                      aria-label="Close the snippet drawer"
                      className="rounded-lg border border-line p-1.5"
                    >
                      <X className="size-3.5" aria-hidden />
                    </button>
                  </div>
                  <p className="text-[12px] text-muted">
                    Fill <code className="text-ink">$OMNION_API_KEY</code> with one of your keys
                    first. Nothing here contains a credential.
                  </p>
                  {snippets.map((snippet) => (
                    <div key={snippet.language} className="flex min-w-0 flex-col gap-1">
                      <div className="flex items-center justify-between">
                        <span className="text-[11.5px] uppercase tracking-wide text-muted">
                          {snippet.language}
                        </span>
                        <button
                          type="button"
                          onClick={() => void copy(snippet.language, snippet.code)}
                          className="flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-2 text-[12px]"
                        >
                          {copied === snippet.language ? (
                            <Check className="size-3.5" aria-hidden />
                          ) : (
                            <Copy className="size-3.5" aria-hidden />
                          )}
                          Copy
                        </button>
                      </div>
                      {/* Horizontal scroll, never wrapping mid-token: a wrapped `Bearer` header
                          is a snippet a person pastes and it does not run. */}
                      <pre className="min-w-0 overflow-x-auto rounded-lg bg-surface-muted p-3 text-[12px] leading-relaxed">
                        {snippet.code}
                      </pre>
                    </div>
                  ))}
                </div>
              ) : null}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
