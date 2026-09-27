"use client";

/**
 * `/organizations` — the tenant list (REQ-005, slice 1).
 *
 * A platform account sees every organization it may open; an organization account is sent to its
 * own overview instead, because there is nothing to choose between. The list carries the search
 * and the status filter the detail screen's own rows need, and its empty state is a real
 * Create rather than a sentence.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { Building2, Plus, RefreshCw, Search } from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { ApiError, createOrganization, fetchOrganizations, updateOrganization } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { Organization } from "@/lib/types";

/** The statuses a filter offers — the three the schema allows. */
const STATUSES = ["active", "suspended", "archived"] as const;

/** The longest slug the schema accepts (crates/identity::organizations::MAX_SLUG_LENGTH). */
const MAX_SLUG_LENGTH = 64;

/**
 * Turn a name into a slug the server will accept, and say so when it cannot.
 *
 * The server's rule is strict — a slug starts with a lowercase letter or digit, carries only
 * lowercase letters, digits and single dashes. Falling back to the raw name sent "QA sample" to
 * the API and took a 400 the reader could not have predicted, so the form normalizes here and
 * says plainly that it did.
 */
function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, MAX_SLUG_LENGTH);
}

/** Why a slug is unusable, or `null` when it is fine. Mirrors the server's rule. */
function slugProblem(slug: string): string | null {
  if (!slug) return "A slug is required.";
  if (slug.length > MAX_SLUG_LENGTH) {
    return `A slug is at most ${MAX_SLUG_LENGTH} characters.`;
  }
  if (!/^[a-z0-9]/.test(slug)) {
    return "A slug starts with a lowercase letter or a digit.";
  }
  if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(slug)) {
    return "A slug uses lowercase letters, digits and single dashes.";
  }
  return null;
}

