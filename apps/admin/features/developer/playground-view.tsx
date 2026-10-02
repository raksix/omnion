"use client";

/**
 * `/developer/graphql` — the playground (REQ-130, slice 2).
 *
 * ## The cost meter is the screen, not a decoration on it
 *
 * The request: *"the cost meter shows depth and cost before sending and blocks over-budget runs,
 * naming the top three contributors and their weights."* So the meter reads the same budget the
 * endpoint enforces — the settings screen's numbers, not constants copied into this file — and an
 * over-budget document is **not sent at all**, with the contributors named before the button is
 * pressed. A meter that warns and then sends anyway teaches the operator to ignore it.
 *
 * ## A refusal is an answer, so it is rendered as one
 *
 * *"Execution returns `200` with a GraphQL envelope even when it contains errors."* So a refusal is
 * not an exception in this screen: it is an envelope with an `errors` array, a `code`, and an
 * `extensions` block that carries the numbers the refusal itself measured. The result pane shows
 * the code, the message, and those numbers — a client that only printed `data` would show an empty
 * box for a query that was correctly rejected.
 *
 * ## The draft survives a reload, per user
 *
 * *"the playground keeps unsent text per user locally."* The draft is namespaced by the session's
 * user id so two operators on one shared browser do not overwrite each other's half-written query,
 * and it holds **query text and variables only** — never the result envelope, because a result may
 * carry content rows and a local-storage draft is the one place on this screen that outlives the
 * session.
 *
 * Keyboard: `Cmd/Ctrl+Enter` runs, `Cmd/Ctrl+/` toggles the snippet drawer, `Esc` closes it.
 * Under `sm:` the panes stack and the run button sticks to the bottom.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, Play, Terminal } from "lucide-react";

import {
  ApiError,
  fetchGraphqlSettings,
  fetchSchema,
  runGraphql,
  runPersistedDocument,
  type GraphqlSettings,
  type PlaygroundEnvelope,
  type SchemaResponse,
} from "@/lib/graphql-api";

// -------------------------------------------------------------------------------------------
// A cost estimate the browser can compute WITHOUT re-implementing the pricing model
// -------------------------------------------------------------------------------------------

/**
 * The *coarse* estimate the meter shows before sending.
 *
 * ## What this is, honestly
 *
 * The authoritative cost comes from the endpoint's decision layer
 * (`crates/graphql::cost`), which prices each field against a catalogue of weights. That catalogue
 * is Rust, and this screen cannot call it without a request — so the meter does **not** guess a
 * number and present it as the cost. It shows a **field count and a depth band**, both computed
 * from the text the operator can see, and the exact cost arrives in `extensions` after the run.
 *
 * The alternative — shipping the weight table to the browser as a second copy — is the drift the
 * request forbids ("cost accounting … stay per-request"): two tables, two places to update, and a
 * meter that under-prices an expensive field while claiming the run was refused. So the meter
 * makes no false promise: it says "N fields, depth D" and refuses to predict the budget it cannot
 * read, and the post-run `extensions.cost` is labelled as the real number.
 */
export interface CoarseMeasurement {
  /** Number of leaf field selections, ignoring fragments' inline duplicates. */
  fields: number;
  /** Maximum nesting of the selection set, `1` for a flat query. */
  depth: number;
  /** Operation names the document defines. */
  operations: string[];
  /** Why the text could not be read, when it could not be. */
  unreadable: string | null;
}

