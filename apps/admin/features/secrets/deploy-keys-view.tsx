"use client";

/**
 * `/secrets/deploy-keys` — scoped machine credentials for CI and remote environments
 * (docs/requests/REQ-125, slice 3).
 *
 * A deployment key is the one identity in Omnion that exists to be pasted into a CI secret store
 * and forgotten, so the screen is designed around three facts rather than around the row:
 *
 * 1. **It will leak.** A key in a pipeline config is copied, forked, logged by a runner that
 *    nobody reads and pasted into a wiki. So the value is shown exactly once, the row keeps a
 *    hash, and the create drawer says so before you mint, not after.
 * 2. **A key that never expires is a liability, not a convenience.** The API requires an expiry and
 *    the form does too — an operator cannot create a permanent key here even by accident.
 * 3. **A key can lease inside its environment and nowhere else.** It cannot reveal, cannot list
 *    values and cannot cross environments; the scope list is what it is, and a refusal is the same
 *    error wherever it happens, so a caller cannot probe for the difference.
 *
 * The use log is therefore not a nicety: it is the only way an operator finds out a key they
 * thought was dead is being used somewhere.
 *
 * Keyboard: `/` focuses search, `n` mints a key, `Esc` closes a dialog. Under `sm:` the table
 * becomes cards and the drawer is one column.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  Check,
  Copy,
  KeyRound,
  LogOut,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createDeploymentKey,
  deleteDeploymentKey,
  fetchDeploymentKeyUses,
  fetchDeploymentKeys,
  revokeDeploymentKey,
  type CreatedDeploymentKey,
  type DeploymentKey,
  type DeploymentKeysResponse,
  type DeploymentKeyUse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The scopes a key may be granted. `family.*` covers a whole family. */
const SCOPE_CHOICES = [
  { value: "secrets.lease", label: "Lease secrets", hint: "issue a lease over a secret in scope" },
  { value: "secrets.*", label: "Every secret scope", hint: "the wildcard — use sparingly" },
  { value: "deployments.*", label: "Deployments", hint: "record a deployment for the environment" },
  { value: "health.read", label: "Health read", hint: "read the environment's health" },
];

/** Sensible environments; the API accepts any name, so this is a starting point not a limit. */
const ENVIRONMENTS = ["production", "staging", "development"];

const STATE_TONE: Record<string, string> = {
  active: "bg-positive-soft text-positive",
  revoked: "bg-caution-soft text-caution",
  expired: "bg-quiet-soft text-muted",
};

/** `30 days` / `6 h` / `expired` — the expiry, in a form a person can scan. */
function until(seconds: number): string {
  if (seconds <= 0) return "expired";
  if (seconds < 3600) return `${Math.max(Math.floor(seconds / 60), 1)} min`;
  const hours = Math.floor(seconds / 3600);
  if (hours < 24) return `${hours} h`;
  const days = Math.floor(hours / 24);
  return `${days} day${days === 1 ? "" : "s"}`;
}

