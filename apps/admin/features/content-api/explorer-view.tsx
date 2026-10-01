"use client";

/**
 * `/content-api/explorer` — the Explorer tab (REQ-019, slice 3).
 *
 * The screen an integrator uses to find out whether the content API works, and it makes **real
 * calls** — the same handlers, the same limiter, the same bytes over a real router. Nothing here
 * is a simulation or a sample response, for one reason that shapes the whole component:
 *
 * **The panel cannot hold a token's plaintext.** The API stores only a digest, so the call is
 * *dispatched server-side as* a named token's row. That is why the picker asks for a token **id**
 * and why the snippets carry `$OMNION_TOKEN` rather than a credential: a screen that showed a
 * secret would be showing something the platform cannot produce, and a snippet with one baked in
 * would put it in somebody's shell history the moment they pasted it.
 *
 * Four things this screen has to do that a list of endpoints cannot:
 *
 * 1. **Show what the caller will actually get** — status, the two rate-limit headers, `etag`, the
 *    timing, and the resolved URL. A snippet you have not executed is a guess; a response you can
 *    read is an answer.
 * 2. **Make the two numbers that matter explicit.** `x-ratelimit-remaining` decreasing across two
 *    sends is the platform's limiter proving itself, and a screenshot of that is what a person
 *    debugging an integration needs. The header is absent rather than zero when the counter could
 *    not be read, and this screen says "not counted" instead of drawing a `0`.
 * 3. **Deep-link its own state** (`?endpoint=pages.list&token=…&limit=5`), so a colleague can be
 *    sent to the exact call that is failing rather than to a screen they must rebuild by hand.
 * 4. **Never send a token it cannot use.** A revoked or expired token is listed but not
 *    selectable, with the reason on the option — the alternative is a `403` on the Send button
 *    that reads as "the API is broken".
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams, type ReadonlyURLSearchParams } from "next/navigation";
import {
  AlertCircle,
  Check,
  ChevronRight,
  Clock,
  Copy,
  CornerDownLeft,
  Loader2,
  Play,
  Send,
  Terminal,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  explorerEndpoints,
  fetchContentApiOpenApi,
  fetchContentApiTokens,
  runContentApiExplorer,
} from "@/lib/api";
import type {
  ContentApiToken,
  ExplorerAnswer,
  ExplorerEndpoint,
  ExplorerSnippets,
} from "@/lib/types";

/** The call the tab opens on: the list a person integrating is looking for first. */
const DEFAULT_OPERATION = "pages.list";

/** The three snippet renderings, in the order they are most likely to be wanted. */
const SNIPPET_LANGS = [
  { key: "curl", label: "cURL" },
  { key: "fetch", label: "fetch" },
  { key: "python", label: "Python" },
] as const satisfies readonly { key: keyof ExplorerSnippets; label: string }[];

/** Read one query-string value, `null` when absent or empty. */
function paramOf(params: ReadonlyURLSearchParams, key: string): string | null {
  const value = params.get(key);
  return value !== null && value.trim() !== "" ? value : null;
}