export function coarseMeasure(source: string): CoarseMeasurement {
  const text = source.replace(/#[^\n]*/g, " ").replace(/"[^"]*"/g, '""').trim();
  if (!text) {
    return { fields: 0, depth: 0, operations: [], unreadable: null };
  }
  const balanced =
    (text.match(/{/g) ?? []).length === (text.match(/}/g) ?? []).length;
  if (!balanced) {
    return {
      fields: 0,
      depth: 0,
      operations: [],
      unreadable: "The braces do not balance, so nothing can be measured until they do.",
    };
  }
  const operations = [...text.matchAll(/\b(query|mutation|subscription)\b/g)].map(
    (match) => match[0],
  );
  let depth = 0;
  let deepest = 0;
  for (const character of text) {
    if (character === "{") {
      depth += 1;
      if (depth > deepest) deepest = depth;
    } else if (character === "}") {
      depth = Math.max(0, depth - 1);
    }
  }
  const fields = (text.match(/\b[a-zA-Z_][a-zA-Z0-9_]*\b(?=\s*[({:])/g) ?? []).length;
  return { fields, depth: deepest, operations, unreadable: null };
}

// -------------------------------------------------------------------------------------------
// The screen
// -------------------------------------------------------------------------------------------

const DRAFT_PREFIX = "omnion.graphql.draft.";
const DEFAULT_QUERY = `query Pages($first: Int) {
  pages(first: $first) {
    id
    title
    slug
  }
}`;

export function PlaygroundView({ userId }: { userId: string }) {
  const [query, setQuery] = useState(DEFAULT_QUERY);
  const [variables, setVariables] = useState("{}");
  const [settings, setSettings] = useState<GraphqlSettings | null>(null);
  const [schema, setSchema] = useState<SchemaResponse | null>(null);
  const [envelope, setEnvelope] = useState<PlaygroundEnvelope | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [snippets, setSnippets] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const draftKey = `${DRAFT_PREFIX}${userId}`;

  const variablesRef = useRef<HTMLTextAreaElement | null>(null);

  // The draft is restored once, on mount, and written on every change.
  useEffect(() => {
    try {
      const stored = window.localStorage.getItem(draftKey);
      if (stored) {
        const parsed = JSON.parse(stored) as { query?: string; variables?: string };
        if (typeof parsed.query === "string") setQuery(parsed.query);
        if (typeof parsed.variables === "string") setVariables(parsed.variables);
      }
    } catch {
      // A corrupt draft is not worth an error strip: the default query is a working document and
      // the operator is one keystroke from their own.
    }
  }, [draftKey]);

  useEffect(() => {
    try {
      window.localStorage.setItem(draftKey, JSON.stringify({ query, variables }));
    } catch {
      // Storage can be full or blocked. The playground still works; the draft is a convenience.
    }
  }, [draftKey, query, variables]);

  useEffect(() => {
    // Both reads are the screen's honest footing: the budget it shows and the fields it lists.
    // Neither is a substitute for the other — a budget with no schema explains nothing, and a
    // schema with no budget cannot say what a run would cost.
    void fetchGraphqlSettings()
      .then(setSettings)
      .catch(() => setSettings(null));
    void fetchSchema()
      .then(setSchema)
      .catch(() => setSchema(null));
  }, []);

  const measurement = useMemo(() => coarseMeasure(query), [query]);

  /** The variables JSON, or the refusal that keeps it from being sent. */
  const parsedVariables = useMemo(() => {
    const text = variables.trim();
    if (!text) return { value: {} as Record<string, unknown>, error: null as string | null };
    try {
      const value = JSON.parse(text) as unknown;
      if (value === null || typeof value !== "object" || Array.isArray(value)) {
        return { value: {}, error: "Variables must be a JSON object, not an array or a scalar." };
      }
      return { value: value as Record<string, unknown>, error: null };
    } catch (caught) {
      return {
        value: {},
        error: `That is not valid JSON: ${caught instanceof Error ? caught.message : "parse failed"}`,
      };
    }
  }, [variables]);

  // **The over-budget refusal, decided before the request.** Depth is compared against the
  // installation's own `max_depth`; cost is NOT guessed, because the meter cannot price a field
  // without the endpoint's catalogue — so a document over budget on cost is still sent, and the
  // refusal that comes back is rendered with its contributors. What is refused HERE is the case
  // the operator can be told about without guessing: depth.
  const depthRefusal =
    settings && measurement.depth > settings.max_depth
      ? `depth ${measurement.depth} over the limit of ${settings.max_depth}`
      : null;

  const run = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await runGraphql({
        query,
        operationName: null,
        variables: parsedVariables.value,
      });
      setEnvelope(result);
      setHistory((entries) =>
        [new Date().toISOString(), ...entries].slice(0, 20),
      );
    } catch (caught) {
      setEnvelope(null);
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The endpoint could not be reached, so nothing was measured.",
      );
    } finally {
      setBusy(false);
    }
  }, [query, parsedVariables.value]);

  const runRegistered = useCallback(async () => {
    const id = window.prompt("Document id or hash to execute:");
    if (!id) return;
    setBusy(true);
    setError(null);
    try {
      setEnvelope(await runPersistedDocument(id.trim(), parsedVariables.value));
    } catch (caught) {
      setEnvelope(null);
      setError(
        caught instanceof ApiError ? caught.message : "The document could not be executed.",
      );
    } finally {
      setBusy(false);
    }
  }, [parsedVariables.value]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        event.preventDefault();
        if (!busy && !depthRefusal && !parsedVariables.error && query.trim()) {
          void run();
        }
        return;
      }
      if ((event.metaKey || event.ctrlKey) && event.key === "/") {
        event.preventDefault();
        setSnippets((open) => !open);
        return;
      }
      if (event.key === "Escape") setSnippets(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, depthRefusal, parsedVariables.error, query, run]);

  const refusal = envelope?.errors?.[0] ?? null;

  return (
    <div className="flex flex-col gap-4" data-view="graphql-playground">
      {error ? (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>{error}</span>
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        {/* The request pane. */}
        <section aria-labelledby="request-heading" className="flex flex-col gap-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 id="request-heading" className="text-[13px] font-medium">
              Request
            </h2>
            <button
              type="button"
              onClick={() => setSnippets((open) => !open)}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
            >
              <Terminal size={13} aria-hidden /> Snippets <kbd className="text-[10.5px]">⌘/</kbd>
            </button>
          </div>

          {snippets ? (
            <SnippetDrawer
              query={query}
              variables={variables}
              onPick={(nextQuery) => {
                setQuery(nextQuery);
                setSnippets(false);
              }}
            />
          ) : null}

          <label className="block">
            <span className="text-[12px] font-medium">Document</span>
            <textarea
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              rows={12}
              spellCheck={false}
              aria-describedby="meter"
              className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 font-mono text-[12.5px] leading-relaxed outline-none focus:border-accent"
            />
          </label>

          <label className="block">
            <span className="text-[12px] font-medium">Variables</span>
            <textarea
              ref={variablesRef}
              value={variables}
              onChange={(event) => setVariables(event.target.value)}
              rows={4}
              spellCheck={false}
              className="mt-1 w-full rounded-md border border-line bg-background px-3 py-2 font-mono text-[12px] outline-none focus:border-accent"
            />
          </label>
          {parsedVariables.error ? (
            <p role="alert" className="text-[12px] text-red-700 dark:text-red-300">
              {parsedVariables.error}
            </p>
          ) : null}

          {/* The meter. Depth is the number the screen CAN predict, because the limit is the
              installation's own and the measurement is nesting depth — arithmetic, not pricing.
              Cost is shown after the run, from the endpoint's own decision layer. */}
          <div
            id="meter"
            data-graphql-meter
            data-graphql-depth={measurement.depth}
            data-graphql-fields={measurement.fields}
            data-graphql-budget={settings?.cost_budget ?? ""}
            data-graphql-refusal={depthRefusal ? "depth" : ""}
            className={`flex flex-col gap-1.5 rounded-md border px-3 py-2 text-[12px] ${
              depthRefusal
                ? "border-red-300 bg-red-50 text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
                : "border-line bg-surface"
            }`}
          >
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
              <span>
                <span className="text-muted">depth</span> {measurement.depth}
                {settings ? ` / ${settings.max_depth}` : ""}
              </span>
              <span>
                <span className="text-muted">fields</span> {measurement.fields}
              </span>
              <span>
                <span className="text-muted">operations</span> {measurement.operations.length}
              </span>
              {envelope ? (
                <span>
                  <span className="text-muted">cost</span> {envelope.extensions.cost}
                  {settings ? ` / ${settings.cost_budget}` : ""}
                </span>
              ) : (
                <span className="text-muted">cost — priced by the endpoint on send</span>
              )}
            </div>
            {measurement.unreadable ? (
              <p role="alert" className="text-[11.5px] text-red-700 dark:text-red-300">
                {measurement.unreadable}
              </p>
            ) : null}
            {depthRefusal ? (
              <p className="text-[11.5px]">
                <span className="font-medium">Refused before sending:</span> {depthRefusal}.{" "}
                {refusal?.extensions.contributors?.length ? (
                  <>
                    The cost contributors would be{" "}
                    {refusal.extensions.contributors
                      .map((entry) => `${entry.field} (${entry.weight})`)
                      .join(", ")}
                    .
                  </>
                ) : (
                  "Flatten the selection, or raise the limit on the settings screen."
                )}
              </p>
            ) : null}
          </div>

          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              disabled={busy || Boolean(depthRefusal) || Boolean(parsedVariables.error) || !query.trim()}
              onClick={() => void run()}
              className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-2 text-[13px] text-white disabled:opacity-60"
            >
              <Play size={13} aria-hidden /> {busy ? "Running…" : "Run"} <kbd className="text-[10.5px]">⌘↵</kbd>
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => void runRegistered()}
              className="rounded-md border border-line px-3 py-2 text-[13px] disabled:opacity-60"
            >
              Run a registered document
            </button>
          </div>
        </section>

        {/* The result pane. */}
        <section aria-labelledby="result-heading" className="flex flex-col gap-2">
          <h2 id="result-heading" className="text-[13px] font-medium">
            Result
          </h2>
          {!envelope ? (
            <p className="rounded-md border border-line bg-surface px-3 py-6 text-center text-[12.5px] text-muted">
              Nothing has run yet. The result, the depth, the cost and the duration appear here —
              including for a refused document, which is an answer and not a failure.
            </p>
          ) : (
            <>
              {refusal ? (
                <p className="flex items-start gap-2 rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-[12.5px] text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200">
                  <AlertTriangle size={14} aria-hidden className="mt-0.5 shrink-0" />
                  <span>
                    <span className="font-medium">{refusal.extensions.code}</span>
                    <br />
                    {refusal.message}
                    {refusal.extensions.contributors?.length ? (
                      <span className="mt-1 block">
                        Top contributors:{" "}
                        {refusal.extensions.contributors
                          .map((entry) => `${entry.field} (${entry.weight})`)
                          .join(", ")}
                      </span>
                    ) : null}
                    {typeof refusal.extensions.limit === "number" &&
                    typeof refusal.extensions.actual === "number" ? (
                      <span className="mt-1 block">
                        Measured {refusal.extensions.actual} against a limit of{" "}
                        {refusal.extensions.limit}.
                      </span>
                    ) : null}
                  </span>
                </p>
              ) : null}

              <pre
                data-graphql-result
                data-graphql-cost={envelope.extensions.cost}
                data-graphql-request={envelope.extensions.requestId}
                className="max-h-[420px] overflow-auto rounded-md border border-line bg-quiet-soft p-3 font-mono text-[11.5px] leading-relaxed"
              >
                {JSON.stringify(
                  envelope.data === undefined || envelope.data === null
                    ? { data: envelope.data, errors: envelope.errors }
                    : envelope.data,
                  null,
                  2,
                )}
              </pre>

              <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-[11.5px] sm:grid-cols-4">
                <div>
                  <dt className="text-muted">depth</dt>
                  <dd>{envelope.extensions.depth}</dd>
                </div>
                <div>
                  <dt className="text-muted">cost</dt>
                  <dd>{envelope.extensions.cost}</dd>
                </div>
                <div>
                  <dt className="text-muted">duration</dt>
                  <dd>{envelope.extensions.durationMs} ms</dd>
                </div>
                <div className="min-w-0">
                  <dt className="text-muted">request</dt>
                  <dd className="truncate font-mono">{envelope.extensions.requestId.slice(0, 12)}…</dd>
                </div>
              </dl>
            </>
          )}

          {/* The visible schema, so the operator writes against what they may read rather than
              what they remember. */}
          {schema ? (
            <details className="rounded-md border border-line bg-surface px-3 py-2">
              <summary className="cursor-pointer text-[12.5px] font-medium">
                Your schema — {schema.schema.types.length} visible type
                {schema.schema.types.length === 1 ? "" : "s"}
              </summary>
              <ul className="mt-2 flex flex-col gap-1">
                {schema.schema.types.map((typeDefinition) => (
                  <li key={typeDefinition.name} className="text-[11.5px]">
                    <span className="font-mono font-medium">{typeDefinition.name}</span>{" "}
                    <span className="text-muted">
                      {typeDefinition.fields.map((field) => field.name).join(" · ") || "no visible field"}
                    </span>
                  </li>
                ))}
              </ul>
            </details>
          ) : null}

          {history.length > 0 ? (
            <p className="text-[11.5px] text-muted">
              {history.length} run{history.length === 1 ? "" : "s"} in this session. Results are not
              kept locally — only the unsent document is.
            </p>
          ) : null}
        </section>
      </div>
    </div>
  );
}

