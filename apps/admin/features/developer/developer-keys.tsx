"use client";

/**
 * The API key list, its create dialog, the one-time reveal and rotate/revoke (REQ-022, slice 2).
 *
 * ## The one-time secret is the whole design constraint of this screen
 *
 * The platform will show a key exactly once, and this screen is the only place that ever sees it.
 * Three consequences, each of which is a way the obvious implementation leaks:
 *
 * 1. **The token lives in React state and nowhere else.** Not `localStorage`, not the URL, not a
 *    ref that outlives the dialog. Closing the reveal drops it; there is no code path that puts
 *    it back, because the API has no route that can return it again.
 * 2. **The dialog cannot be dismissed by accident.** `Esc` and the backdrop both go through the
 *    same acknowledgement, so a stray keypress does not destroy a secret nobody has written
 *    down yet. The REQ asks for an explicit "I have stored it" for exactly this reason.
 * 3. **The copy button reports what it did.** `navigator.clipboard` rejects on an insecure
 *    origin and on a denied permission, and a button that silently does nothing reads as a
 *    broken key. The panel shows the token instead, so the value is still reachable.
 *
 * ## Scopes are grouped by the API, never re-derived here
 *
 * The picker renders `GET /developer/scopes` as it arrives, category by category, and only
 * offers rows the server marked `grantable`. A picker that re-derived the grouping would drift
 * the day a category is renamed, and a picker that offered everything would submit scopes the
 * server then refuses — the form would work and the answer would be `400`.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  Check,
  ClipboardCopy,
  KeyRound,
  Plus,
  RefreshCw,
  RotateCw,
  Search,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import Link from "next/link";

import {
  ApiError,
  createDeveloperKey,
  fetchDeveloperKeys,
  fetchDeveloperScopes,
  revokeDeveloperKey,
  rotateDeveloperKey,
} from "@/lib/api";
import {
  developerFieldOf,
  type DeveloperKey,
  type DeveloperScopeCatalogue,
  type IssuedDeveloperKey,
} from "@/lib/developer";
import { formatTimestamp } from "@/lib/format";

/** The expiry choices the REQ names. `never` is `null` to the API. */
const EXPIRY_CHOICES: { value: string; label: string; days: number | null }[] = [
  { value: "never", label: "Never", days: null },
  { value: "30", label: "30 days", days: 30 },
  { value: "90", label: "90 days", days: 90 },
  { value: "365", label: "1 year", days: 365 },
];

type ListState =
  | { status: "loading" }
  | { status: "ready"; keys: DeveloperKey[] }
  | { status: "error"; message: string };