/** The create/suspend controls. */
function CreateForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!name.trim()) {
      setError("A name is required.");
      return;
    }
    // An empty slug is a request to derive one, not a broken form — but the derived slug still
    // has to satisfy the server's rule, so it is checked here rather than discovered as a 400.
    const effectiveSlug = slug.trim() || slugify(name);
    const problem = slugProblem(effectiveSlug);
    if (problem) {
      setError(problem);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await createOrganization({ name: name.trim(), slug: effectiveSlug });
      onDone();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The organization could not be created.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-3 border-b border-line px-4 py-3.5" data-qa-guard="write">
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Name
          <input
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="Acme Corporation"
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
        </label>
        <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
          Slug
          <input
            value={slug}
            onChange={(event) => setSlug(event.target.value)}
            placeholder={slugify(name) || "acme-corp"}
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          <span className="text-[11.5px] font-normal text-muted">
            {slug.trim()
              ? "Lowercase letters, digits and single dashes."
              : name.trim()
                ? `Leave empty to use "${slugify(name)}".`
                : "How the organization is addressed in a URL."}
          </span>
        </label>
      </div>
      {error ? (
        <p role="alert" className="text-[12.5px] text-accent-strong">
          {error}
        </p>
      ) : null}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={() => void submit()}
          disabled={busy}
          className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-60"
        >
          {busy ? "Creating…" : "Create organization"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/** `/organizations`. */
export function OrganizationsView() {
  const { user } = useSession();
  const router = useRouter();
  const [organizations, setOrganizations] = useState<Organization[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<string>("");
  const [creating, setCreating] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      setOrganizations(await fetchOrganizations());
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setError(
        cause instanceof ApiError ? cause.message : "The organizations could not be loaded.",
      );
    }
  }, []);

  useEffect(() => {
    if (user === null) return;
    // An organization account has exactly one organization to be in: send it to its overview
    // rather than showing a list it can only ever read one row of.
    if (user.organization_id !== null) {
      router.replace(`/organizations/${user.organization_id}`);
      return;
    }
    void load();
  }, [user, load, router]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return organizations.filter((organization) => {
      if (statusFilter && organization.status !== statusFilter) return false;
      if (!needle) return true;
      return (
        organization.name.toLowerCase().includes(needle) ||
        organization.slug.toLowerCase().includes(needle)
      );
    });
  }, [organizations, query, statusFilter]);

  const setStatusOf = async (organization: Organization, next: string) => {
    setBusyId(organization.id);
    setNotice(null);
    try {
      await updateOrganization(organization.id, { status: next });
      setNotice(`${organization.name} is now ${next}.`);
      await load();
    } catch (cause) {
      setNotice(
        cause instanceof ApiError ? cause.message : "The status could not be changed.",
      );
    } finally {
      setBusyId(null);
    }
  };

  if (user === null) {
    return null;
  }

  return (
    <div className="flex flex-col gap-4">
      {notice ? (
        <p
          role="status"
          className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex items-baseline gap-2">
            <h2 className="text-[13.5px] font-medium">Organizations</h2>
            <span className="text-[12px] text-muted">
              {status === "ready" ? `${filtered.length} shown` : "Loading…"}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5">
              <Search className="size-3.5 text-muted" aria-hidden />
              <span className="sr-only">Search organizations</span>
              <input
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder="Name or slug"
                className="w-40 bg-transparent text-[12.5px] outline-none"
              />
            </label>
            <label className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5">
              <span className="sr-only">Filter by status</span>
              <select
                value={statusFilter}
                onChange={(event) => setStatusFilter(event.target.value)}
                className="bg-transparent text-[12.5px] outline-none"
              >
                <option value="">All statuses</option>
                {STATUSES.map((value) => (
                  <option key={value} value={value}>
                    {value}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={() => void load()}
              aria-label="Reload organizations"
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
            {platformAccount ? (
              <button
                type="button"
                onClick={() => setCreating((open) => !open)}
                className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition"
              >
                <Plus className="size-3.5" aria-hidden />
                New organization
              </button>
            ) : null}
          </div>
        </div>

        {creating ? (
          <CreateForm
            onDone={() => {
              setCreating(false);
              void load();
            }}
            onCancel={() => setCreating(false)}
          />
        ) : null}

        {error ? (
          <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
            <p className="text-[12.5px] text-accent-strong">{error}</p>
            <button
              type="button"
              onClick={() => void load()}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : status !== "ready" ? (
          <LoadingTable columns={5} />
        ) : filtered.length === 0 ? (
          <EmptyState
            title={
              organizations.length === 0 ? "No organizations yet" : "Nothing matches that search"
            }
            hint={
              organizations.length === 0
                ? "An organization owns its sites, members and settings. Create the first one to get started."
                : "Clear the search or the status filter to see the organizations you have."
            }
            action={
              organizations.length === 0 && platformAccount ? (
                <button
                  type="button"
                  onClick={() => setCreating(true)}
                  className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
                >
                  Create organization
                </button>
              ) : null
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                  <th scope="col" className="px-4 py-2.5">
                    Organization
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Slug
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Status
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Members
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Created
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {filtered.map((organization) => (
                  <tr key={organization.id} className="border-t border-line transition hover:bg-canvas/60">
                    <td className="px-4 py-3.5">
                      <span className="flex min-w-0 items-center gap-2">
                        <Building2 className="size-3.5 shrink-0 text-muted" aria-hidden />
                        <Link
                          href={`/organizations/${organization.id}`}
                          className="truncate font-medium hover:underline"
                        >
                          {organization.name}
                        </Link>
                      </span>
                    </td>
                    <td className="px-4 py-3.5 text-muted">{organization.slug}</td>
                    <td className="px-4 py-3.5">
                      <StatusBadge status={organization.status} />
                    </td>
                    <td className="px-4 py-3.5">
                      <Link
                        href={`/organizations/${organization.id}?tab=members`}
                        className="text-accent-strong hover:underline"
                      >
                        Manage
                      </Link>
                    </td>
                    <td className="px-4 py-3.5 text-muted">
                      {formatTimestamp(organization.created_at)}
                    </td>
                    <td className="px-4 py-3.5 text-right">
                      {organization.status === "active" ? (
                        <button
                          type="button"
                          data-qa-guard="write"
                          onClick={() => void setStatusOf(organization, "suspended")}
                          disabled={busyId === organization.id}
                          className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
                        >
                          Suspend
                        </button>
                      ) : (
                        <button
                          type="button"
                          data-qa-guard="write"
                          onClick={() => void setStatusOf(organization, "active")}
                          disabled={busyId === organization.id}
                          className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-60"
                        >
                          Reactivate
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