export function ContentApiExplorerView() {
  // ONE value, not a tuple: destructuring this returned `[params, setter]` and every call below
  // then passed a two-string tuple where a URLSearchParams belongs.
  const search = useSearchParams();
  const { push } = useRouter();
  const [tokens, setTokens] = useState<ContentApiToken[]>([]);
  const [endpoints, setEndpoints] = useState<ExplorerEndpoint[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);

  const operationId = paramOf(search, "endpoint") ?? DEFAULT_OPERATION;
  const tokenId = paramOf(search, "token");
  const deepParams = useDeepLinkedParams(search);

  const [answer, setAnswer] = useState<ExplorerAnswer | null>(null);
  const [sending, setSending] = useState(false);
  const [sendError, setSendError] = useState<ApiError | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // The parameters of the *currently selected* endpoint. Keyed by endpoint id so switching
  // endpoints resets the values that endpoint no longer takes — a form that keeps `limit` across
  // a switch to `sites.list` sends a parameter the API will refuse, and the refusal reads as the
  // API's fault.
  const [values, setValues] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      // Both reads are needed before the form can be honest: the document says what an endpoint
      // takes, and the token list says who may call it. Rendering one without the other produces
      // a form that offers a field nobody can fill, or a token picker with nothing in it.
      const [document, tokenList] = await Promise.all([
        fetchContentApiOpenApi(),
        fetchContentApiTokens(),
      ]);
      setEndpoints(explorerEndpoints(document));
      setTokens(tokenList);
    } catch (cause) {
      setLoadError(
        cause instanceof ApiError ? cause.message : "The explorer could not be prepared.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const endpoint = useMemo(
    () => endpoints.find((entry) => entry.operationId === operationId) ?? null,
    [endpoints, operationId],
  );

  // Only tokens that can actually authenticate are offered. A revoked or expired one in the list
  // is a row about a dead credential; putting it in the picker would only produce a `403` on the
  // Send button, and an operator reads that as "the content API is broken".
  const usable = useMemo(() => tokens.filter((token) => token.status === "active"), [tokens]);
  const unusable = useMemo(
    () => tokens.filter((token) => token.status !== "active"),
    [tokens],
  );

  const selected = useMemo(
    () => usable.find((token) => token.id === tokenId) ?? usable[0] ?? null,
    [usable, tokenId],
  );

  // The deep-linked parameter values seed the form once per endpoint, and a later edit is the
  // user's. Keyed on the resolved endpoint so a link to a different call replaces the form rather
  // than merging into whatever was typed before.
  const seededFor = useRef<string | null>(null);
  useEffect(() => {
    if (endpoint === null) return;
    const key = `${endpoint.operationId}|${tokenId ?? ""}`;
    if (seededFor.current === key) return;
    seededFor.current = key;
    setValues(deepParams);
    setAnswer(null);
    setSendError(null);
  }, [endpoint, deepParams, tokenId]);

  const setParameter = useCallback((name: string, value: string) => {
    setValues((current) => ({ ...current, [name]: value }));
  }, []);

  /** Write the state a reader can send somebody else. */
  const applyUrl = useCallback(
    (next: { operationId?: string; tokenId?: string | null; params?: Record<string, string> }) => {
      // Copied into a writable copy: `useSearchParams` hands a readonly view and the next lines
      // delete and set on it, and mutating the router's object is how a screen ends up fighting
      // the navigation that re-rendered it.
      const params = new URLSearchParams(search.toString());
      const operation = next.operationId ?? operationId;
      params.set("endpoint", operation);
      const token = next.tokenId === undefined ? (tokenId ?? selected?.id ?? null) : next.tokenId;
      if (token !== null && token !== undefined) params.set("token", token);
      else params.delete("token");
      const carried = next.params ?? values;
      for (const key of Array.from(params.keys())) {
        if (key !== "endpoint" && key !== "token") params.delete(key);
      }
      for (const [key, value] of Object.entries(carried)) {
        if (value !== "") params.set(key, value);
      }
      // `push`, not `replace`: the link is meant to be copyable and to survive a reload,
      // and a replace-chain makes the Back button walk through parameter edits one at a time.
      push(`?${params.toString()}`);
    },
    [operationId, push, search, selected?.id, tokenId, values],
  );

  const send = useCallback(async () => {
    if (selected === null || endpoint === null) return;
    setSending(true);
    setSendError(null);
    setNotice(null);
    try {
      // Empty values are dropped rather than sent as `?slug=`: the server's `integer`/`uuid`
      // readers treat an empty string as "absent" for most fields but a `Path` parameter cannot
      // be empty at all, and a `400` about a field the form shows as blank is a dead end.
      const carried: Record<string, string> = {};
      for (const [key, value] of Object.entries(values)) {
        if (value.trim() !== "") carried[key] = value.trim();
      }
      setAnswer(
        await runContentApiExplorer({
          token_id: selected.id,
          operation_id: endpoint.operationId,
          params: carried,
        }),
      );
    } catch (cause) {
      setAnswer(null);
      setSendError(
        cause instanceof ApiError
          ? cause
          : new ApiError(0, "network_error", "The call could not be made."),
      );
    } finally {
      setSending(false);
    }
  }, [endpoint, selected, values]);

  // "Use cursor for next page" is raised by the response pane, which knows the cursor but not the
  // form. Listening here is what keeps the button real: without this handler the button would set
  // `cursor` on the form and nothing else, and the next Send would page while the screen still
  // showed the previous page's parameters — a paginator that answers a different question than
  // the one on screen.
  useEffect(() => {
    const onNext = (event: Event) => {
      const cursor = (event as CustomEvent<{ cursor?: unknown }>).detail?.cursor;
      if (typeof cursor !== "string" || cursor === "") return;
      setValues((current) => ({ ...current, cursor }));
      setAnswer(null);
      setSendError(null);
    };
    window.addEventListener("content-api-explorer:next-page", onNext);
    return () => window.removeEventListener("content-api-explorer:next-page", onNext);
  }, []);

  // `s` sends, `⌘/Ctrl+Enter` sends from inside a field. Ignored while a field has focus for a
  // plain `s`, because a form is not a keyboard surface for the screen behind it — and `⌘Enter`
  // is the one that works *because* you are in a field.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement;
      if (typing) {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          event.preventDefault();
          void send();
        }
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "s") {
        event.preventDefault();
        void send();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [send]);

  const copy = useCallback(async (text: string, label: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setNotice(`${label} copied`);
    } catch {
      setNotice("The clipboard is not available in this browser — select the text and copy it.");
    }
  }, []);

  if (loading) {
    return (
      <div data-content-api-explorer="loading" aria-busy="true" className="space-y-3">
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          Loading the API document and your tokens…
        </p>
        <div className="grid gap-3 lg:grid-cols-[240px_1fr]">
          <div className="h-64 animate-pulse rounded-xl bg-quiet-soft" />
          <div className="h-64 animate-pulse rounded-xl bg-quiet-soft" />
        </div>
      </div>
    );
  }

  if (loadError !== null || endpoint === null || endpoints.length === 0) {
    return (
      <div data-content-api-explorer="error" className="space-y-3">
        <p
          role="alert"
          data-content-api-explorer-error
          className="flex items-start gap-2 rounded-xl border border-red-500/40 px-3 py-2 text-[12.5px]"
        >
          <AlertCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            {loadError ??
              "The API document listed no operations, so there is nothing to call."}
          </span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          data-content-api-explorer-retry
          className="flex w-fit items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
        >
          Try again
        </button>
      </div>
    );
  }

  return (
    <div data-content-api-explorer="ready" className="space-y-4">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <p className="max-w-3xl text-[12.5px] text-muted">
          Real calls against the headless surface, made as the token you pick. The platform stores
          only a digest of a token, so the call is dispatched for you and the snippet you copy
          reads the credential from your own shell — nothing secret is ever shown here.
        </p>
        <button
          type="button"
          onClick={() => void copy(window.location.href, "Link")}
          data-content-api-explorer-copy-link
          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
        >
          <Copy className="size-3.5" aria-hidden />
          Copy link to this call
        </button>
      </header>

      {tokens.length === 0 ? (
        <EmptyState
          title="No token to call as"
          hint="The Explorer dispatches as a token, because a token's secret is stored as a digest and cannot be replayed. Mint one on the Tokens tab first."
        />
      ) : usable.length === 0 ? (
        <EmptyState
          title="Every token is revoked or expired"
          hint="None of them can authenticate, so a call made as one would prove nothing. Rotate or mint a token on the Tokens tab."
        />
      ) : (
        <div className="grid gap-4 lg:grid-cols-[260px_minmax(0,1fr)]">
          {/* ------------------------------------------------------ the endpoint picker */}
          <nav
            aria-label="Endpoints"
            data-content-api-explorer-endpoints={endpoints.length}
            className="flex flex-col gap-1 lg:sticky lg:top-4 lg:self-start"
          >
            <h2 className="mb-1 text-[12px] font-medium tracking-wide text-muted uppercase">
              Endpoint
            </h2>
            {endpoints.map((entry) => {
              const current = entry.operationId === endpoint.operationId;
              return (
                <button
                  key={entry.operationId}
                  type="button"
                  aria-current={current ? "true" : undefined}
                  data-content-api-explorer-endpoint={entry.operationId}
                  onClick={() => applyUrl({ operationId: entry.operationId, params: {} })}
                  className={`flex items-center gap-2 rounded-lg px-2 py-1.5 text-left text-[12.5px] transition ${
                    current
                      ? "bg-accent-soft font-medium text-accent-strong"
                      : "text-muted hover:bg-quiet-soft hover:text-ink"
                  }`}
                >
                  <span className="shrink-0 rounded border border-line px-1 py-0.5 font-mono text-[10px] uppercase">
                    {entry.method}
                  </span>
                  <span className="min-w-0 flex-1 truncate">{entry.operationId}</span>
                  {entry.experimental ? (
                    <span
                      title="Experimental: this shape will change when the blog module ships"
                      className="shrink-0 rounded border border-amber-500/40 px-1 text-[10px] text-amber-700 dark:text-amber-400"
                    >
                      exp
                    </span>
                  ) : null}
                </button>
              );
            })}
          </nav>

          {/* --------------------------------------------- the form, and the answer */}
          <div className="flex min-w-0 flex-col gap-4">
            <section className="rounded-xl border border-line px-4 py-3">
              <div className="flex flex-wrap items-center gap-2">
                <code className="min-w-0 font-mono text-[12px] break-all">
                  {endpoint.method} {endpoint.path}
                </code>
                {endpoint.requiredScope ? (
                  <span
                    data-content-api-explorer-scope
                    className="rounded border border-line px-1.5 py-0.5 font-mono text-[10.5px] text-muted"
                    title="The scope a token must carry for this call to be served"
                  >
                    needs {endpoint.requiredScope}
                  </span>
                ) : null}
                {endpoint.experimental ? (
                  <span className="rounded border border-amber-500/40 px-1.5 py-0.5 text-[10.5px] text-amber-700 dark:text-amber-400">
                    experimental — this shape will change
                  </span>
                ) : null}
              </div>
              <p className="mt-1.5 text-[12.5px] text-muted">{endpoint.summary}</p>

              {/* ------------------------------------------------ the token it calls as */}
              <label
                htmlFor="content-api-explorer-token"
                className="mt-3 block text-[12px] font-medium"
              >
                Call as
              </label>
              <select
                id="content-api-explorer-token"
                data-content-api-explorer-token
                value={selected?.id ?? ""}
                onChange={(event) => applyUrl({ tokenId: event.target.value, params: {} })}
                className="mt-1 w-full rounded-md border border-line bg-transparent px-2 py-1.5 text-[12.5px]"
              >
                {usable.map((token) => (
                  <option key={token.id} value={token.id}>
                    {token.name} · {token.prefix} · {token.rate_limit_per_minute}/min
                  </option>
                ))}
                {unusable.map((token) => (
                  <option key={token.id} value={token.id} disabled>
                    {token.name} · {token.prefix} · {token.status}
                  </option>
                ))}
              </select>
              {endpoint.requiredScope !== null &&
              selected !== null &&
              !selected.scopes.includes(endpoint.requiredScope) ? (
                <p
                  role="status"
                  data-content-api-explorer-scope-warning
                  className="mt-1.5 flex items-start gap-1.5 text-[11.5px] text-amber-700 dark:text-amber-400"
                >
                  <AlertCircle className="mt-0.5 size-3 shrink-0" aria-hidden />
                  <span>
                    {selected.name} does not carry <code>{endpoint.requiredScope}</code>, so this
                    call will be refused with <code>403 insufficient_scope</code>. That is the
                    platform&rsquo;s answer, not a mistake in this screen.
                  </span>
                </p>
              ) : null}

              {/* ------------------------------------------------------ the parameters */}
              <div className="mt-4 grid gap-3 sm:grid-cols-2">
                {endpoint.pathParams.map((parameter) => (
                  <ParameterField
                    key={parameter.name}
                    name={parameter.name}
                    description={parameter.description}
                    required
                    value={values[parameter.name] ?? ""}
                    offender={sendError?.details?.field}
                    onChange={(value) => setParameter(parameter.name, value)}
                  />
                ))}
                {endpoint.query.map((parameter) => (
                  <ParameterField
                    key={parameter.name}
                    name={parameter.name}
                    description={parameter.description}
                    required={false}
                    value={values[parameter.name] ?? ""}
                    offender={sendError?.details?.field}
                    onChange={(value) => setParameter(parameter.name, value)}
                  />
                ))}
              </div>
              {endpoint.pathParams.length === 0 && endpoint.query.length === 0 ? (
                <p className="mt-3 text-[12px] text-muted">
                  This endpoint takes no parameters.
                </p>
              ) : null}

              <div className="mt-4 flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  onClick={() => void send()}
                  disabled={sending || selected === null}
                  data-content-api-explorer-send
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-ink disabled:opacity-60"
                >
                  {sending ? (
                    <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  ) : (
                    <Send className="size-3.5" aria-hidden />
                  )}
                  Send
                </button>
                <span className="flex items-center gap-1 text-[11.5px] text-muted">
                  <CornerDownLeft className="size-3" aria-hidden />
                  <kbd className="rounded border border-line px-1 font-mono">s</kbd> or
                  <kbd className="rounded border border-line px-1 font-mono">⌘/Ctrl+Enter</kbd>
                </span>
                {notice !== null ? (
                  <span
                    data-content-api-explorer-notice
                    className="flex items-center gap-1.5 text-[11.5px] text-muted"
                  >
                    <Check className="size-3" aria-hidden />
                    {notice}
                  </span>
                ) : null}
              </div>
            </section>

            {sendError !== null ? (
              <p
                role="alert"
                data-content-api-explorer-error
                className="flex items-start gap-2 rounded-xl border border-red-500/40 px-3 py-2 text-[12.5px]"
              >
                <AlertCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                <span>
                  {sendError.message}
                  {sendError.code ? (
                    <span className="ml-1.5 font-mono text-[11px] opacity-80">
                      {sendError.code}
                    </span>
                  ) : null}
                  {typeof sendError.retryAfterSeconds === "number" ? (
                    <span className="ml-1.5">
                      Try again in {sendError.retryAfterSeconds}s.
                    </span>
                  ) : null}
                </span>
              </p>
            ) : null}

            {answer === null ? (
              <p
                data-content-api-explorer-idle
                className="rounded-xl border border-line px-4 py-6 text-center text-[12.5px] text-muted"
              >
                Send a request to see the status, the headers and the body — the same answer your
                frontend would get.
              </p>
            ) : (
              <AnswerPane answer={answer} onCopy={copy} />
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Every parameter except the two the tab owns itself.
 *
 * `forEach` rather than iterating entries: Next hands a `ReadonlyURLSearchParams`, which is not
 * itself an iterable of pairs in the type it is declared with, and `useMemo` keyed on the string
 * keeps the form from being reseeded by an unrelated router push.
 */
function useDeepLinkedParams(search: ReadonlyURLSearchParams): Record<string, string> {
  const key = search.toString();
  return useMemo(() => {
    const carried: Record<string, string> = {};
    search.forEach((value, name) => {
      if (name === "endpoint" || name === "token") return;
      if (value !== "") carried[name] = value;
    });
    return carried;
  }, [key]);
}

/**
 * One parameter input.
 *
 * The `offender` prop is what makes a refusal actionable: the API names the field it refused in
 * `details.field`, and the field it names is highlighted here rather than left for the reader to
 * find by trying values. A form that says "invalid parameter" without pointing at one is the form
 * everybody works around by guessing.
 */
function ParameterField({
  name,
  description,
  required,
  value,
  offender,
  onChange,
}: {
  name: string;
  description: string;
  required: boolean;
  value: string;
  /** The `details.field` of the last refusal, when it named this field. */
  offender?: unknown;
  onChange: (value: string) => void;
}) {
  const marked = offender === name;
  return (
    <div data-content-api-explorer-field={name} data-offender={marked ? "true" : undefined}>
      <label
        htmlFor={`content-api-explorer-${name}`}
        className="flex items-center gap-1.5 text-[12px] font-medium"
      >
        <code className="font-mono">{name}</code>
        {required ? (
          <span className="text-[10.5px] font-normal text-muted">required</span>
        ) : null}
      </label>
      <input
        id={`content-api-explorer-${name}`}
        name={name}
        value={value}
        required={required}
        onChange={(event) => onChange(event.target.value)}
        aria-invalid={marked || undefined}
        placeholder={placeholderFor(name)}
        className={`mt-1 w-full rounded-md border bg-transparent px-2 py-1.5 font-mono text-[12px] ${
          marked ? "border-red-500" : "border-line"
        }`}
      />
      <p className="mt-0.5 text-[11px] text-muted">{description}</p>
    </div>
  );
}

/** A placeholder that shows the *shape*, never a value that looks like real data. */
function placeholderFor(name: string): string {
  switch (name) {
    case "site":
      return "site id (optional)";
    case "locale":
      return "tr";
    case "limit":
      return "5";
    case "cursor":
      return "opaque cursor from next_cursor";
    case "fields":
      return "slug,title";
    case "slug":
      return "about";
    default:
      return "optional";
  }
}

/**
 * Status, headers, body, snippets.
 *
 * The header table is not decoration: `x-ratelimit-remaining` is the number an integrator builds
 * a back-off on, and it is **absent** rather than zero when the meter failed open. This pane says
 * "not counted" in that case, because a `0` there would be telling a reader their token is spent
 * when the platform never measured anything.
 */
function AnswerPane({
  answer,
  onCopy,
}: {
  answer: ExplorerAnswer;
  onCopy: (text: string, label: string) => Promise<void>;
}) {
  const [lang, setLang] = useState<keyof ExplorerSnippets>("curl");
  const remaining = answer.headers.find((header) => header.name === "x-ratelimit-remaining");

  return (
    <div data-content-api-explorer-answer={answer.operation_id} className="flex flex-col gap-4">
      <section className="rounded-xl border border-line px-4 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span
            data-content-api-explorer-status={answer.status}
            className={`rounded px-1.5 py-0.5 font-mono text-[12px] ${
              answer.status < 400
                ? "bg-green-500/15 text-green-700 dark:text-green-400"
                : "bg-amber-500/15 text-amber-700 dark:text-amber-400"
            }`}
          >
            {answer.status}
          </span>
          <span className="flex items-center gap-1.5 text-[12px] text-muted">
            <Clock className="size-3.5" aria-hidden />
            {answer.duration_ms} ms
          </span>
          <span
            data-content-api-explorer-metered={answer.metered_route}
            className="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-muted"
            title="The route this call was metered against — the Usage tab's leaderboard keys on it"
          >
            {answer.metered_route}
          </span>
          {remaining ? (
            <span
              data-content-api-explorer-remaining={remaining.value}
              className="rounded border border-line px-1.5 py-0.5 font-mono text-[11px]"
              title="Requests left in this token's minute, from the limiter itself"
            >
              {remaining.value} left of {answer.token.rate_limit_per_minute}
            </span>
          ) : (
            <span
              data-content-api-explorer-remaining="unknown"
              className="rounded border border-amber-500/40 px-1.5 py-0.5 text-[11px] text-amber-700 dark:text-amber-400"
              title="The counter could not be read, so the request was served but NOT counted — the meter fails open"
            >
              not counted
            </span>
          )}
        </div>
        <p className="mt-2 flex items-center gap-1.5 font-mono text-[11.5px] break-all text-muted">
          <ChevronRight className="size-3 shrink-0" aria-hidden />
          <span data-content-api-explorer-url>{answer.url}</span>
          <button
            type="button"
            onClick={() => void onCopy(answer.url, "URL")}
            data-content-api-explorer-copy-url
            aria-label="Copy the request URL"
            className="shrink-0 rounded border border-line p-0.5"
          >
            <Copy className="size-3" aria-hidden />
          </button>
        </p>
      </section>

      <section className="rounded-xl border border-line px-4 py-3">
        <h3 className="text-[12px] font-medium tracking-wide text-muted uppercase">
          Response headers
        </h3>
        {answer.headers.length === 0 ? (
          <p className="mt-1 text-[12px] text-muted">
            This response carried none of the headers an integrator branches on.
          </p>
        ) : (
          <dl
            data-content-api-explorer-headers={answer.headers.length}
            className="mt-1.5 grid grid-cols-[minmax(0,auto)_minmax(0,1fr)] gap-x-3 gap-y-1 font-mono text-[11.5px]"
          >
            {answer.headers.map((header) => (
              <div key={header.name} className="contents">
                <dt className="truncate text-muted">{header.name}</dt>
                <dd className="truncate" title={header.value}>
                  {header.value}
                </dd>
              </div>
            ))}
          </dl>
        )}
      </section>

      <section className="rounded-xl border border-line px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-[12px] font-medium tracking-wide text-muted uppercase">
            {answer.body_is_json ? "Body (JSON)" : "Body (text)"}
          </h3>
          <button
            type="button"
            onClick={() =>
              void onCopy(
                answer.body_is_json ? JSON.stringify(answer.body, null, 2) : String(answer.body),
                "Body",
              )
            }
            data-content-api-explorer-copy-body
            className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px]"
          >
            <Copy className="size-3" aria-hidden />
            Copy body
          </button>
        </div>
        <pre
          data-content-api-explorer-body
          className="mt-2 max-h-96 overflow-auto rounded-lg bg-quiet-soft px-3 py-2 font-mono text-[11.5px] leading-relaxed"
        >
          {answer.body_is_json
            ? JSON.stringify(answer.body, null, 2)
            : String(answer.body)}
        </pre>
        {answer.next_cursor !== null ? (
          <button
            type="button"
            data-content-api-explorer-next-page
            onClick={() => applyNextCursor(answer.next_cursor as string)}
            className="mt-2 flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
          >
            <Play className="size-3.5" aria-hidden />
            Use cursor for next page
          </button>
        ) : null}
      </section>

      <section className="rounded-xl border border-line px-4 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="flex items-center gap-1.5 text-[12px] font-medium tracking-wide text-muted uppercase">
            <Terminal className="size-3.5" aria-hidden />
            Copy as
          </h3>
          {SNIPPET_LANGS.map((entry) => (
            <button
              key={entry.key}
              type="button"
              data-content-api-explorer-snippet={entry.key}
              onClick={() => setLang(entry.key)}
              aria-pressed={lang === entry.key}
              className={`rounded px-1.5 py-0.5 text-[11.5px] ${
                lang === entry.key
                  ? "bg-accent-soft font-medium text-accent-strong"
                  : "text-muted hover:bg-quiet-soft"
              }`}
            >
              {entry.label}
            </button>
          ))}
          <button
            type="button"
            onClick={() => void onCopy(answer.snippets[lang], SNIPPET_LANGS.find((entry) => entry.key === lang)?.label ?? "Snippet")}
            data-content-api-explorer-copy-snippet
            className="ml-auto flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
          >
            <Copy className="size-3.5" aria-hidden />
            Copy
          </button>
        </div>
        <pre
          data-content-api-explorer-snippet
          className="mt-2 overflow-x-auto rounded-lg bg-quiet-soft px-3 py-2 font-mono text-[11.5px] leading-relaxed"
        >
          {answer.snippets[lang]}
        </pre>
        {/* Stated here rather than left to be inferred: a snippet with a credential in it would be
            pasted into a shell, and a shell keeps its history. */}
        <p className="mt-1.5 text-[11px] text-muted">
          The snippet reads the credential from <code>$OMNION_TOKEN</code> in your shell — this
          screen never holds one, and neither should your history.
        </p>
      </section>
    </div>
  );
}

/**
 * Move to the next page.
 *
 * A module-level callback would need the whole form's state to reach `AnswerPane`, and the pane
 * has none of it: the pane knows the cursor, the view knows the form. So the button reports the
 * value upward by dispatching a custom event the view listens for — and the view is the only
 * place that knows which parameters are still valid for the endpoint it is showing.
 */
function applyNextCursor(cursor: string): void {
  window.dispatchEvent(
    new CustomEvent("content-api-explorer:next-page", { detail: { cursor } }),
  );
}