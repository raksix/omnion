"use client";

/**
 * `/developer/keys` — the API keys a developer mints to call this platform (REQ-033, slice 1).
 *
 * Four things on this screen are decisions rather than fields, and each is written down because
 * the wrong version of it is a bug nobody reports:
 *
 * - **The secret is shown exactly once, in a dialog that says so.** The API has no endpoint
 *   that can return it again, so the dialog is the only chance. It states the consequence, offers
 *   a copy button with visible confirmation, and refuses to be dismissed by clicking away
 *   without an acknowledgement — because a developer who closes it by accident has lost a
 *   credential they cannot regenerate and has to go and rotate.
 * - **Rotation has no overlap window here, and the confirmation says that in as many words.**
 *   Rotating a key breaks the caller's integration at once. A dialog that merely says "Rotate?"
 *   lets someone click through it on a Friday; one that says "the old secret stops working
 *   immediately" is a decision.
 * - **The scopes are chosen from the permission catalogue, not typed.** A free-text scope box is
 *   how a key ends up holding a permission name that does not exist — which authenticates and
 *   then silently does nothing, or worse, does something.
 * - **The `high` rate tier is offered but marked.** It requires an owner or administrator, and
 *   the API checks the caller's *role*, not the body. A form that offers it to everybody would be
 *   a control that fails on save with a `403`; it is shown disabled with the reason instead.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  Check,
  Copy,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  createApiKey,
  fetchApiKeys,
  revokeApiKey,
  rotateApiKey,
  type CreateApiKeyInput,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { ApiKey } from "@/lib/types";

/**
 * The scopes the picker offers, with the plain-language consequence next to each.
 *
 * A list of permission keys with no descriptions is a list nobody can choose from — "do I need
 * `developer.keys.read`?" is not answerable by reading it. Each line says what holding it means.
 */
const SCOPE_CHOICES: { key: string; label: string }[] = [
  { key: "developer.keys.read", label: "Read keys, usage and the request log" },
  { key: "developer.keys.manage", label: "Create, rotate and revoke keys" },
  { key: "content.pages.read", label: "Read pages" },
  { key: "content.pages.create", label: "Create pages" },
  { key: "content.pages.update", label: "Edit pages" },
  { key: "content.pages.publish", label: "Publish and unpublish pages" },
  { key: "media.read", label: "Read media" },
  { key: "media.upload", label: "Upload media" },
  { key: "users.read", label: "Read users" },
  { key: "sites.read", label: "Read sites" },
];

/** The four expiry choices the request names, and the default it names with them. */
const EXPIRY_CHOICES: { value: string; label: string; days?: number }[] = [
  { value: "90", label: "90 days", days: 90 },
  { value: "30", label: "30 days", days: 30 },
  { value: "365", label: "One year", days: 365 },
  { value: "never", label: "Never" },
];

/** Name bounds, restated from the API so the form refuses before it posts. */
const NAME_MIN = 3;
const NAME_MAX = 60;

/** How many scope chips are shown before the `+N` overflow takes over. */
const SCOPE_CHIP_LIMIT = 2;

/** The form's own state, kept apart from the saved rows. */
type Draft = {
  name: string;
  environment: "live" | "sandbox";
  scopes: string[];
  expiry: string;
  /** One entry per line; the API validates each as a CIDR block. */
  ipAllowlist: string;
  rateTier: "standard" | "high";
};

const EMPTY_DRAFT: Draft = {
  name: "",
  environment: "sandbox",
  scopes: [],
  expiry: "90",
  ipAllowlist: "",
  rateTier: "standard",
};

/** Shorten a scope key for a chip: `content.pages.publish` → `content.pages…`. */
function shortScope(scope: string): string {
  const parts = scope.split(".");
  return parts.length > 2 ? `${parts.slice(0, 2).join(".")}…` : scope;
}

/** Copy to the clipboard, reporting success so the button is not a dead control. */
async function copyToClipboard(value: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    return false;
  }
}

