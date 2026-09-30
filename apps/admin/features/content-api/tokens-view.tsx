"use client";

/**
 * `/content-api` — the Tokens tab (REQ-019, slice 1).
 *
 * The one screen in this slice whose job is to be *careful*, because the thing it lists is a
 * credential that leaves the building. Four decisions carry that:
 *
 * 1. **The plaintext is shown once, behind a checkbox.** The server returns it exactly once and
 *    stores only its digest, so this dialog cannot offer "show again" — there is nothing to show.
 *    The `I stored it` checkbox is therefore a real gate rather than a ritual: `Done` is disabled
 *    until it is ticked, because the alternative is a person closing the dialog and discovering
 *    hours later that they cannot recover the value.
 *
 * 2. **The list shows a prefix, and the prefix is the point.** `omn_xxxxxxxx` is what somebody
 *    types into "which token is this?" — the secret half is useless without its prefix, and the
 *    copy button copies the prefix so a rotation conversation can be had without pasting a
 *    credential into a chat.
 *
 * 3. **Rotate and revoke are different buttons with different words.** Rotation says in advance
 *    that the previous secret stops working *now* — an integration that has not been updated will
 *    start failing within the minute, and the only fair thing is to say so before the click.
 *    Revoke says the token is dead for good.
 *
 * 4. **Status is the server's word, not this screen's arithmetic.** `active` / `expired` /
 *    `revoked` arrive from the API, which computes them from the same columns it authenticates
 *    against. A panel that recomputed "expired" from a locally formatted date would eventually
 *    disagree with the store about the one row that matters.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  Check,
  Copy,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  RotateCw,
  Search,
  ShieldOff,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  createContentApiToken,
  fetchContentApiTokens,
  fetchContentApiVocabulary,
  revokeContentApiToken,
  rotateContentApiToken,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { ContentApiToken, ContentApiVocabulary, CreatedContentApiToken } from "@/lib/types";

/** A relative "in 89 days" beside the exact stamp, so a 30-day token is obvious at a glance. */
function relativeExpiry(iso: string | null): string {
  if (iso === null) return "never";
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "—";
  const days = Math.round((then - Date.now()) / 86_400_000);
  if (days < 0) return "expired";
  if (days === 0) return "today";
  if (days === 1) return "tomorrow";
  return `in ${days} days`;
}

function statusClass(status: ContentApiToken["status"]): string {
  if (status === "active") return "text-[11px] rounded border border-line px-1.5 py-0.5";
  if (status === "expired")
    return "text-[11px] rounded border border-amber-500/40 px-1.5 py-0.5 text-amber-700 dark:text-amber-400";
  return "text-[11px] rounded border border-red-500/40 px-1.5 py-0.5 text-red-700 dark:text-red-400";
}