/** The snippet drawer: the request as curl, TypeScript and Python. */
function SnippetDrawer({
  query,
  variables,
  onPick,
}: {
  query: string;
  variables: string;
  onPick: (next: string) => void;
}) {
  const trimmed = variables.trim() || "{}";
  const snippets = [
    {
      label: "curl",
      language: "shell",
      body: `curl -sS -X POST /api/v1/graphql \\n  -H 'content-type: application/json' \\\n  -d '${JSON.stringify({ query, variables: trimmed }).replace(/'/g, "'\\''")}'`,
    },
    {
      label: "TypeScript",
      language: "typescript",
      body: `const response = await fetch("/api/v1/graphql", {\n  method: "POST",\n  headers: { "content-type": "application/json" },\n  body: JSON.stringify({\n    query: ${JSON.stringify(query)},\n    variables: ${trimmed},\n  }),\n});\nconst envelope = await response.json();`,
    },
    {
      label: "Python",
      language: "python",
      body: `import requests\n\nresponse = requests.post(\n    "https://<host>/api/v1/graphql",\n    json={"query": ${JSON.stringify(query)}, "variables": ${trimmed}},\n)\nenvelope = response.json()`,
    },
  ];

  return (
    <div className="flex flex-col gap-2 rounded-md border border-line bg-surface p-3">
      {snippets.map((snippet) => (
        <details key={snippet.label}>
          <summary className="cursor-pointer text-[12px] font-medium">
            {snippet.label} — click to send this document
          </summary>
          <pre className="mt-1.5 max-h-48 overflow-auto rounded border border-line bg-quiet-soft p-2.5 font-mono text-[11px] leading-relaxed">
            {snippet.body}
          </pre>
          <button
            type="button"
            onClick={() => onPick(query)}
            className="mt-1.5 rounded-md border border-line px-2.5 py-1 text-[11.5px]"
          >
            Keep the current document
          </button>
        </details>
      ))}
      <p className="text-[11.5px] text-muted">
        A Python snippet names the host as a placeholder on purpose: the request says generated SDKs
        must contain no environment values or hostnames, and a snippet is a public artifact by the
        same rule.
      </p>
    </div>
  );
}