export function DeveloperKeysScreen() {
  const [state, setState] = useState<ListState>({ status: "loading" });
  const [search, setSearch] = useState("");
  const [status, setStatus] = useState("");
  const [environment, setEnvironment] = useState("");

  const [creating, setCreating] = useState(false);
  const [issued, setIssued] = useState<IssuedDeveloperKey | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);

  const load = useCallback(async () => {
    setState({ status: "loading" });
    try {
      const keys = await fetchDeveloperKeys({
        search: search || null,
        status: status || null,
        environment: environment || null,
      });
      setState({ status: "ready", keys });
    } catch (cause: unknown) {
      setState({
        status: "error",
        message:
          cause instanceof ApiError ? cause.message : "The API keys could not be loaded.",
      });
    }
  }, [search, status, environment]);

  useEffect(() => {
    void load();
  }, [load]);

  // `/` focuses the search box and `n` opens the create dialog, both only when the operator is
  // not already typing — a global `n` handler that fires while somebody types a key name is a
  // dialog that opens itself in the middle of a word.
  const searchRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (event.key === "n" && !typing && !creating && !issued) {
        event.preventDefault();
        setCreating(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [creating, issued]);

  const onRotate = async (key: DeveloperKey) => {
    setBusyId(key.id);
    setFailure(null);
    try {
      const next = await rotateDeveloperKey(key.id);
      // The old secret is already dead the moment this resolves; the reveal says so, because an
      // operator who does not know that will keep shipping the old value.
      setIssued(next);
      await load();
    } catch (cause: unknown) {
      setFailure(
        cause instanceof ApiError ? cause.message : "The key could not be rotated.",
      );
    } finally {
      setBusyId(null);
    }
  };

  const onRevoke = async (key: DeveloperKey) => {
    setBusyId(key.id);
    setFailure(null);
    try {
      await revokeDeveloperKey(key.id);
      setNotice(
        `“${key.name}” no longer authenticates. Its row and its request history are kept.`,
      );
      await load();
    } catch (cause: unknown) {
      setFailure(
        cause instanceof ApiError ? cause.message : "The key could not be revoked.",
      );
    } finally {
      setBusyId(null);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-developer-keys>
      {notice ? (
        <p
          role="status"
          className="rounded-xl border border-positive/30 bg-positive-soft px-4 py-2.5 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}
      {failure ? (
        <p
          role="alert"
          className="rounded-xl border border-caution/30 bg-caution-soft px-4 py-2.5 text-[12.5px] text-caution"
        >
          {failure}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-56 flex-1">
          <Search
            className="pointer-events-none absolute top-2.5 left-2.5 size-3.5 text-muted"
            aria-hidden
          />
          <input
            ref={searchRef}
            type="search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search by name or prefix"
            aria-label="Search API keys"
            className="w-full rounded-lg border border-line bg-surface py-2 pr-3 pl-8 text-[13px] outline-none focus:border-accent"
          />
        </div>
        <select
          value={status}
          onChange={(event) => setStatus(event.target.value)}
          aria-label="Filter by status"
          className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[13px]"
        >
          <option value="">Any status</option>
          <option value="active">Active</option>
          <option value="expired">Expired</option>
          <option value="revoked">Revoked</option>
        </select>
        <select
          value={environment}
          onChange={(event) => setEnvironment(event.target.value)}
          aria-label="Filter by environment"
          className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[13px]"
        >
          <option value="">Any environment</option>
          <option value="live">Live</option>
          <option value="sandbox">Sandbox</option>
        </select>
        <button
          type="button"
          onClick={() => void load()}
          aria-label="Reload the key list"
          className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
        </button>
        <button
          type="button"
          onClick={() => setCreating(true)}
          className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          New key
          <kbd className="ml-1 rounded bg-white/20 px-1 text-[10px]">n</kbd>
        </button>
      </div>

      {state.status === "loading" ? (
        <div className="overflow-hidden rounded-xl border border-line bg-surface" aria-busy="true">
          {[0, 1, 2].map((row) => (
            <div key={row} className="flex gap-3 border-b border-line px-4 py-3.5 last:border-0">
              <span className="block h-3.5 w-40 animate-pulse rounded bg-quiet-soft" />
              <span className="block h-3.5 w-28 animate-pulse rounded bg-quiet-soft" />
              <span className="block h-3.5 w-20 animate-pulse rounded bg-quiet-soft" />
            </div>
          ))}
        </div>
      ) : null}

      {state.status === "error" ? (
        <div role="alert" className="rounded-xl border border-line bg-surface px-4 py-6">
          <p className="flex items-center gap-2 text-[13px] text-caution">
            <TriangleAlert className="size-4" aria-hidden />
            {state.message}
          </p>
          <button
            type="button"
            onClick={() => void load()}
            className="mt-3 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Try again
          </button>
        </div>
      ) : null}

      {state.status === "ready" && state.keys.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface" data-developer-keys-empty>
          <div className="flex flex-col items-center gap-2 px-6 py-12 text-center">
            <KeyRound className="size-5 text-muted" aria-hidden />
            <p className="text-[13.5px] font-medium">
              {search || status || environment
                ? "No key matches these filters"
                : "No API keys yet — create your first"}
            </p>
            <p className="max-w-sm text-[12.5px] text-muted">
              {search || status || environment
                ? "A revoked key keeps its name available, so an operator can reuse it immediately."
                : "A key is a delegation, not an identity: it carries a scope list you choose and may only narrow what you already hold."}
            </p>
            {search || status || environment ? (
              <button
                type="button"
                onClick={() => {
                  setSearch("");
                  setStatus("");
                  setEnvironment("");
                }}
                className="mt-2 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
              >
                Clear filters
              </button>
            ) : (
              <button
                type="button"
                onClick={() => setCreating(true)}
                className="mt-2 inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                <Plus className="size-3.5" aria-hidden />
                Create a key
              </button>
            )}
          </div>
        </div>
      ) : null}

      {state.status === "ready" && state.keys.length > 0 ? (
        <div className="overflow-hidden rounded-xl border border-line bg-surface">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th scope="col" className="px-4 py-2.5 font-medium">Name</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Prefix</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Scopes</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Last used</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Expires</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Status</th>
                <th scope="col" className="px-4 py-2.5 font-medium">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {state.keys.map((key) => (
                <KeyRow
                  key={key.id}
                  entry={key}
                  busy={busyId === key.id}
                  onRotate={() => void onRotate(key)}
                  onRevoke={() => void onRevoke(key)}
                />
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      {issued ? (
        <RevealDialog issued={issued} onClose={() => setIssued(null)} />
      ) : null}
      {creating ? (
        <CreateDialog
          onClose={() => setCreating(false)}
          onCreated={async (result) => {
            setCreating(false);
            setIssued(result);
            await load();
          }}
        />
      ) : null}
    </div>
  );
}

/** One key. The scope cell expands, because "12" tells an operator nothing on its own. */
function KeyRow({
  entry,
  busy,
  onRotate,
  onRevoke,
}: {
  entry: DeveloperKey;
  busy: boolean;
  onRotate: () => void;
  onRevoke: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const dead = entry.status !== "active";

  return (
    <tr className="border-b border-line last:border-0" data-developer-key-row={entry.id}>
      <td className="px-4 py-3 align-top">
        <Link
          href={`/developer/api-keys/${entry.id}`}
          className="font-medium hover:underline"
        >
          {entry.name}
        </Link>
        <div className="text-[11.5px] text-muted">
          {entry.environment} · by {entry.created_by_name || "a deleted account"}
        </div>
      </td>
      {/* Monospace, because this column is compared against a log row by eye. */}
      <td className="px-4 py-3 align-top font-mono text-[12px]">{entry.key_prefix}</td>
      <td className="px-4 py-3 align-top">
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
          className="text-[12.5px] text-accent-strong hover:underline"
        >
          {entry.scopes.length} {entry.scopes.length === 1 ? "scope" : "scopes"}
        </button>
        {expanded ? (
          <ul className="mt-1.5 flex flex-col gap-0.5">
            {entry.scopes.map((scope) => (
              <li key={scope} className="font-mono text-[11.5px] text-muted">
                {scope}
              </li>
            ))}
          </ul>
        ) : null}
      </td>
      <td className="px-4 py-3 align-top text-[12px] text-muted">
        {entry.last_used_at ? formatTimestamp(entry.last_used_at) : "never"}
      </td>
      <td className="px-4 py-3 align-top text-[12px] text-muted">
        {entry.expires_at ? formatTimestamp(entry.expires_at) : "never"}
      </td>
      <td className="px-4 py-3 align-top">
        <span
          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
            dead ? "bg-quiet-soft text-muted" : "bg-positive-soft text-positive"
          }`}
          data-key-status={entry.status}
        >
          {entry.status}
        </span>
      </td>
      <td className="px-4 py-3 align-top">
        <div className="flex items-center justify-end gap-1.5">
          <button
            type="button"
            onClick={onRotate}
            disabled={busy || dead}
            aria-label={`Rotate ${entry.name}`}
            title={dead ? "A dead key cannot be rotated — create a new one" : "Rotate"}
            className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink disabled:opacity-40"
          >
            <RotateCw className="size-3.5" aria-hidden />
          </button>
          <button
            type="button"
            onClick={onRevoke}
            disabled={busy || dead}
            aria-label={`Revoke ${entry.name}`}
            title={dead ? "Already dead" : "Revoke"}
            className="rounded-lg border border-line p-1.5 text-muted transition hover:text-caution disabled:opacity-40"
          >
            <Trash2 className="size-3.5" aria-hidden />
          </button>
        </div>
      </td>
    </tr>
  );
}

/**
 * The one-time reveal.
 *
 * `acknowledged` is a separate flag rather than a direct "close": the panel can be closed by
 * `Esc`, the backdrop, or the button, and all three routes through here, so the one thing this
 * dialog must never do is disappear before the operator has said they stored the value.
 */
function RevealDialog({
  issued,
  onClose,
}: {
  issued: IssuedDeveloperKey;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const [copyFailed, setCopyFailed] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && acknowledged) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [acknowledged, onClose]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(issued.token);
      setCopied(true);
      setCopyFailed(false);
    } catch {
      // The value stays on screen either way, so a refused clipboard degrades to "read it off
      // the screen" instead of to "the key is lost".
      setCopyFailed(true);
    }
  };

  return (
    <div className="fixed inset-0 z-40 flex items-center justify-center bg-ink/40 p-4">
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="reveal-title"
        data-developer-reveal
        className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-surface p-5"
      >
        <div className="flex items-start justify-between gap-3">
          <h2 id="reveal-title" className="text-[15px] font-semibold">
            Your new key
          </h2>
          <button
            type="button"
            onClick={onClose}
            disabled={!acknowledged}
            aria-label="Close"
            className="rounded-lg p-1 text-muted transition hover:text-ink disabled:opacity-30"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        <p className="flex items-start gap-2 rounded-lg border border-caution/30 bg-caution-soft px-3 py-2 text-[12.5px] text-caution">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>
            This is the only time the platform will show it. It is stored hashed, so it cannot
            be shown again or recovered — store it in your secret manager now.
          </span>
        </p>

        <div className="flex items-center gap-2">
          <code
            data-developer-token
            className="min-w-0 flex-1 overflow-x-auto rounded-lg border border-line bg-quiet-soft px-3 py-2 font-mono text-[12px] whitespace-nowrap"
          >
            {issued.token}
          </code>
          <button
            type="button"
            onClick={() => void copy()}
            className="inline-flex shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 py-2 text-[12.5px] transition hover:bg-quiet-soft"
          >
            {copied ? (
              <Check className="size-3.5 text-positive" aria-hidden />
            ) : (
              <ClipboardCopy className="size-3.5" aria-hidden />
            )}
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
        {copyFailed ? (
          <p role="alert" className="text-[12px] text-caution">
            The clipboard was refused. The key is still on screen — copy it from there.
          </p>
        ) : null}

        <label className="flex items-start gap-2 text-[12.5px]">
          <input
            type="checkbox"
            checked={acknowledged}
            onChange={(event) => setAcknowledged(event.target.checked)}
            className="mt-0.5"
          />
          <span>I have stored this key somewhere safe.</span>
        </label>

        <div className="flex justify-end">
          <button
            type="button"
            onClick={onClose}
            disabled={!acknowledged}
            className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-40"
          >
            Done
          </button>
        </div>
      </div>
    </div>
  );
}

/** The create form. Its values survive a failed submit — the REQ asks for that explicitly. */
function CreateDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (issued: IssuedDeveloperKey) => void | Promise<void>;
}) {
  const [name, setName] = useState("");
  const [environment, setEnvironment] = useState("live");
  const [expiry, setExpiry] = useState("never");
  const [scopes, setScopes] = useState<string[]>([]);
  const [catalogue, setCatalogue] = useState<DeveloperScopeCatalogue | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [errorField, setErrorField] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchDeveloperScopes()
      .then((value) => {
        if (!cancelled) setCatalogue(value);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setLoadError(
            cause instanceof ApiError
              ? cause.message
              : "The scope catalogue could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // `Enter` submits from anywhere in the form except a textarea, which does not exist here — so
  // the key is bound on the form itself rather than on the name field.
  const submit = async () => {
    setSubmitting(true);
    setError(null);
    setErrorField(null);
    try {
      const days = EXPIRY_CHOICES.find((choice) => choice.value === expiry)?.days ?? null;
      const expiresAt =
        days === null
          ? null
          : new Date(Date.now() + days * 24 * 60 * 60 * 1000).toISOString();
      const issued = await createDeveloperKey({
        name,
        scopes,
        environment,
        expires_at: expiresAt,
      });
      await onCreated(issued);
    } catch (cause: unknown) {
      // **The form keeps its values.** A create that fails and empties the name field is an
      // operator retyping three scopes they chose deliberately, and the REQ names this.
      setError(
        cause instanceof ApiError ? cause.message : "The key could not be created.",
      );
      setErrorField(
        cause instanceof ApiError ? developerFieldOf(cause.details) : null,
      );
    } finally {
      setSubmitting(false);
    }
  };

  const nameError =
    errorField === "name"
      ? error
      : name.trim().length > 0 && name.trim().length < 3
        ? "A name needs at least 3 characters."
        : null;
  const scopesError = errorField === "scopes" ? error : null;

  const grouped = useMemo(() => catalogue?.categories ?? [], [catalogue]);

  return (
    <div className="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto bg-ink/40 p-4">
      <form
        role="dialog"
        aria-modal="true"
        aria-labelledby="create-key-title"
        data-developer-create
        className="my-8 flex w-full max-w-2xl flex-col gap-4 rounded-xl border border-line bg-surface p-5"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <div className="flex items-start justify-between gap-3">
          <h2 id="create-key-title" className="text-[15px] font-semibold">
            New API key
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded-lg p-1 text-muted transition hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        {error && !nameError && !scopesError ? (
          <p
            role="alert"
            className="rounded-lg border border-caution/30 bg-caution-soft px-3 py-2 text-[12.5px] text-caution"
          >
            {error}
          </p>
        ) : null}

        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <label className="flex flex-col gap-1">
            <span className="text-[12.5px] font-medium">Name</span>
            <input
              type="text"
              value={name}
              onChange={(event) => setName(event.target.value)}
              required
              minLength={3}
              maxLength={64}
              autoFocus
              aria-invalid={nameError ? true : undefined}
              aria-describedby={nameError ? "create-key-name-error" : undefined}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus:border-accent"
            />
            {nameError ? (
              <span id="create-key-name-error" className="text-[11.5px] text-caution">
                {nameError}
              </span>
            ) : (
              <span className="text-[11.5px] text-muted">
                Unique per environment. 3–64 characters.
              </span>
            )}
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12.5px] font-medium">Environment</span>
            <select
              value={environment}
              onChange={(event) => setEnvironment(event.target.value)}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px]"
            >
              {(catalogue?.environments ?? ["live", "sandbox"]).map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
            <span className="text-[11.5px] text-muted">
              A live key and a sandbox key are different systems.
            </span>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12.5px] font-medium">Expiry</span>
            <select
              value={expiry}
              onChange={(event) => setExpiry(event.target.value)}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px]"
            >
              {EXPIRY_CHOICES.map((choice) => (
                <option key={choice.value} value={choice.value}>
                  {choice.label}
                </option>
              ))}
            </select>
            <span className="text-[11.5px] text-muted">
              An expired key stops authenticating and keeps its history.
            </span>
          </label>
        </div>

        <div className="flex flex-col gap-1.5">
          <span className="text-[12.5px] font-medium">
            Scopes{" "}
            {scopes.length > 0 ? (
              <span className="text-muted">({scopes.length} selected)</span>
            ) : null}
          </span>

          {loadError ? (
            <p role="alert" className="rounded-lg border border-caution/30 bg-caution-soft px-3 py-2 text-[12px] text-caution">
              {loadError}
            </p>
          ) : null}
          {!catalogue && !loadError ? (
            <p className="text-[12px] text-muted" aria-busy="true">
              Loading the scope catalogue…
            </p>
          ) : null}

          {grouped.map((category) => (
            <fieldset key={category.key} className="rounded-lg border border-line px-3 py-2">
              <legend className="px-1 text-[11.5px] font-medium text-muted">
                {category.key}
              </legend>
              <div className="grid grid-cols-1 gap-1 sm:grid-cols-2">
                {category.scopes.map((scope) => {
                  // A scope the caller does not hold is shown **disabled with its reason**, not
                  // hidden: an operator looking for `content.pages.manage` and finding nothing
                  // has no way to learn that the account lacks it.
                  const unavailable = !scope.grantable;
                  return (
                    <label
                      key={scope.key}
                      title={scope.description}
                      className={`flex items-start gap-2 text-[12px] ${
                        unavailable ? "text-muted" : ""
                      }`}
                    >
                      <input
                        type="checkbox"
                        checked={scopes.includes(scope.key)}
                        disabled={unavailable}
                        onChange={(event) =>
                          setScopes((current) =>
                            event.target.checked
                              ? [...current, scope.key]
                              : current.filter((value) => value !== scope.key),
                          )
                        }
                        className="mt-0.5"
                      />
                      <span className="min-w-0">
                        <span className="block font-mono">{scope.key}</span>
                        <span className="block text-[11px] text-muted">
                          {unavailable
                            ? "You do not hold this, so you cannot delegate it."
                            : scope.description}
                        </span>
                      </span>
                    </label>
                  );
                })}
              </div>
            </fieldset>
          ))}

          {scopesError ? (
            <span role="alert" className="text-[11.5px] text-caution">
              {scopesError}
            </span>
          ) : null}
        </div>

        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={submitting || scopes.length === 0 || name.trim().length < 3}
            className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-40"
          >
            {submitting ? "Creating…" : "Create key"}
          </button>
        </div>
      </form>
    </div>
  );
}