export function ContentApiTokensView() {
  const [tokens, setTokens] = useState<ContentApiToken[] | null>(null);
  const [vocabulary, setVocabulary] = useState<ContentApiVocabulary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState<"all" | ContentApiToken["status"]>("all");
  const [busyId, setBusyId] = useState<string | null>(null);
  /** The one dialog that shows a plaintext, shared by create and rotate. */
  const [revealed, setRevealed] = useState<CreatedContentApiToken | null>(null);
  const [pendingRevoke, setPendingRevoke] = useState<ContentApiToken | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const [rows, words] = await Promise.all([
        fetchContentApiTokens(),
        fetchContentApiVocabulary(),
      ]);
      setTokens(rows);
      setVocabulary(words);
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const visible = useMemo(() => {
    if (tokens === null) return null;
    const needle = search.trim().toLowerCase();
    return tokens.filter((token) => {
      if (statusFilter !== "all" && token.status !== statusFilter) return false;
      if (needle.length === 0) return true;
      return (
        token.name.toLowerCase().includes(needle) ||
        token.prefix.toLowerCase().includes(needle)
      );
    });
  }, [tokens, search, statusFilter]);

  const rotate = useCallback(
    async (token: ContentApiToken) => {
      setBusyId(token.id);
      setError(null);
      setNotice(null);
      try {
        const rotated = await rotateContentApiToken(token.id);
        setRevealed(rotated);
        setNotice(
          `Rotated ${token.name}. The previous secret stopped working immediately — update the integration before you close this.`,
        );
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  const revoke = useCallback(
    async (token: ContentApiToken) => {
      setBusyId(token.id);
      setError(null);
      setNotice(null);
      try {
        await revokeContentApiToken(token.id);
        setNotice(
          `Revoked ${token.name}. Anything still calling with it now gets 401 token_revoked.`,
        );
        setPendingRevoke(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
        setPendingRevoke(null);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  if (error !== null && tokens === null) {
    return <ErrorStrip message={error} onRetry={() => void load()} />;
  }
  if (tokens === null) return <TokensSkeleton />;

  return (
    <div className="space-y-6" data-content-api-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          {tokens.length === 0
            ? "Tokens are read-only credentials for the headless content API. None minted yet."
            : `${tokens.length} token${tokens.length === 1 ? "" : "s"}. Only the prefix is ever shown again.`}
        </p>
        <div className="flex flex-wrap gap-2">
          <label className="flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5">
            <Search className="h-3.5 w-3.5 text-muted" aria-hidden />
            <span className="sr-only">Search tokens</span>
            <input
              data-content-api-search
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Name or prefix"
              className="w-36 bg-transparent text-[12.5px] outline-none"
            />
          </label>
          <select
            data-content-api-status-filter
            value={statusFilter}
            onChange={(event) => setStatusFilter(event.target.value as typeof statusFilter)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[12.5px]"
          >
            <option value="all">All statuses</option>
            <option value="active">Active</option>
            <option value="expired">Expired</option>
            <option value="revoked">Revoked</option>
          </select>
          <button
            type="button"
            data-content-api-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-content-api-create
            onClick={() => {
              setCreating((value) => !value);
              setError(null);
            }}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            Create token
          </button>
        </div>
      </div>

      {error !== null ? <ErrorStrip message={error} onRetry={() => void load()} /> : null}
      {notice !== null ? (
        <p data-content-api-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}

      {creating && vocabulary !== null ? (
        <CreateTokenForm
          vocabulary={vocabulary}
          onCancel={() => setCreating(false)}
          onCreated={async (created) => {
            setCreating(false);
            setRevealed(created);
            await load();
          }}
          onError={setError}
        />
      ) : null}

      {tokens.length === 0 ? (
        <EmptyState
          title="No content API tokens yet"
          hint="A token is how a headless frontend reads published content. Create one and it is shown to you exactly once."
        />
      ) : visible !== null && visible.length === 0 ? (
        <EmptyState
          title="No token matches this filter"
          hint="Clear the search or the status filter to see the ones you did not ask for."
        />
      ) : (
        <TokenTable
          tokens={visible ?? []}
          busyId={busyId}
          onRotate={rotate}
          onRevoke={setPendingRevoke}
        />
      )}

      {revealed !== null ? (
        <PlaintextDialog created={revealed} onClose={() => setRevealed(null)} />
      ) : null}

      {pendingRevoke !== null ? (
        <RevokeDialog
          token={pendingRevoke}
          busy={busyId === pendingRevoke.id}
          onCancel={() => setPendingRevoke(null)}
          onConfirm={() => void revoke(pendingRevoke)}
        />
      ) : null}
    </div>
  );
}

function TokenTable({
  tokens,
  busyId,
  onRotate,
  onRevoke,
}: {
  tokens: ContentApiToken[];
  busyId: string | null;
  onRotate: (token: ContentApiToken) => void;
  onRevoke: (token: ContentApiToken) => void;
}) {
  return (
    <div className="overflow-x-auto rounded-lg border border-line">
      <table data-content-api-table className="w-full min-w-[760px] text-left text-[12.5px]">
        <thead className="text-[11px] uppercase tracking-wide text-muted">
          <tr>
            <th scope="col" className="px-3 py-2 font-medium">Name</th>
            <th scope="col" className="px-3 py-2 font-medium">Prefix</th>
            <th scope="col" className="px-3 py-2 font-medium">Site scope</th>
            <th scope="col" className="px-3 py-2 font-medium">Scopes</th>
            <th scope="col" className="px-3 py-2 font-medium">Last used</th>
            <th scope="col" className="px-3 py-2 font-medium">Expires</th>
            <th scope="col" className="px-3 py-2 font-medium">Status</th>
            <th scope="col" className="px-3 py-2 font-medium">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {tokens.map((token) => (
            <tr key={token.id} data-content-api-row={token.id} className="border-t border-line">
              <td className="px-3 py-2.5">{token.name}</td>
              <td className="px-3 py-2.5">
                <span className="flex items-center gap-1.5">
                  <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11.5px]">
                    {token.prefix}
                  </code>
                  <CopyButton value={token.prefix} label={token.name} />
                </span>
              </td>
              <td className="px-3 py-2.5">{token.site_key ?? "All sites"}</td>
              <td className="px-3 py-2.5">
                <span className="flex flex-wrap gap-1">
                  {token.scopes.map((scope) => (
                    <span
                      key={scope}
                      className="rounded border border-line px-1.5 py-0.5 text-[11px]"
                    >
                      {scope}
                    </span>
                  ))}
                </span>
              </td>
              <td className="px-3 py-2.5 text-muted">
                {token.last_used_at ? formatTimestamp(token.last_used_at) : "never"}
              </td>
              <td className="px-3 py-2.5 text-muted" title={token.expires_at ?? undefined}>
                {relativeExpiry(token.expires_at)}
              </td>
              <td className="px-3 py-2.5">
                <span className={statusClass(token.status)}>{token.status}</span>
              </td>
              <td className="px-3 py-2.5">
                <span className="flex justify-end gap-1.5">
                  <button
                    type="button"
                    data-content-api-rotate={token.id}
                    disabled={busyId === token.id || token.status === "revoked"}
                    onClick={() => onRotate(token)}
                    className="inline-flex items-center gap-1 rounded border border-line px-2 py-1 disabled:opacity-40"
                    title="Issue a new secret. The previous one stops working immediately."
                  >
                    {busyId === token.id ? (
                      <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
                    ) : (
                      <RotateCw className="h-3 w-3" aria-hidden />
                    )}
                    Rotate
                  </button>
                  <button
                    type="button"
                    data-content-api-revoke={token.id}
                    disabled={busyId === token.id || token.status === "revoked"}
                    onClick={() => onRevoke(token)}
                    className="inline-flex items-center gap-1 rounded border border-line px-2 py-1 disabled:opacity-40"
                    title="Revoke for good. The token stays in the list, marked revoked."
                  >
                    <ShieldOff className="h-3 w-3" aria-hidden />
                    Revoke
                  </button>
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Copy a value and say so, then stop saying so. */
function CopyButton({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      data-copy={label}
      onClick={() => {
        void navigator.clipboard?.writeText(value);
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1500);
      }}
      className="text-muted"
      aria-label={`Copy the prefix of ${label}`}
    >
      {copied ? (
        <Check className="h-3.5 w-3.5" aria-hidden />
      ) : (
        <Copy className="h-3.5 w-3.5" aria-hidden />
      )}
    </button>
  );
}

function CreateTokenForm({
  vocabulary,
  onCancel,
  onCreated,
  onError,
}: {
  vocabulary: ContentApiVocabulary;
  onCancel: () => void;
  onCreated: (created: CreatedContentApiToken) => Promise<void>;
  onError: (message: string | null) => void;
}) {
  const [name, setName] = useState("");
  const [siteScope, setSiteScope] = useState<"all" | "site">("all");
  const [scopes, setScopes] = useState<string[]>([vocabulary.scopes[0] ?? "content:read"]);
  const [origins, setOrigins] = useState("");
  const [expiryDays, setExpiryDays] = useState(90);
  const [tier, setTier] = useState(vocabulary.rate_tiers[0]?.per_minute ?? 120);
  const [busy, setBusy] = useState(false);

  const submit = useCallback(async () => {
    setBusy(true);
    onError(null);
    try {
      const created = await createContentApiToken({
        name: name.trim(),
        scopes,
        allowed_origins: origins
          .split("\n")
          .map((line) => line.trim())
          .filter((line) => line.length > 0),
        rate_limit_per_minute: tier,
        expires_in_days: expiryDays,
      });
      await onCreated(created);
    } catch (caught) {
      onError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [name, scopes, origins, tier, expiryDays, onCreated, onError]);

  return (
    <form
      data-content-api-form
      className="space-y-4 rounded-lg border border-line p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Name</span>
          <input
            data-content-api-form-name
            required
            maxLength={vocabulary.max_name_length}
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="Frontend"
            className="rounded border border-line bg-transparent px-2 py-1.5"
          />
          <span className="text-[11px] text-muted">
            Unique in this organization, ignoring case.
          </span>
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Site scope</span>
          <select
            data-content-api-form-site
            value={siteScope}
            onChange={(event) => setSiteScope(event.target.value as typeof siteScope)}
            className="rounded border border-line bg-transparent px-2 py-1.5"
          >
            <option value="all">All sites</option>
            <option value="site">One site (pick below)</option>
          </select>
          {siteScope === "site" ? (
            <span className="text-[11px] text-muted">
              A site-scoped token is created from that site&apos;s own screen, where the site is
              already known.
            </span>
          ) : null}
        </label>
      </div>

      <fieldset className="space-y-1.5">
        <legend className="text-[12px] text-muted">Scopes</legend>
        <div className="flex flex-wrap gap-3">
          {vocabulary.scopes.map((scope) => (
            <label key={scope} className="flex items-center gap-1.5 text-[12px]">
              <input
                type="checkbox"
                data-content-api-scope={scope}
                checked={scopes.includes(scope)}
                onChange={(event) =>
                  setScopes((current) =>
                    event.target.checked
                      ? [...current, scope]
                      : current.filter((value) => value !== scope),
                  )
                }
              />
              {scope}
            </label>
          ))}
        </div>
        {vocabulary.reserved_scopes.length > 0 ? (
          <p className="text-[11px] text-muted">
            {vocabulary.reserved_scopes.map((reserved) => reserved.note).join(" ")}
          </p>
        ) : null}
      </fieldset>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Expiry</span>
          <select
            data-content-api-form-expiry
            value={expiryDays}
            onChange={(event) => setExpiryDays(Number(event.target.value))}
            className="rounded border border-line bg-transparent px-2 py-1.5"
          >
            {vocabulary.expiry_presets.map((preset) => (
              <option key={preset.days} value={preset.days}>
                {preset.label}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Rate limit</span>
          <select
            data-content-api-form-tier
            value={tier}
            onChange={(event) => setTier(Number(event.target.value))}
            className="rounded border border-line bg-transparent px-2 py-1.5"
          >
            {vocabulary.rate_tiers.map((rate) => (
              <option key={rate.per_minute} value={rate.per_minute}>
                {rate.label} — {rate.per_minute}/min
              </option>
            ))}
          </select>
        </label>
      </div>

      <label className="flex flex-col gap-1 text-[12px]">
        <span className="text-muted">Allowed origins (optional, one per line)</span>
        <textarea
          data-content-api-form-origins
          value={origins}
          onChange={(event) => setOrigins(event.target.value)}
          rows={3}
          placeholder={"https://app.example.com\nhttp://localhost:3000"}
          className="rounded border border-line bg-transparent px-2 py-1.5 font-mono text-[11.5px]"
        />
        <span className="text-[11px] text-muted">
          Exact origins only — no path, no trailing slash, no wildcard. Empty means any origin.
        </span>
      </label>

      <div className="flex gap-2">
        <button
          type="submit"
          data-content-api-form-submit
          disabled={busy || scopes.length === 0}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-40"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Create token
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

/**
 * The copy-once dialog.
 *
 * The `I stored it` gate is the load-bearing part: the value is unrecoverable, and a dialog that
 * could be closed without acknowledging that is how somebody discovers it an hour later.
 */
function PlaintextDialog({
  created,
  onClose,
}: {
  created: CreatedContentApiToken;
  onClose: () => void;
}) {
  const [stored, setStored] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <div
      data-content-api-plaintext
      role="dialog"
      aria-modal="true"
      aria-labelledby="content-api-plaintext-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
    >
      <div className="w-full max-w-lg space-y-3 rounded-lg border border-line bg-panel p-5">
        <h2
          id="content-api-plaintext-title"
          className="flex items-center gap-2 text-[14px] font-medium"
        >
          <KeyRound className="h-4 w-4" aria-hidden />
          Copy this token now
        </h2>
        <p className="flex items-start gap-2 text-[12.5px] text-muted">
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
          This is the only time the secret is shown. The server keeps only its SHA-256 digest, so
          it cannot be shown again — if you lose it, rotate the token.
        </p>
        <div className="flex items-center gap-2">
          <code
            data-content-api-plaintext-value
            className="flex-1 break-all rounded bg-quiet-soft px-2 py-1.5 font-mono text-[12px]"
          >
            {created.plaintext}
          </code>
          <button
            type="button"
            data-content-api-plaintext-copy
            onClick={() => {
              void navigator.clipboard?.writeText(created.plaintext);
              setCopied(true);
            }}
            className="inline-flex items-center gap-1 rounded border border-line px-2 py-1.5 text-[12px]"
          >
            {copied ? (
              <Check className="h-3.5 w-3.5" aria-hidden />
            ) : (
              <Copy className="h-3.5 w-3.5" aria-hidden />
            )}
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
        <p className="text-[12px] text-muted">
          Scope: {created.token.scopes.join(", ")} ·{" "}
          {created.token.site_key ?? "all sites"} · {created.token.rate_limit_per_minute}/min
        </p>
        <div className="flex items-center justify-between gap-3">
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              data-content-api-plaintext-stored
              checked={stored}
              onChange={(event) => setStored(event.target.checked)}
            />
            I stored it
          </label>
          <button
            type="button"
            data-content-api-plaintext-done
            disabled={!stored}
            onClick={onClose}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-40"
          >
            Done
          </button>
        </div>
      </div>
    </div>
  );
}

function RevokeDialog({
  token,
  busy,
  onCancel,
  onConfirm,
}: {
  token: ContentApiToken;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      data-content-api-revoke-dialog
      role="dialog"
      aria-modal="true"
      aria-labelledby="content-api-revoke-title"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
    >
      <div className="w-full max-w-md space-y-3 rounded-lg border border-line bg-panel p-5">
        <h2 id="content-api-revoke-title" className="text-[14px] font-medium">
          Revoke {token.name}?
        </h2>
        <p className="text-[12.5px] text-muted">
          Anything still calling with this token will get <code>401 token_revoked</code> on its next
          request. The row stays in the list, marked revoked — revoking is not deleting, so the
          usage history survives.
        </p>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Keep it
          </button>
          <button
            type="button"
            data-content-api-revoke-confirm
            disabled={busy}
            onClick={onConfirm}
            className="rounded-md border border-red-500/50 px-2.5 py-1.5 text-[12.5px] disabled:opacity-40"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
            Revoke
          </button>
        </div>
      </div>
    </div>
  );
}

function TokensSkeleton() {
  return (
    <div className="space-y-3" data-content-api-state="loading" aria-busy="true">
      <div className="h-4 w-56 animate-pulse rounded bg-quiet-soft" />
      {Array.from({ length: 3 }, (_, index) => (
        <div key={index} className="h-14 animate-pulse rounded-lg bg-quiet-soft" />
      ))}
    </div>
  );
}

function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      role="alert"
      className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-red-500/40 px-3 py-2.5 text-[12.5px]"
    >
      <span>{message}</span>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-1.5 rounded border border-line px-2 py-1"
      >
        <RefreshCw className="h-3 w-3" aria-hidden />
        Try again
      </button>
    </div>
  );
}
