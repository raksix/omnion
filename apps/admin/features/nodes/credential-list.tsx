"use client";

/**
 * The credential list (`/workflows/credentials`): the integrations an installation has, and
 * what state each one is in.
 *
 * Five claims this screen makes, each one a way a credential list misleads the person reading
 * it:
 *
 * 1. **The counts are the server's.** "4 credentials · 2 need attention" reads `total` and
 *    `needs_attention` off the response rather than counting rows on screen, so a filter the
 *    server applied and the client ignored cannot produce two different numbers.
 * 2. **An empty list says which of two things it is.** "No credential matches `stripe`" and
 *    "this installation has none yet" are different problems with the same pixels, and only
 *    one of them is fixed by typing a different word. The empty state offers the two most
 *    common types as one-click creates rather than only a link back to the library.
 * 3. **A credential that needs a human is amber, and says which button fixes it.** An
 *    `effective_health` of `needs_reauth` means re-connecting, not re-testing, and the chip
 *    says so — the two need different buttons and sending somebody to the wrong one is how a
 *    broken integration stays broken for a month.
 * 4. **A delete that is refused shows what still uses it.** The API's `credential_in_use`
 *    refusal carries the workflow list; rendering "cannot delete" without it would make the
 *    reader go and find out by hand, which is exactly the trip the guard exists to prevent.
 * 5. **Nothing on this screen can display a secret.** There is no value in the response to
 *    render, and the reveal toggle that *would* exist on the detail screen is off by default
 *    and — since the API never returns one — has nothing to reveal.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import Link from "next/link";
import {
  ArrowRight,
  CircleAlert,
  KeyRound,
  Plus,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";

import {
  deleteCredential,
  fetchCredentials,
  fetchCredentialTypes,
  type ApiError,
} from "@/lib/api";
import type { Credential, CredentialFilters, CredentialType } from "@/lib/types";

/**
 * Chip colour per health.
 *
 * `untested` is deliberately neutral rather than green: a credential nobody has tested is not
 * a working credential, and painting it as one is how a broken integration ships.
 */
const HEALTH_TONE: Record<string, string> = {
  ok: "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300",
  untested: "bg-quiet-soft text-muted",
  failing: "bg-red-500/10 text-red-700 dark:text-red-300",
  needs_reauth: "bg-amber-500/10 text-amber-700 dark:text-amber-300",
};

/** What the reader is told, in the reader's words rather than the database's. */
const HEALTH_LABEL: Record<string, string> = {
  ok: "Verified",
  untested: "Not verified",
  failing: "Failing",
  needs_reauth: "Needs re-connecting",
};

/** The lucide icon per credential type, so a row is recognisable at a glance. */
const TYPE_ICON: Record<string, typeof KeyRound> = {
  api_key: KeyRound,
  oauth2: ShieldCheck,
  basic_auth: KeyRound,
  smtp: CircleAlert,
  cloud_storage: KeyRound,
};

/** Read the filters out of the query string, so a filtered list can be pasted. */
function filtersFrom(params: URLSearchParams): CredentialFilters {
  const filters: CredentialFilters = {};
  const search = params.get("search");
  if (search) filters.search = search;
  for (const field of ["type", "scope", "health", "sharing"] as const) {
    const value = params.get(field);
    if (value) filters[field] = value;
  }
  return filters;
}