export function DeveloperKeysView() {
  const [keys, setKeys] = useState<ApiKey[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const [formOpen, setFormOpen] = useState(false);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [busy, setBusy] = useState(false);

  /** The one-time secret, held only until the dialog is dismissed. */
  const [minted, setMinted] = useState<{ secret: string; name: string; reason: "created" | "rotated" } | null>(
    null,
  );
  const [mintedCopied, setMintedCopied] = useState(false);
  const [mintedAcknowledged, setMintedAcknowledged] = useState(false);

  /** The key a confirmation is about, and what it would do. */
  const [confirming, setConfirming] = useState<{
    key: ApiKey;
    action: "rotate" | "revoke";
  } | null>(null);
  const [confirmText, setConfirmText] = useState("");

  const [filterEnvironment, setFilterEnvironment] = useState<"all" | "live" | "sandbox">("all");
  const [filterStatus, setFilterStatus] = useState<"all" | ApiKey["status"]>("all");
  const [filterScope, setFilterScope] = useState("all");
  const [filterName, setFilterName] = useState("");

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    fetchApiKeys()
      .then((response) => {
        if (!cancelled) {
          setKeys(response.keys);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setKeys([]);
          setError(
            cause instanceof ApiError ? cause.message : "The API keys could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [reloadToken]);

  /** Every scope that exists in the organization, for the filter's dropdown. */
  const allScopes = useMemo(() => {
    const seen = new Set<string>();
    for (const key of keys ?? []) {
      for (const scope of key.scopes) {
        seen.add(scope);
      }
    }
    return [...seen].sort();
  }, [keys]);

  /**
   * The filtered table.
   *
   * Filtering in the browser rather than round-tripping: the list is one organization's keys —
   * tens of rows, not thousands — and a filter that refetches on every keystroke makes the table
   * flicker while somebody is typing a name. The *log* screen fetches, because that table is
   * genuinely large.
   */
  const visible = useMemo(() => {
    let rows = keys ?? [];
    if (filterEnvironment !== "all") {
      rows = rows.filter((key) => key.environment === filterEnvironment);
    }
    if (filterStatus !== "all") {
      rows = rows.filter((key) => key.status === filterStatus);
    }
    if (filterScope !== "all") {
      rows = rows.filter((key) => key.scopes.includes(filterScope));
    }
    const needle = filterName.trim().toLowerCase();
    if (needle.length > 0) {
      rows = rows.filter(
        (key) =>
          key.name.toLowerCase().includes(needle) || key.prefix.toLowerCase().includes(needle),
      );
    }
    return rows;
  }, [keys, filterEnvironment, filterStatus, filterScope, filterName]);

  const openForm = () => {
    setDraft(EMPTY_DRAFT);
    setFieldError(null);
    setNotice(null);
    setFormOpen(true);
  };

  const toggleScope = (scope: string) => {
    setDraft((current) => ({
      ...current,
      scopes: current.scopes.includes(scope)
        ? current.scopes.filter((entry) => entry !== scope)
        : // Stored sorted so the payload is deterministic — two developers creating the same key
          // from the same clicks should produce byte-identical requests, which makes a diff in a
          // bug report readable.
          [...current.scopes, scope].sort(),
    }));
  };

  const submitForm = async () => {
    const name = draft.name.trim();
    if (name.length < NAME_MIN || name.length > NAME_MAX) {
      setFieldError({
        field: "name",
        message: `A name must be between ${NAME_MIN} and ${NAME_MAX} characters.`,
      });
      return;
    }
    if (draft.scopes.length === 0) {
      setFieldError({
        field: "scopes",
        message: "Choose at least one scope. A key with none could not do anything.",
      });
      return;
    }

    const allowlist = draft.ipAllowlist
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line.length > 0);

    const input: CreateApiKeyInput = {
      name,
      scopes: draft.scopes,
      environment: draft.environment,
      rate_tier: draft.rateTier,
      ...(allowlist.length > 0 ? { ip_allowlist: allowlist } : {}),
    };
    const choice = EXPIRY_CHOICES.find((entry) => entry.value === draft.expiry);
    if (choice?.days !== undefined) {
      input.expires_in_days = choice.days;
    }

    setBusy(true);
    setFieldError(null);
    try {
      const created = await createApiKey(input);
      setFormOpen(false);
      // The dialog is opened from the *response*, and the response is the only place the secret
      // exists. Nothing is cached, nothing is refetched, and closing the dialog is what makes the
      // panel discard it.
      setMinted({ secret: created.secret, name: created.name, reason: "created" });
      setMintedCopied(false);
      setMintedAcknowledged(false);
      reload();
    } catch (cause: unknown) {
      if (cause instanceof ApiError && typeof cause.details?.field === "string") {
        setFieldError({ field: cause.details.field, message: cause.message });
      } else {
        setFieldError({
          field: "form",
          message: cause instanceof ApiError ? cause.message : "The key could not be created.",
        });
      }
    } finally {
      setBusy(false);
    }
  };

  const runConfirmed = async () => {
    if (!confirming) {
      return;
    }
    const { key, action } = confirming;
    setBusy(true);
    setError(null);
    try {
      if (action === "rotate") {
        const rotated = await rotateApiKey(key.id);
        setConfirming(null);
        setConfirmText("");
        setMinted({ secret: rotated.secret, name: rotated.name, reason: "rotated" });
        setMintedCopied(false);
        setMintedAcknowledged(false);
        setNotice(`${key.name} was rotated. Its previous secret no longer authenticates.`);
      } else {
        await revokeApiKey(key.id);
        setConfirming(null);
        setConfirmText("");
        setNotice(`${key.name} was revoked. Its history stays in the request log.`);
      }
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : `The key could not be ${action}d.`);
      setConfirming(null);
    } finally {
      setBusy(false);
    }
  };

  const dismissMinted = () => {
    // Deliberately the *only* place the secret is dropped. There is nothing to re-fetch it from,
    // which is the whole design.
    setMinted(null);
    setMintedCopied(false);
    setMintedAcknowledged(false);
  };

  if (keys === null) {
    return error ? (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{error}</p>
        <button
          type="button"
          onClick={reload}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    ) : (
      <div className="flex items-center gap-2 px-1 py-8 text-[12.5px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Loading the API keys…
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <p
          role="alert"
          data-developer-keys-error
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          data-developer-keys-notice
          className="rounded-xl border border-positive/25 bg-positive-soft px-4 py-3 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[12.5px] text-muted">
          {keys.length === 0
            ? "No API keys yet."
            : `${visible.length} of ${keys.length} key${keys.length === 1 ? "" : "s"}.`}
        </p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={reload}
            className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-developer-key-create
            onClick={openForm}
            className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            New API key
          </button>
        </div>
      </div>

      {/* The filters. Present even when there is nothing to filter: an empty table with no
          filters reads as "the screen is broken", while an empty table with filters reads as
          "try widening these". */}
      <section
        aria-label="Filters"
        data-developer-keys-filters
        className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Environment
          <select
            value={filterEnvironment}
            onChange={(event) => setFilterEnvironment(event.target.value as typeof filterEnvironment)}
            data-developer-key-filter-environment
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="all">Any</option>
            <option value="live">Live</option>
            <option value="sandbox">Sandbox</option>
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Status
          <select
            value={filterStatus}
            onChange={(event) => setFilterStatus(event.target.value as typeof filterStatus)}
            data-developer-key-filter-status
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="all">Any</option>
            <option value="active">Active</option>
            <option value="pending">Pending</option>
            <option value="expired">Expired</option>
            <option value="revoked">Revoked</option>
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Scope
          <select
            value={filterScope}
            onChange={(event) => setFilterScope(event.target.value)}
            data-developer-key-filter-scope
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="all">Any</option>
            {allScopes.map((scope) => (
              <option key={scope} value={scope}>
                {scope}
              </option>
            ))}
          </select>
        </label>
        <label className="flex min-w-48 flex-1 flex-col gap-1 text-[12px] text-muted">
          Name or prefix
          <input
            value={filterName}
            onChange={(event) => setFilterName(event.target.value)}
            data-developer-key-filter-name
            placeholder="ci, staging, omn_a1b2…"
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          />
        </label>
      </section>

      {keys.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="Henüz API anahtarı yok"
            hint="A key is a credential your own integration presents to this platform. It is created once, shown once, and can be revoked at any time."
            action={
              <button
                type="button"
                onClick={openForm}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white"
              >
                <Plus className="size-3.5" aria-hidden />
                Create the first key
              </button>
            }
          />
        </div>
      ) : visible.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="No key matches these filters"
            hint="Widen the environment or status, or clear the name filter."
            action={
              <button
                type="button"
                onClick={() => {
                  setFilterEnvironment("all");
                  setFilterStatus("all");
                  setFilterScope("all");
                  setFilterName("");
                }}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px]"
              >
                Clear the filters
              </button>
            }
          />
        </div>
      ) : (
        <>
          {/* A table from `md` up, cards below it. Both carry the row hooks, because a harness
              that measures a table at 390px measures a hidden element and reports "the screen
              renders nothing" — which is the w5 lesson from the CDN purge history. */}
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
            <table className="w-full text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-3 py-2 font-medium">Name</th>
                  <th className="px-3 py-2 font-medium">Prefix</th>
                  <th className="px-3 py-2 font-medium">Environment</th>
                  <th className="px-3 py-2 font-medium">Scopes</th>
                  <th className="px-3 py-2 font-medium">Created</th>
                  <th className="px-3 py-2 font-medium">Last used</th>
                  <th className="px-3 py-2 font-medium">Expires</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {visible.map((key) => (
                  <tr
                    key={key.id}
                    data-developer-key-row
                    data-developer-key-status={key.status}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="px-3 py-2 font-medium">{key.name}</td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">{key.prefix}</td>
                    <td className="px-3 py-2">
                      {key.rate_tier === "high" ? (
                        <span className="inline-flex items-center gap-1 text-[11.5px] text-muted">
                          <ShieldCheck className="size-3" aria-hidden />
                          High tier
                        </span>
                      ) : null}
                      {key.environment === "live" ? "Live" : "Sandbox"}
                    </td>
                    <td className="px-3 py-2">
                      <span className="flex flex-wrap items-center gap-1">
                        {key.scopes.slice(0, SCOPE_CHIP_LIMIT).map((scope) => (
                          <span
                            key={scope}
                            className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                          >
                            {shortScope(scope)}
                          </span>
                        ))}
                        {key.scopes.length > SCOPE_CHIP_LIMIT ? (
                          <span
                            title={key.scopes.join(", ")}
                            className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                          >
                            +{key.scopes.length - SCOPE_CHIP_LIMIT}
                          </span>
                        ) : null}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-muted">{formatTimestamp(key.created_at)}</td>
                    <td className="px-3 py-2 text-muted">
                      {key.last_used_at ? formatTimestamp(key.last_used_at) : "Never used"}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {key.expires_at ? formatTimestamp(key.expires_at) : "Never"}
                    </td>
                    <td className="px-3 py-2">
                      <StatusBadge status={key.status} />
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex items-center justify-end gap-1">
                        <a
                          href={`/developer/logs?key_prefix=${encodeURIComponent(key.prefix)}`}
                          data-developer-key-logs-link
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-canvas"
                        >
                          Logs
                        </a>
                        <button
                          type="button"
                          data-developer-key-rotate
                          disabled={key.status !== "active"}
                          onClick={() => {
                            setConfirming({ key, action: "rotate" });
                            setConfirmText("");
                          }}
                          title={
                            key.status !== "active"
                              ? `A ${key.status} key cannot be rotated.`
                              : "Issue a new secret"
                          }
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <RefreshCw className="size-3.5" aria-hidden />
                          <span className="sr-only">Rotate {key.name}</span>
                        </button>
                        <button
                          type="button"
                          data-developer-key-revoke
                          disabled={key.status === "revoked"}
                          onClick={() => {
                            setConfirming({ key, action: "revoke" });
                            setConfirmText("");
                          }}
                          title={
                            key.status === "revoked"
                              ? "This key is already revoked."
                              : "Revoke this key"
                          }
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                          <span className="sr-only">Revoke {key.name}</span>
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="flex flex-col gap-2 md:hidden">
            {visible.map((key) => (
              <article
                key={key.id}
                data-developer-key-card
                data-developer-key-status={key.status}
                className="rounded-xl border border-line bg-surface px-4 py-3"
              >
                <div className="flex items-start justify-between gap-2">
                  <div>
                    <p className="text-[13.5px] font-medium">{key.name}</p>
                    <p className="font-mono text-[11.5px] text-muted">{key.prefix}</p>
                  </div>
                  <StatusBadge status={key.status} />
                </div>
                <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-[11.5px]">
                  <dt className="text-muted">Environment</dt>
                  <dd>{key.environment === "live" ? "Live" : "Sandbox"}</dd>
                  <dt className="text-muted">Scopes</dt>
                  <dd>{key.scopes.length}</dd>
                  <dt className="text-muted">Last used</dt>
                  <dd>{key.last_used_at ? formatTimestamp(key.last_used_at) : "Never"}</dd>
                  <dt className="text-muted">Expires</dt>
                  <dd>{key.expires_at ? formatTimestamp(key.expires_at) : "Never"}</dd>
                </dl>
                <div className="mt-3 flex items-center gap-2">
                  <a
                    href={`/developer/logs?key_prefix=${encodeURIComponent(key.prefix)}`}
                    className="inline-flex min-h-11 flex-1 items-center justify-center rounded-lg border border-line text-[12.5px]"
                  >
                    Logs
                  </a>
                  <button
                    type="button"
                    disabled={key.status !== "active"}
                    onClick={() => {
                      setConfirming({ key, action: "rotate" });
                      setConfirmText("");
                    }}
                    className="inline-flex min-h-11 flex-1 items-center justify-center rounded-lg border border-line text-[12.5px] disabled:opacity-40"
                  >
                    Rotate
                  </button>
                  <button
                    type="button"
                    disabled={key.status === "revoked"}
                    onClick={() => {
                      setConfirming({ key, action: "revoke" });
                      setConfirmText("");
                    }}
                    className="inline-flex min-h-11 flex-1 items-center justify-center rounded-lg border border-line text-[12.5px] disabled:opacity-40"
                  >
                    Revoke
                  </button>
                </div>
              </article>
            ))}
          </div>
        </>
      )}

      {formOpen ? (
        <section
          aria-label="New API key"
          data-developer-key-form
          className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4"
        >
          <div className="flex items-center gap-2">
            <KeyRound className="size-4" aria-hidden />
            <h2 className="text-[13.5px] font-medium">New API key</h2>
          </div>

          <label className="flex flex-col gap-1 text-[12.5px]">
            Name
            <input
              value={draft.name}
              onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              data-developer-key-name
              placeholder="ci-deploy"
              className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px]"
            />
            {fieldError?.field === "name" ? (
              <span
                data-developer-key-name-error
                role="alert"
                className="text-[11.5px] text-accent-strong"
              >
                {fieldError.message}
              </span>
            ) : null}
          </label>

          <fieldset className="flex flex-col gap-2">
            <legend className="text-[12.5px] font-medium">Environment</legend>
            <div className="flex flex-wrap gap-3">
              {(["sandbox", "live"] as const).map((value) => (
                <label key={value} className="flex cursor-pointer items-center gap-2 text-[12.5px]">
                  <input
                    type="radio"
                    name="developer-key-environment"
                    value={value}
                    checked={draft.environment === value}
                    onChange={() => setDraft({ ...draft, environment: value })}
                    data-developer-key-environment={value}
                    className="size-3.5"
                  />
                  {value === "live" ? "Live — production data" : "Sandbox — test data"}
                </label>
              ))}
            </div>
            <p className="text-[11.5px] text-muted">
              A key authenticates against one environment. Pick sandbox while you are building
              the integration.
            </p>
          </fieldset>

          <fieldset className="flex flex-col gap-2">
            <legend className="text-[12.5px] font-medium">Scopes</legend>
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                data-developer-key-select-reads
                onClick={() =>
                  setDraft((current) => {
                    const reads = SCOPE_CHOICES.filter((choice) => choice.key.endsWith(".read")).map(
                      (choice) => choice.key,
                    );
                    const already = reads.every((scope) => current.scopes.includes(scope));
                    return {
                      ...current,
                      scopes: already
                        ? current.scopes.filter((scope) => !reads.includes(scope))
                        : [...new Set([...current.scopes, ...reads])].sort(),
                    };
                  })
                }
                className="rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:bg-canvas"
              >
                Select all read scopes
              </button>
              <span className="text-[11.5px] text-muted">
                {draft.scopes.length} selected
              </span>
            </div>
            <div className="grid gap-1.5 sm:grid-cols-2">
              {SCOPE_CHOICES.map((choice) => (
                <label
                  key={choice.key}
                  className="flex cursor-pointer items-start gap-2 rounded-lg border border-line px-2.5 py-1.5"
                >
                  <input
                    type="checkbox"
                    checked={draft.scopes.includes(choice.key)}
                    onChange={() => toggleScope(choice.key)}
                    data-developer-key-scope={choice.key}
                    className="mt-0.5 size-3.5"
                  />
                  <span className="text-[11.5px]">
                    <span className="block font-mono">{choice.key}</span>
                    <span className="block text-muted">{choice.label}</span>
                  </span>
                </label>
              ))}
            </div>
            {fieldError?.field === "scopes" ? (
              <span
                data-developer-key-scopes-error
                role="alert"
                className="text-[11.5px] text-accent-strong"
              >
                {fieldError.message}
              </span>
            ) : null}
          </fieldset>

          <label className="flex flex-col gap-1 text-[12.5px]">
            Expiry
            <select
              value={draft.expiry}
              onChange={(event) => setDraft({ ...draft, expiry: event.target.value })}
              data-developer-key-expiry
              className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px]"
            >
              {EXPIRY_CHOICES.map((choice) => (
                <option key={choice.value} value={choice.value}>
                  {choice.label}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-[12.5px]">
            IP allowlist (optional)
            <textarea
              value={draft.ipAllowlist}
              onChange={(event) => setDraft({ ...draft, ipAllowlist: event.target.value })}
              data-developer-key-allowlist
              rows={3}
              placeholder={"10.0.0.0/8\n203.0.113.4/32"}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 font-mono text-[11.5px]"
            />
            <span className="text-[11.5px] text-muted">
              One CIDR block per line. Leave empty to allow any source address.
            </span>
            {fieldError?.field === "ip_allowlist" ? (
              <span role="alert" className="text-[11.5px] text-accent-strong">
                {fieldError.message}
              </span>
            ) : null}
          </label>

          <fieldset className="flex flex-col gap-1">
            <legend className="text-[12.5px] font-medium">Rate tier</legend>
            <label className="flex cursor-pointer items-center gap-2 text-[12.5px]">
              <input
                type="radio"
                name="developer-key-tier"
                checked={draft.rateTier === "standard"}
                onChange={() => setDraft({ ...draft, rateTier: "standard" })}
                data-developer-key-tier="standard"
                className="size-3.5"
              />
              Standard
            </label>
            <label className="flex items-center gap-2 text-[12.5px]">
              {/* Offered and disabled rather than hidden: a developer who needs it is entitled
                  to know it exists and to ask an owner for it. */}
              <input
                type="radio"
                name="developer-key-tier"
                checked={draft.rateTier === "high"}
                onChange={() => setDraft({ ...draft, rateTier: "high" })}
                data-developer-key-tier="high"
                disabled
                className="size-3.5"
              />
              High — a higher request ceiling. Requires an owner or administrator.
            </label>
          </fieldset>

          {fieldError?.field === "form" ? (
            <p role="alert" className="text-[12px] text-accent-strong">
              {fieldError.message}
            </p>
          ) : null}

          <div className="flex items-center gap-2">
            <button
              type="button"
              data-developer-key-submit
              disabled={busy}
              onClick={submitForm}
              className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-50"
            >
              {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
              Create the key
            </button>
            <button
              type="button"
              onClick={() => setFormOpen(false)}
              className="min-h-9 rounded-lg border border-line px-3 text-[12.5px]"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      {/* The one-time secret. Not dismissible by clicking the backdrop — only by the button, and
          only after the acknowledgement, because the alternative is a lost credential that the
          developer has to go and rotate. */}
      {minted ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Your new API key"
          data-developer-key-secret-dialog
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
        >
          <div className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-surface px-5 py-4">
            <div className="flex items-center gap-2">
              <AlertTriangle className="size-4 text-caution" aria-hidden />
              <h2 className="text-[13.5px] font-medium">
                {minted.reason === "created" ? "Your key is ready" : "Your key was rotated"}
              </h2>
            </div>
            <p className="text-[12.5px] text-muted">
              This is the only time <span className="font-medium text-ink">{minted.name}</span>{" "}
              will show its secret. Copy it now — the platform stores only a one-way hash and
              cannot show it again. If you lose it, rotate the key and use the new secret.
            </p>
            <code
              data-developer-key-secret
              className="block overflow-x-auto rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px]"
            >
              {minted.secret}
            </code>
            <div className="flex items-center gap-2">
              <button
                type="button"
                data-developer-key-copy
                onClick={async () => {
                  const copied = await copyToClipboard(minted.secret);
                  setMintedCopied(copied);
                }}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-canvas"
              >
                {mintedCopied ? (
                  <Check className="size-3.5 text-positive" aria-hidden />
                ) : (
                  <Copy className="size-3.5" aria-hidden />
                )}
                {mintedCopied ? "Copied" : "Copy the secret"}
              </button>
              <label className="flex items-center gap-2 text-[11.5px] text-muted">
                <input
                  type="checkbox"
                  checked={mintedAcknowledged}
                  onChange={(event) => setMintedAcknowledged(event.target.checked)}
                  data-developer-key-acknowledged
                  className="size-3.5"
                />
                I have copied it somewhere safe.
              </label>
            </div>
            <div className="flex justify-end">
              <button
                type="button"
                data-developer-key-secret-done
                disabled={!mintedAcknowledged}
                onClick={dismissMinted}
                className="min-h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-40"
              >
                Done
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {/* Rotate and revoke confirmations. Rotation's copy names the consequence; revocation's
          demands a typed name because it is the one action here that cannot be undone. */}
      {confirming ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={confirming.action === "rotate" ? "Rotate key" : "Revoke key"}
          data-developer-key-confirm
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
        >
          <div className="flex w-full max-w-md flex-col gap-3 rounded-xl border border-line bg-surface px-5 py-4">
            <h2 className="text-[13.5px] font-medium">
              {confirming.action === "rotate" ? "Rotate this key?" : "Revoke this key?"}
            </h2>
            <p className="text-[12.5px] text-muted">
              {confirming.action === "rotate" ? (
                <>
                  <span className="font-medium text-ink">{confirming.key.name}</span> gets a new
                  secret, shown once. The previous secret stops working immediately — there is no
                  overlap window, so any integration still using it will start failing at once.
                </>
              ) : (
                <>
                  <span className="font-medium text-ink">{confirming.key.name}</span> will stop
                  authenticating immediately. The key keeps its history in the request log, so you
                  can still see what it did. This cannot be undone — rotating a revoked key is not
                  possible.
                </>
              )}
            </p>
            {confirming.action === "revoke" ? (
              <label className="flex flex-col gap-1 text-[12px]">
                Type <span className="font-medium text-ink">{confirming.key.name}</span> to confirm
                <input
                  value={confirmText}
                  onChange={(event) => setConfirmText(event.target.value)}
                  data-developer-key-confirm-input
                  className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px]"
                />
              </label>
            ) : null}
            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => {
                  setConfirming(null);
                  setConfirmText("");
                }}
                className="min-h-9 rounded-lg border border-line px-3 text-[12.5px]"
              >
                Cancel
              </button>
              <button
                type="button"
                data-developer-key-confirm-submit
                disabled={busy || (confirming.action === "revoke" && confirmText !== confirming.key.name)}
                onClick={runConfirmed}
                className="min-h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-40"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                {confirming.action === "rotate" ? "Rotate and show the new secret" : "Revoke"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