/** `/secrets/deploy-keys`. */
export function DeployKeysView() {
  const [state, setState] = useState<DeploymentKeysResponse | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [needle, setNeedle] = useState("");
  const [minting, setMinting] = useState(false);
  const [minted, setMinted] = useState<CreatedDeploymentKey | null>(null);
  const [acting, setActing] = useState<DeploymentKey | null>(null);
  const [inspecting, setInspecting] = useState<DeploymentKey | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const search = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await fetchDeploymentKeys();
      setState(next);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "network", message: "The deployment keys could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT" ||
        target?.isContentEditable;
      if (event.key === "Escape") {
        if (minted) {
          setMinted(null);
          return;
        }
        if (minting) {
          setMinting(false);
          return;
        }
        if (acting) {
          setActing(null);
          return;
        }
        if (inspecting) {
          setInspecting(null);
          return;
        }
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        search.current?.focus();
      }
      if (event.key === "n") {
        event.preventDefault();
        setNotice(null);
        setMinting(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [minted, minting, acting, inspecting]);

  const rows = useMemo(() => {
    const all = state?.keys ?? [];
    const query = needle.trim().toLowerCase();
    if (!query) return all;
    return all.filter((key) =>
      [key.name, key.environment, key.key_prefix, ...key.scopes]
        .filter(Boolean)
        .some((value) => value.toLowerCase().includes(query)),
    );
  }, [state, needle]);

  if (status === "loading") return <LoadingTable columns={6} />;

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-deploykeys-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <section
        data-deploykeys-summary
        className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <Stat label="Keys" value={state?.total ?? 0} />
        <Stat label="Active" value={state?.active ?? 0} />
        <Stat label="Expiring" value={state?.expired ?? 0} />
        <Stat label="Revoked" value={state?.revoked ?? 0} />
        <div className="flex flex-1 items-center justify-end gap-2">
          <div className="relative">
            <Search
              className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted"
              aria-hidden
            />
            <input
              ref={search}
              value={needle}
              onChange={(event) => setNeedle(event.target.value)}
              placeholder="Search keys — press /"
              aria-label="Search deployment keys"
              data-deploykeys-search
              className="h-8 w-56 rounded-lg border border-line bg-surface pl-8 pr-2 text-[12.5px] outline-none focus:border-accent"
            />
          </div>
          <button
            type="button"
            onClick={() => void load()}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            onClick={() => {
              setNotice(null);
              setMinting(true);
            }}
            data-deploykeys-mint
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
          >
            <KeyRound className="size-3.5" aria-hidden />
            Mint a key
          </button>
        </div>
      </section>

      {notice ? (
        <p
          role="status"
          data-deploykeys-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      {rows.length === 0 ? (
        <EmptyState
          title={needle ? "No key matches that search" : "No deployment key exists yet"}
          hint={
            needle
              ? "The search covers the name, the environment, the prefix and the scopes."
              : "A deployment key lets a CI pipeline lease a credential inside one environment. It can never reveal, never list values and never cross the environment it was minted for — and a deploy in that environment revokes everything it leased."
          }
          action={
            needle ? null : (
              <button
                type="button"
                onClick={() => setMinting(true)}
                data-deploykeys-empty-mint
                className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel"
              >
                <KeyRound className="size-3.5" aria-hidden />
                Mint the first key
              </button>
            )
          }
        />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line bg-surface">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th className="px-4 py-2 font-medium">Name</th>
                <th className="px-4 py-2 font-medium">Environment</th>
                <th className="px-4 py-2 font-medium">Scopes</th>
                <th className="px-4 py-2 font-medium">State</th>
                <th className="px-4 py-2 font-medium">Uses</th>
                <th className="px-4 py-2 font-medium">Expires</th>
                <th className="px-4 py-2" />
              </tr>
            </thead>
            <tbody>
              {rows.map((key) => (
                <KeyRow
                  key={key.id}
                  entry={key}
                  onInspect={() => {
                    setNotice(null);
                    setInspecting(key);
                  }}
                  onAct={() => {
                    setNotice(null);
                    setActing(key);
                  }}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {minted ? (
        <MintedDialog
          minted={minted}
          header={state?.header ?? "x-omnion-deployment-key"}
          onClose={() => setMinted(null)}
        />
      ) : null}

      {minting ? (
        <MintDialog
          onClose={() => setMinting(false)}
          onMinted={async (created) => {
            setMinting(false);
            setMinted(created);
            await load();
          }}
        />
      ) : null}

      {acting ? (
        <ActDialog
          entry={acting}
          onClose={() => setActing(null)}
          onDone={async (message) => {
            setActing(null);
            setNotice(message);
            await load();
          }}
        />
      ) : null}

      {inspecting ? (
        <UseLogDialog entry={inspecting} onClose={() => setInspecting(null)} />
      ) : null}
    </div>
  );
}

/** A counter in the header strip. */
function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="flex flex-col">
      <span className="text-[11px] text-muted">{label}</span>
      <span className="text-[15px] font-medium tabular-nums">{value}</span>
    </div>
  );
}

/** One key row. */
function KeyRow({
  entry,
  onInspect,
  onAct,
}: {
  entry: DeploymentKey;
  onInspect: () => void;
  onAct: () => void;
}) {
  const tone = STATE_TONE[entry.state] ?? "bg-quiet-soft text-muted";
  return (
    <tr data-deploykey-row data-deploykey-state={entry.state} className="border-b border-line last:border-0">
      <td className="px-4 py-2.5">
        <span className="font-medium">{entry.name}</span>
        <span className="ml-2 font-mono text-[11px] text-muted">{entry.key_prefix}…</span>
      </td>
      <td className="px-4 py-2.5">{entry.environment}</td>
      <td className="px-4 py-2.5">
        <span className="flex flex-wrap gap-1">
          {entry.scopes.map((scope) => (
            <span
              key={scope}
              data-deploykey-scope
              className="rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11px] text-muted"
            >
              {scope}
            </span>
          ))}
        </span>
      </td>
      <td className="px-4 py-2.5">
        <span
          className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${tone}`}
        >
          {entry.state}
        </span>
        {entry.revoke_reason ? (
          <span className="mt-1 block max-w-xs text-[11.5px] text-muted">{entry.revoke_reason}</span>
        ) : null}
      </td>
      <td className="px-4 py-2.5 tabular-nums text-muted">
        {entry.uses}
        {entry.last_used_at ? (
          <span className="block text-[11px]">{formatTimestamp(entry.last_used_at)}</span>
        ) : (
          <span className="block text-[11px]">never</span>
        )}
      </td>
      <td className="px-4 py-2.5 tabular-nums text-muted">
        {entry.state === "active" ? until(entry.expires_in_seconds) : "—"}
      </td>
      <td className="px-4 py-2.5">
        <span className="flex items-center justify-end gap-1.5">
          <button
            type="button"
            onClick={onInspect}
            data-deploykey-uses={entry.id}
            className="h-7 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-panel"
          >
            Use log
          </button>
          {entry.state === "active" ? (
            <button
              type="button"
              onClick={onAct}
              data-deploykey-revoke={entry.id}
              className="inline-flex h-7 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-panel"
            >
              <LogOut className="size-3.5" aria-hidden />
              Revoke
            </button>
          ) : entry.deletable ? (
            <button
              type="button"
              onClick={onAct}
              data-deploykey-delete={entry.id}
              className="inline-flex h-7 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-panel"
            >
              <Trash2 className="size-3.5" aria-hidden />
              Delete
            </button>
          ) : null}
        </span>
      </td>
    </tr>
  );
}

/** The one-time value panel. Same rule as the gateway keys (REQ-040): it exists exactly once. */
function MintedDialog({
  minted,
  header,
  onClose,
}: {
  minted: CreatedDeploymentKey;
  header: string;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(minted.value);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[8vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="deploykey-minted-title"
      data-deploykey-minted
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="deploykey-minted-title"
          ref={heading}
          tabIndex={-1}
          className="flex items-center gap-2 text-[15px] font-medium outline-none"
        >
          <ShieldCheck className="size-4 text-positive" aria-hidden />
          {minted.name} is minted
        </h3>
        <p className="mt-1.5 flex items-start gap-2 text-[12.5px] text-caution">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          This is the only time the value is shown. Omnion keeps a hash and cannot read it back —
          store it in your pipeline's secret store now, and mint a new key if you lose it.
        </p>
        <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-[12.5px]">
          <dt className="text-muted">Header</dt>
          <dd className="font-mono">{header}</dd>
          <dt className="text-muted">Environment</dt>
          <dd>{minted.environment}</dd>
          <dt className="text-muted">Scopes</dt>
          <dd className="font-mono">{minted.scopes.join(", ")}</dd>
          <dt className="text-muted">Expires</dt>
          <dd>{formatTimestamp(minted.expires_at)}</dd>
          <dt className="text-muted">Fingerprint</dt>
          <dd className="font-mono text-[11.5px] text-muted">{minted.fingerprint}</dd>
        </dl>
        <div className="mt-3 flex items-center gap-2">
          <code
            data-deploykey-value
            className="block flex-1 overflow-x-auto rounded-lg border border-line bg-quiet-soft px-2.5 py-2 font-mono text-[12px]"
          >
            {minted.value}
          </code>
          <button
            type="button"
            onClick={() => void copy()}
            data-deploykey-copy
            className="flex h-8 shrink-0 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-panel"
          >
            {copied ? <Check className="size-3.5" aria-hidden /> : <Copy className="size-3.5" aria-hidden />}
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={onClose}
            data-deploykey-minted-done
            className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            I stored it
          </button>
        </div>
      </div>
    </div>
  );
}

/** The mint form. An expiry is required, and the scopes are an explicit choice. */
function MintDialog({
  onClose,
  onMinted,
}: {
  onClose: () => void;
  onMinted: (created: CreatedDeploymentKey) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [environment, setEnvironment] = useState(ENVIRONMENTS[0]);
  const [scopes, setScopes] = useState<string[]>([SCOPE_CHOICES[0].value]);
  const [expiresAt, setExpiresAt] = useState(() => {
    const when = new Date(Date.now() + 30 * 24 * 3600 * 1000);
    return when.toISOString().slice(0, 10);
  });
  const [allowedIps, setAllowedIps] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const submit = async () => {
    if (!name.trim()) {
      setError("Name the key after the pipeline that will hold it.");
      return;
    }
    if (scopes.length === 0) {
      setError("A key with no scope can do nothing — choose at least one.");
      return;
    }
    if (!expiresAt) {
      setError("A deployment key must expire. Pick a date.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await createDeploymentKey({
        name: name.trim(),
        environment,
        scopes,
        // The form works in whole days; the API wants an instant, so the end of that day.
        expiresAt: new Date(`${expiresAt}T23:59:59Z`).toISOString(),
        allowedIps: allowedIps.trim() || null,
      });
      await onMinted(created);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The key could not be minted.");
      setBusy(false);
    }
  };

  const toggle = (scope: string) =>
    setScopes((current) =>
      current.includes(scope)
        ? current.filter((entry) => entry !== scope)
        : [...current, scope],
    );

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[8vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="deploykey-mint-title"
      data-deploykey-drawer
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="deploykey-mint-title"
          ref={heading}
          tabIndex={-1}
          className="text-[15px] font-medium outline-none"
        >
          Mint a deployment key
        </h3>
        <p className="mt-1 text-[12.5px] text-muted">
          The value is shown once, in the panel that appears after this. Everything else is
          metadata; the row keeps a hash.
        </p>

        <label className="mt-4 block text-[12px] text-muted" htmlFor="deploykey-name">
          Name — the pipeline that will hold it
        </label>
        <input
          id="deploykey-name"
          value={name}
          onChange={(event) => setName(event.target.value)}
          data-deploykey-name
          placeholder="release · nightly-deploy"
          className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
        />

        <label className="mt-3 block text-[12px] text-muted" htmlFor="deploykey-env">
          Environment — the only one this key can ever lease in
        </label>
        <input
          id="deploykey-env"
          list="deploykey-env-choices"
          value={environment}
          onChange={(event) => setEnvironment(event.target.value)}
          data-deploykey-environment
          className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
        />
        <datalist id="deploykey-env-choices">
          {ENVIRONMENTS.map((value) => (
            <option key={value} value={value} />
          ))}
        </datalist>

        <fieldset className="mt-3">
          <legend className="text-[12px] text-muted">Scopes — least privilege first</legend>
          <ul className="mt-1.5 flex flex-col gap-1.5">
            {SCOPE_CHOICES.map((choice) => (
              <li key={choice.value}>
                <label className="flex items-start gap-2 text-[12.5px]">
                  <input
                    type="checkbox"
                    checked={scopes.includes(choice.value)}
                    onChange={() => toggle(choice.value)}
                    data-deploykey-scope-choice={choice.value}
                    className="mt-0.5"
                  />
                  <span>
                    <span className="font-mono">{choice.value}</span>
                    <span className="block text-[11.5px] text-muted">{choice.hint}</span>
                  </span>
                </label>
              </li>
            ))}
          </ul>
        </fieldset>

        <div className="mt-3 grid gap-3 sm:grid-cols-2">
          <div>
            <label className="block text-[12px] text-muted" htmlFor="deploykey-expiry">
              Expires — required
            </label>
            <input
              id="deploykey-expiry"
              type="date"
              value={expiresAt}
              onChange={(event) => setExpiresAt(event.target.value)}
              data-deploykey-expiry
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
            />
          </div>
          <div>
            <label className="block text-[12px] text-muted" htmlFor="deploykey-ips">
              Address allow-list (optional)
            </label>
            <input
              id="deploykey-ips"
              value={allowedIps}
              onChange={(event) => setAllowedIps(event.target.value)}
              data-deploykey-ips
              placeholder="203.0.113.7, 10.0.0.0/8"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
            />
          </div>
        </div>

        {error ? (
          <p role="alert" data-deploykey-drawer-error className="mt-2 text-[12px] text-caution">
            {error}
          </p>
        ) : null}

        <div className="mt-4 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-deploykey-save
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-3 text-[12.5px] transition hover:bg-panel disabled:opacity-60"
          >
            <KeyRound className="size-3.5" aria-hidden />
            {busy ? "Minting…" : "Mint it"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Revoke a live key, or delete a dead one. Two different consequences, so two different words. */
function ActDialog({
  entry,
  onClose,
  onDone,
}: {
  entry: DeploymentKey;
  onClose: () => void;
  onDone: (message: string) => Promise<void>;
}) {
  const [reason, setReason] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const heading = useRef<HTMLHeadingElement | null>(null);
  const deleting = entry.state !== "active";

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const submit = async () => {
    if (!deleting && !reason.trim()) {
      setError("Say why the key is being taken back — the reason is kept in the audit.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (deleting) {
        await deleteDeploymentKey(entry.id);
        await onDone(`${entry.name} is deleted. The value was never stored, so nothing else to clean.`);
      } else {
        await revokeDeploymentKey(entry.id, reason.trim());
        await onDone(
          `${entry.name} is revoked. Every lease it minted is revoked with it, and a pipeline still holding the value gets a denial.`,
        );
      }
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The key could not be changed.");
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[10vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="deploykey-act-title"
      data-deploykey-dialog
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-md rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="deploykey-act-title"
          ref={heading}
          tabIndex={-1}
          className="flex items-center gap-2 text-[15px] font-medium outline-none"
        >
          {deleting ? <Trash2 className="size-4 text-muted" aria-hidden /> : <LogOut className="size-4 text-caution" aria-hidden />}
          {deleting ? "Delete this key's record" : "Revoke this key"}
        </h3>
        <p className="mt-1.5 text-[12.5px] text-muted">
          {deleting ? (
            <>
              <span className="font-medium">{entry.name}</span> is {entry.state}, so its record can go.
              The use log is removed with it. This is the only case where a key row is ever
              removed — a live key cannot be deleted, only revoked.
            </>
          ) : (
            <>
              <span className="font-medium">{entry.name}</span> is bound to{" "}
              <span className="font-medium">{entry.environment}</span> and has been presented{" "}
              {entry.uses} time{entry.uses === 1 ? "" : "s"}. Revoking it also revokes every lease
              it minted, so a running pipeline fails on its next lease rather than continuing on a
              credential you meant to replace.
            </>
          )}
        </p>
        {deleting ? null : (
          <>
            <label className="mt-4 block text-[12px] text-muted" htmlFor="deploykey-reason">
              Reason (kept on the row and in the audit)
            </label>
            <input
              id="deploykey-reason"
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              data-deploykey-reason-input
              placeholder="pipeline retired, leaked in a log, rotated…"
              className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 text-[13px] outline-none focus:border-accent"
            />
          </>
        )}
        {error ? (
          <p role="alert" data-deploykey-dialog-error className="mt-2 text-[12px] text-caution">
            {error}
          </p>
        ) : null}
        <div className="mt-4 flex items-center justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-deploykey-act-confirm
            className={`flex h-8 items-center gap-1.5 rounded-lg border px-3 text-[12.5px] transition disabled:opacity-60 ${
              deleting
                ? "border-line bg-surface hover:bg-panel"
                : "border-danger/50 bg-danger-soft text-caution"
            }`}
          >
            {busy ? "Working…" : deleting ? "Delete the record" : "Revoke the key"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The use log. Denials are first-class here: a key used where it should not be, is the finding. */
function UseLogDialog({ entry, onClose }: { entry: DeploymentKey; onClose: () => void }) {
  const [uses, setUses] = useState<DeploymentKeyUse[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const heading = useRef<HTMLHeadingElement | null>(null);

  useEffect(() => {
    heading.current?.focus();
    let live = true;
    fetchDeploymentKeyUses(entry.id)
      .then((answer) => {
        if (live) setUses(answer.uses);
      })
      .catch((cause) => {
        if (live) {
          setError(cause instanceof ApiError ? cause.message : "The use log could not be read.");
        }
      });
    return () => {
      live = false;
    };
  }, [entry.id]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[8vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="deploykey-log-title"
      data-deploykey-log
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-2xl rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="deploykey-log-title"
          ref={heading}
          tabIndex={-1}
          className="text-[15px] font-medium outline-none"
        >
          Use log — {entry.name}
        </h3>
        <p className="mt-1 text-[12.5px] text-muted">
          Every presentation of <span className="font-mono">{entry.key_prefix}…</span> with the
          pipeline identity as presented, the address it came from and the result. A denial is
          logged too: a key reaching for a lease it was refused is the finding worth having.
        </p>

        {error ? (
          <p role="alert" data-deploykey-log-error className="mt-3 text-[12px] text-caution">
            {error}
          </p>
        ) : null}

        {uses === null && !error ? (
          <p className="mt-4 text-[12.5px] text-muted">Reading the log…</p>
        ) : uses && uses.length === 0 ? (
          <p data-deploykey-log-empty className="mt-4 text-[12.5px] text-muted">
            This key has never been presented. A key nothing uses is either not deployed yet or a
            candidate for deletion once it expires.
          </p>
        ) : uses ? (
          <div className="mt-3 max-h-[50vh] overflow-y-auto rounded-lg border border-line">
            <table className="w-full border-collapse text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11px] text-muted">
                  <th className="px-3 py-1.5 font-medium">When</th>
                  <th className="px-3 py-1.5 font-medium">Action</th>
                  <th className="px-3 py-1.5 font-medium">Identity</th>
                  <th className="px-3 py-1.5 font-medium">Address</th>
                  <th className="px-3 py-1.5 font-medium">Result</th>
                </tr>
              </thead>
              <tbody>
                {uses.map((use, index) => (
                  <tr
                    key={`${use.created_at}-${index}`}
                    data-deploykey-use
                    data-deploykey-use-result={use.result}
                    className="border-b border-line last:border-0"
                  >
                    <td className="px-3 py-1.5 text-muted">{formatTimestamp(use.created_at)}</td>
                    <td className="px-3 py-1.5 font-mono text-[11.5px]">{use.action}</td>
                    <td className="px-3 py-1.5 font-mono text-[11.5px]">{use.identity}</td>
                    <td className="px-3 py-1.5 font-mono text-[11.5px] text-muted">
                      {use.address ?? "—"}
                    </td>
                    <td
                      className={`px-3 py-1.5 ${
                        use.result === "ok" ? "text-positive" : "text-caution"
                      }`}
                    >
                      {use.result}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : null}

        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={onClose}
            className="h-8 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
          >
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