export function CredentialList() {
  const router = useRouter();
  const params = useSearchParams();
  const filters = useMemo(() => filtersFrom(params), [params]);

  const [rows, setRows] = useState<Credential[]>([]);
  const [total, setTotal] = useState(0);
  const [attention, setAttention] = useState(0);
  const [types, setTypes] = useState<CredentialType[]>([]);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [blocked, setBlocked] = useState<{ id: string; message: string } | null>(null);
  const [confirmed, setConfirmed] = useState<string | null>(null);

  const searchRef = useRef<HTMLInputElement>(null);

  // `/` focuses the search box — the same binding the node library and the inbox use, so the
  // panel has one set of habits rather than one per screen.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "/" && !event.metaKey && !event.ctrlKey) {
        const target = event.target as HTMLElement | null;
        const typing =
          target?.tagName === "INPUT" ||
          target?.tagName === "TEXTAREA" ||
          target?.isContentEditable;
        if (!typing) {
          event.preventDefault();
          searchRef.current?.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    let alive = true;
    fetchCredentialTypes()
      .then((page) => {
        if (alive) setTypes(page.types);
      })
      .catch(() => {
        // A failed catalogue read is not a failed list: the rows carry their own `type_label`,
        // so the filter select is simply shorter. Saying so would be noise.
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    fetchCredentials(filters)
      .then((page) => {
        if (!alive) return;
        setRows(page.credentials);
        setTotal(page.total);
        setAttention(page.needs_attention);
        setError(null);
        setBlocked(null);
      })
      .catch((cause: ApiError) => {
        if (!alive) return;
        setError(cause);
        setRows([]);
        setTotal(0);
        setAttention(0);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [filters]);

  const setFilter = useCallback(
    (patch: Partial<CredentialFilters>) => {
      const next = new URLSearchParams();
      const merged = { ...filters, ...patch };
      if (merged.search) next.set("search", merged.search);
      if (merged.type) next.set("type", merged.type);
      if (merged.scope) next.set("scope", merged.scope);
      if (merged.health) next.set("health", merged.health);
      if (merged.sharing) next.set("sharing", merged.sharing);
      const query = next.toString();
      router.replace(query ? `/workflows/credentials?${query}` : "/workflows/credentials");
    },
    [filters, router],
  );

  const hasFilters = Object.keys(filters).length > 0;

  /**
   * Delete, and report what the guard said.
   *
   * A `credential_in_use` refusal is not an error state to shake at the reader: it is the
   * answer, and it carries the list of workflows. The force button is a second press, never
   * the first, so breaking four workflows is never one stray click away.
   */
  const onDelete = useCallback(
    async (credential: Credential, force: boolean) => {
      setBusy(credential.id);
      setError(null);
      try {
        const result = await deleteCredential(credential.id, force);
        setConfirmed(
          result.workflow_count > 0
            ? `Deleted ${credential.name}. ${result.workflow_count} workflow(s) now reference a credential that no longer exists.`
            : `Deleted ${credential.name}.`,
        );
        setRows((current) => current.filter((row) => row.id !== credential.id));
        setTotal((current) => Math.max(0, current - 1));
        setConfirmed(null);
      } catch (cause) {
        const failure = cause as ApiError;
        if (failure.code === "credential_in_use" && !force) {
          const details = failure.details as
            | { references?: { workflow_name: string }[]; workflow_count?: number }
            | null;
          const names = (details?.references ?? [])
            .map((reference) => reference.workflow_name)
            .filter((name, index, all) => all.indexOf(name) === index);
          setBlocked({
            id: credential.id,
            message: names.length
              ? `${names.join(", ")} still ${names.length === 1 ? "names" : "name"} it. Deleting it anyway will break ${details?.workflow_count ?? names.length} workflow(s).`
              : "It is still named by a workflow.",
          });
        } else {
          setError(failure);
        }
      } finally {
        setBusy(null);
      }
    },
    [],
  );

  return (
    <div className="space-y-5">
      <div className="rounded-xl border border-line bg-quiet-soft/40 p-4">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex min-w-[220px] flex-1 flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Search</span>
            <span className="relative">
              <Search
                aria-hidden
                className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted"
                size={15}
              />
              <input
                ref={searchRef}
                type="search"
                value={filters.search ?? ""}
                placeholder="Name or key"
                onChange={(event) => setFilter({ search: event.target.value })}
                className="w-full rounded-lg border border-line bg-surface py-2 pl-9 pr-8 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
              {filters.search ? (
                <button
                  type="button"
                  aria-label="Clear the search"
                  onClick={() => setFilter({ search: "" })}
                  className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-1 text-muted hover:text-ink"
                >
                  <X size={14} />
                </button>
              ) : null}
            </span>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Type</span>
            <select
              data-credential-filter="type"
              value={filters.type ?? ""}
              onChange={(event) => setFilter({ type: event.target.value || undefined })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">All types</option>
              {types.map((type) => (
                <option key={type.key} value={type.key}>
                  {type.label}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Health</span>
            <select
              data-credential-filter="health"
              value={filters.health ?? ""}
              onChange={(event) => setFilter({ health: event.target.value || undefined })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Any</option>
              <option value="ok">Verified</option>
              <option value="untested">Not verified</option>
              <option value="failing">Failing</option>
              <option value="needs_reauth">Needs re-connecting</option>
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Scope</span>
            <select
              data-credential-filter="scope"
              value={filters.scope ?? ""}
              onChange={(event) => setFilter({ scope: event.target.value || undefined })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Any</option>
              <option value="organization">Organization</option>
              <option value="project">Project</option>
            </select>
          </label>

          {hasFilters ? (
            <button
              type="button"
              onClick={() => router.replace("/workflows/credentials")}
              className="pb-2 text-[13px] text-accent underline underline-offset-2"
            >
              Clear filters
            </button>
          ) : null}

          <Link
            href="/workflows/credentials/new"
            data-credential-create
            className="ml-auto inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[13px] font-medium text-white"
          >
            <Plus size={14} />
            New credential
          </Link>
        </div>

        <p className="mt-3 text-[12px] text-muted" data-credential-count>
          {loading && rows.length === 0
            ? "Loading credentials…"
            : `${total} credential${total === 1 ? "" : "s"}${
                attention > 0 ? ` · ${attention} need${attention === 1 ? "s" : ""} attention` : ""
              }`}
        </p>
      </div>

      {error ? (
        <p
          role="alert"
          className="flex items-center gap-2 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
        >
          <TriangleAlert size={15} />
          {error.message}
        </p>
      ) : null}

      {confirmed ? (
        <p
          role="status"
          data-credential-notice
          className="rounded-xl border border-line bg-quiet-soft/60 px-4 py-3 text-[13px] text-ink"
        >
          {confirmed}
        </p>
      ) : null}

      {blocked ? (
        <div
          role="alert"
          data-credential-blocked
          className="rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-[13px] text-amber-800 dark:text-amber-200"
        >
          <p className="flex items-start gap-2">
            <TriangleAlert size={15} className="mt-0.5 shrink-0" />
            <span>{blocked.message}</span>
          </p>
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              onClick={() => setBlocked(null)}
              className="rounded-lg border border-line px-3 py-1.5 text-[13px]"
            >
              Keep it
            </button>
            <button
              type="button"
              data-credential-force-delete
              onClick={() => {
                const id = blocked.id;
                setBlocked(null);
                void onDelete(rows.find((row) => row.id === id) as Credential, true);
              }}
              className="rounded-lg bg-red-600 px-3 py-1.5 text-[13px] font-medium text-white"
            >
              Delete anyway
            </button>
          </div>
        </div>
      ) : null}

      {loading && rows.length === 0 ? (
        <div className="space-y-2" data-credential-skeleton>
          {[0, 1, 2].map((row) => (
            <div key={row} className="h-16 animate-pulse rounded-xl border border-line bg-quiet-soft/40" />
          ))}
        </div>
      ) : rows.length === 0 ? (
        <div
          data-credential-empty
          className="rounded-xl border border-dashed border-line px-6 py-10 text-center"
        >
          {hasFilters ? (
            <>
              <p className="text-[14px] font-medium text-ink">
                No credential matches {Object.values(filters).filter(Boolean).join(", ")}
              </p>
              <p className="mt-1 text-[13px] text-muted">
                Clear the filters to see everything this installation has.
              </p>
              <button
                type="button"
                onClick={() => router.replace("/workflows/credentials")}
                className="mt-3 text-[13px] text-accent underline underline-offset-2"
              >
                Clear filters
              </button>
            </>
          ) : (
            <>
              <p className="text-[14px] font-medium text-ink">No credentials yet</p>
              <p className="mt-1 text-[13px] text-muted">
                A credential holds the key a node needs. Add one and the nodes that ask for it
                stop asking.
              </p>
              <div className="mt-4 flex flex-wrap justify-center gap-2">
                {/* The two most common types, offered directly: a person arriving at an empty
                    screen wants an API key far more often than they want a catalogue tour. */}
                <Link
                  href="/workflows/credentials/new?type=api_key"
                  data-credential-quick="api_key"
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[13px] font-medium text-white"
                >
                  <KeyRound size={14} />
                  Add an API key
                </Link>
                <Link
                  href="/workflows/credentials/new?type=oauth2"
                  data-credential-quick="oauth2"
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px]"
                >
                  <ShieldCheck size={14} />
                  Connect an OAuth app
                </Link>
                <Link
                  href="/workflows/nodes"
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px]"
                >
                  Browse the node library
                  <ArrowRight size={14} />
                </Link>
              </div>
            </>
          )}
        </div>
      ) : (
        <ul className="space-y-2">
          {rows.map((credential) => {
            const Icon = TYPE_ICON[credential.type] ?? KeyRound;
            const health = credential.effective_health;
            return (
              <li
                key={credential.id}
                data-credential-row={credential.key}
                className="rounded-xl border border-line bg-surface p-4"
              >
                <div className="flex flex-wrap items-start gap-3">
                  <span className="mt-0.5 rounded-lg border border-line p-2">
                    <Icon size={16} aria-hidden />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <Link
                        href={`/workflows/credentials/${credential.id}`}
                        data-credential-open={credential.key}
                        className="text-[14px] font-medium text-ink underline-offset-2 hover:underline"
                      >
                        {credential.name}
                      </Link>
                      <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                        {credential.key}
                      </code>
                      <span
                        data-credential-health={health}
                        className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          HEALTH_TONE[health] ?? HEALTH_TONE.untested
                        }`}
                      >
                        {HEALTH_LABEL[health] ?? health}
                      </span>
                      {credential.expired ? (
                        <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[11px] font-medium text-amber-700 dark:text-amber-300">
                          Token expired
                        </span>
                      ) : null}
                      {!credential.has_secret ? (
                        <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                          No secret
                        </span>
                      ) : null}
                    </div>
                    <p className="mt-1 text-[12px] text-muted">
                      {credential.type_label} · {credential.scope} ·{" "}
                      {credential.sharing === "private" ? "private to you" : "shared with the organization"}
                      {credential.last_used_at
                        ? ` · last used ${new Date(credential.last_used_at).toLocaleDateString()}`
                        : " · never used"}
                    </p>
                    {credential.health_detail ? (
                      <p className="mt-1 text-[12px] text-muted">{credential.health_detail}</p>
                    ) : null}
                  </div>

                  <div className="flex items-center gap-1.5">
                    <Link
                      href={`/workflows/credentials/${credential.id}`}
                      data-credential-detail-link={credential.key}
                      className="rounded-lg border border-line px-2.5 py-1.5 text-[13px]"
                    >
                      Details
                    </Link>
                    <button
                      type="button"
                      data-credential-delete={credential.key}
                      disabled={busy === credential.id}
                      onClick={() => void onDelete(credential, false)}
                      aria-label={`Delete ${credential.name}`}
                      className="rounded-lg border border-line p-2 text-muted hover:border-red-500/50 hover:text-red-600 disabled:opacity-50"
                    >
                      {busy === credential.id ? (
                        <RefreshCw size={14} className="animate-spin" />
                      ) : (
                        <Trash2 size={14} />
                      )}
                    </button>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
