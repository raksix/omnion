"use client";

/**
 * `/settings/iam/users/{id}` — one account in full (REQ-006, slice 2).
 *
 * Three tabs: the profile (display name, status, MFA requirement, attributes), the role
 * bindings (with the scope ladder — global down to a path glob — and an expiry window), and the
 * resolved effective permission set with the role behind every entry. Sessions, devices and MFA
 * enrolment land with the next slice; nothing here pretends to be them.
 */
import { useCallback, useEffect, useState } from "react";

import { KeyRound, Plus, RefreshCw, ShieldCheck, Trash2, UserCog } from "lucide-react";
import Link from "next/link";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamBinding,
  fetchEffectivePermissions,
  fetchIamBindings,
  fetchIamUser,
  fetchRoles,
  fetchSites,
  revokeIamBinding,
  updateIamUser,
  type IamBinding,
  type IamEffectivePermissions,
  type IamRole,
  type IamUserDetail,
} from "@/lib/api";
import type { Site } from "@/lib/types";

type Tab = "profile" | "bindings" | "effective";

/** How a scope reads. */
function scopeLabel(binding: IamBinding): string {
  const scope = binding.scope;
  switch (scope.type) {
    case "global":
      return "platform";
    case "organization":
      return "organization";
    case "site":
      return `site ${scope.site_id ? scope.site_id.slice(0, 8) : "?"}…`;
    case "department":
      return `department ${scope.resource_id ?? ""}`;
    case "module":
      return `module ${scope.resource_id ?? ""}`;
    case "resource":
      return `${scope.resource_type ?? "resource"} ${scope.resource_id ?? ""}`;
    default:
      return scope.type;
  }
}

/** `/settings/iam/users/{id}`. */
export function UserDetailView({ userId }: { userId: string }) {
  const { user } = useSession();
  const [tab, setTab] = useState<Tab>("profile");
  const [detail, setDetail] = useState<IamUserDetail | null>(null);
  const [bindings, setBindings] = useState<IamBinding[] | null>(null);
  const [effective, setEffective] = useState<IamEffectivePermissions | null>(null);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [sites, setSites] = useState<Site[]>([]);
  const [error, setError] = useState<{ code: string; message: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Profile form.
  const [displayName, setDisplayName] = useState("");
  const [status, setStatus] = useState("active");
  const [mfaEnforced, setMfaEnforced] = useState(false);
  const [attributesText, setAttributesText] = useState("{}");
  // Binding form.
  const [newRoleId, setNewRoleId] = useState("");
  const [scopeType, setScopeType] = useState<IamBinding["scope"]["type"]>("organization");
  const [scopeSite, setScopeSite] = useState("");
  const [scopeDepartment, setScopeDepartment] = useState("");
  const [scopeModule, setScopeModule] = useState("");
  const [scopeResource, setScopeResource] = useState("");
  const [expiresAt, setExpiresAt] = useState("");

  const load = useCallback(async () => {
    setError(null);
    try {
      const [account, own, ownEffective] = await Promise.all([
        fetchIamUser(userId),
        fetchIamBindings({ subjectType: "user", subjectId: userId }),
        fetchEffectivePermissions({ userId }),
      ]);
      setDetail(account);
      setBindings(own.bindings);
      setEffective(ownEffective);
      setDisplayName(account.user.display_name);
      setStatus(account.user.status);
      setMfaEnforced(account.user.mfa_enforced);
      setAttributesText(JSON.stringify(account.user.attributes ?? {}, null, 2));
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The account could not be read." },
      );
    }
  }, [userId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (!detail) return;
    void fetchRoles(detail.user.organization_id).then(setRoles).catch(() => setRoles([]));
    if (detail.user.organization_id) {
      void fetchSites(detail.user.organization_id).then(setSites).catch(() => setSites([]));
    }
  }, [detail]);

  const saveProfile = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    let attributes: Record<string, unknown>;
    try {
      attributes = JSON.parse(attributesText || "{}") as Record<string, unknown>;
    } catch {
      setError({ code: "invalid_attributes", message: "The attributes are not valid JSON." });
      setBusy(false);
      return;
    }
    try {
      await updateIamUser(userId, { displayName, status, mfaEnforced, attributes });
      setNotice("The profile was saved.");
      await load();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The profile was not saved." },
      );
    } finally {
      setBusy(false);
    }
  };

  const addBinding = async () => {
    if (!newRoleId || !detail) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await createIamBinding({
        subjectType: "user",
        subjectId: userId,
        roleId: newRoleId,
        scopeType,
        organizationId: scopeType === "global" ? null : detail.user.organization_id,
        siteId: scopeType === "site" ? scopeSite : null,
        department: scopeType === "department" ? scopeDepartment : undefined,
        module: scopeType === "module" ? scopeModule : undefined,
        resourceType: scopeType === "resource" ? "path" : undefined,
        resourceId: scopeType === "resource" ? scopeResource : undefined,
        expiresAt: expiresAt ? new Date(expiresAt).toISOString() : undefined,
      });
      setNotice("The role was attached.");
      setNewRoleId("");
      setExpiresAt("");
      await load();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The role was not attached." },
      );
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (binding: IamBinding) => {
    setBusy(true);
    setError(null);
    try {
      await revokeIamBinding(binding.id);
      setNotice("The role assignment was revoked.");
      await load();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The assignment was not revoked." },
      );
    } finally {
      setBusy(false);
    }
  };

  if (error && !detail) {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <UserCog className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The account is unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {error.message} <span className="font-mono text-[11.5px]">({error.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  if (!detail || !bindings) {
    return (
      <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
        <span className="sr-only">Reading the account…</span>
        {[0, 1, 2, 3].map((row) => (
          <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <Link href="/settings/iam/users" className="text-[12px] text-muted hover:text-ink">
            ← All accounts
          </Link>
          <h2 className="text-[15px] font-semibold" data-user-detail-title>
            {detail.user.display_name || detail.user.email}
          </h2>
          <p className="text-[12px] text-muted">
            {detail.user.email} · {detail.user.status}
            {detail.groups.length > 0
              ? ` · ${detail.groups.map((group) => group.name).join(", ")}`
              : ""}
          </p>
        </div>
        <div className="flex items-center gap-1.5" role="tablist" aria-label="Account sections">
          {(
            [
              ["profile", "Profile"],
              ["bindings", `Roles & bindings (${bindings.filter((entry) => entry.active).length})`],
              ["effective", `Effective permissions (${effective?.granted_count ?? 0})`],
            ] as [Tab, string][]
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={tab === id}
              data-user-tab={id}
              onClick={() => setTab(id)}
              className={`rounded-lg px-2.5 py-1.5 text-[12.5px] font-medium transition ${
                tab === id ? "bg-accent-soft text-accent-strong" : "text-muted hover:bg-quiet-soft"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {notice ? (
        <p role="status" data-user-detail-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-user-detail-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error.message} <span className="font-mono text-[11.5px]">({error.code})</span>
        </p>
      ) : null}

      {tab === "profile" ? (
        <section className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4" aria-label="Profile">
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Display name</span>
              <input
                value={displayName}
                data-user-profile-name
                onChange={(event) => setDisplayName(event.target.value)}
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Status</span>
              <select
                value={status}
                data-user-profile-status
                onChange={(event) => setStatus(event.target.value)}
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              >
                <option value="active">Active — may sign in</option>
                <option value="invited">Invited — no password yet</option>
                <option value="disabled">Disabled — refused at sign-in</option>
              </select>
            </label>
          </div>
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={mfaEnforced}
              data-user-profile-mfa
              onChange={(event) => setMfaEnforced(event.target.checked)}
              className="size-3.5 rounded border-line"
            />
            <span className="font-medium text-ink">Require MFA for this account</span>
            <span className="text-muted">(enrolment arrives with the security slice)</span>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Attributes (JSON)</span>
            <textarea
              value={attributesText}
              data-user-profile-attributes
              onChange={(event) => setAttributesText(event.target.value)}
              rows={4}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 font-mono text-[12px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            <span className="text-[11.5px] text-muted">
              Subject attributes for ABAC conditions (`department`, `region`, …).
            </span>
          </label>
          <div className="flex items-center gap-2">
            <button
              type="button"
              disabled={busy}
              data-user-profile-save
              data-qa-guard="iam-user-save"
              onClick={() => void saveProfile()}
              className="rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              Save profile
            </button>
          </div>
        </section>
      ) : null}

      {tab === "bindings" ? (
        <section className="flex flex-col gap-3" aria-label="Roles and bindings">
          <div className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
            <p className="text-[13px] font-semibold">Attach a role</p>
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Role</span>
                <select
                  value={newRoleId}
                  data-user-binding-role
                  onChange={(event) => setNewRoleId(event.target.value)}
                  className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                >
                  <option value="">Pick a role…</option>
                  {roles.map((role) => (
                    <option key={role.id} value={role.id}>
                      {role.name} ({role.key})
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Scope</span>
                <select
                  value={scopeType}
                  data-user-binding-scope
                  onChange={(event) => setScopeType(event.target.value as IamBinding["scope"]["type"])}
                  className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                >
                  <option value="global">Platform — everywhere</option>
                  <option value="organization">Organization</option>
                  <option value="site">Site</option>
                  <option value="department">Department</option>
                  <option value="module">Module</option>
                  <option value="resource">Resource path</option>
                </select>
              </label>
              <label className="flex flex-col gap-1.5">
                <span className="text-[12.5px] font-medium text-ink">Expires (optional)</span>
                <input
                  type="datetime-local"
                  value={expiresAt}
                  data-user-binding-expires
                  onChange={(event) => setExpiresAt(event.target.value)}
                  className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
              </label>
              {scopeType === "site" ? (
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Site</span>
                  <select
                    value={scopeSite}
                    data-user-binding-site
                    onChange={(event) => setScopeSite(event.target.value)}
                    className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  >
                    <option value="">Pick a site…</option>
                    {sites.map((site) => (
                      <option key={site.id} value={site.id}>
                        {site.name} ({site.key})
                      </option>
                    ))}
                  </select>
                </label>
              ) : null}
              {scopeType === "department" ? (
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Department</span>
                  <input
                    value={scopeDepartment}
                    data-user-binding-department
                    onChange={(event) => setScopeDepartment(event.target.value)}
                    placeholder="marketing"
                    className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                </label>
              ) : null}
              {scopeType === "module" ? (
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Module</span>
                  <input
                    value={scopeModule}
                    data-user-binding-module
                    onChange={(event) => setScopeModule(event.target.value)}
                    placeholder="content"
                    className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                </label>
              ) : null}
              {scopeType === "resource" ? (
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Path pattern</span>
                  <input
                    value={scopeResource}
                    data-user-binding-resource
                    onChange={(event) => setScopeResource(event.target.value)}
                    placeholder="/blog/*"
                    className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                </label>
              ) : null}
            </div>
            {scopeType === "resource" || scopeType === "module" || scopeType === "department" ? (
              <p className="rounded-lg border border-line bg-canvas px-3 py-2 text-[11.5px] text-muted">
                A finer scope applies only when the request carries it — a path glob matches the
                path a request names, never every request.
              </p>
            ) : null}
            <div>
              <button
                type="button"
                disabled={busy || !newRoleId || (scopeType === "site" && !scopeSite)}
                data-user-binding-add
                data-qa-guard="iam-binding-add"
                onClick={() => void addBinding()}
                className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
              >
                <Plus className="size-3.5" aria-hidden />
                Attach
              </button>
            </div>
          </div>

          {bindings.length === 0 ? (
            <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
              <ShieldCheck className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">No role is attached to this account</p>
              <p className="max-w-md text-[12.5px] text-muted">
                Without a binding the account holds nothing — access is denied by default.
              </p>
            </div>
          ) : (
            <div className="overflow-x-auto rounded-xl border border-line bg-surface">
              <table className="w-full text-left">
                <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                  <tr>
                    <th className="px-3 py-2 font-medium">Role</th>
                    <th className="px-3 py-2 font-medium">Scope</th>
                    <th className="px-3 py-2 font-medium">Expires</th>
                    <th className="px-3 py-2 font-medium">State</th>
                    <th className="px-3 py-2" />
                  </tr>
                </thead>
                <tbody>
                  {bindings.map((binding) => (
                    <tr key={binding.id} data-user-binding-row={binding.id} className="border-t border-line">
                      <td className="px-3 py-2.5">
                        <Link
                          href={`/settings/iam/roles/${binding.role_id}`}
                          className="text-[13px] font-medium text-ink hover:text-accent-strong"
                        >
                          {roles.find((role) => role.id === binding.role_id)?.name ?? binding.role_id.slice(0, 8)}
                        </Link>
                      </td>
                      <td className="px-3 py-2.5 font-mono text-[11.5px] text-muted">{scopeLabel(binding)}</td>
                      <td className="px-3 py-2.5 text-[12px] text-muted">
                        {binding.expires_at ? binding.expires_at.slice(0, 16).replace("T", " ") : "never"}
                      </td>
                      <td className="px-3 py-2.5">
                        <span
                          className={`rounded-md border px-1.5 py-0.5 text-[11px] font-medium ${
                            binding.active
                              ? "border-positive/40 bg-positive-soft text-positive"
                              : "border-line bg-canvas text-muted"
                          }`}
                        >
                          {binding.active ? "Active" : binding.revoked_at ? "Revoked" : "Expired"}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-right">
                        {binding.active ? (
                          <button
                            type="button"
                            data-user-binding-revoke={binding.id}
                            data-qa-guard="iam-binding-revoke"
                            disabled={busy}
                            onClick={() => void revoke(binding)}
                            className="inline-flex items-center gap-1 rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                          >
                            <Trash2 className="size-3" aria-hidden />
                            Revoke
                          </button>
                        ) : null}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>
      ) : null}

      {tab === "effective" ? (
        <section className="flex flex-col gap-3" aria-label="Effective permissions">
          <p className="text-[12.5px] text-muted">
            Resolved by the same function the route guard runs
            {effective ? ` — ${effective.granted_count} permission(s) held, ${effective.denied.length} refused.` : ""}
          </p>
          {effective && effective.granted.length === 0 && effective.denied.length === 0 ? (
            <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
              <KeyRound className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">Nothing is granted</p>
              <p className="max-w-md text-[12.5px] text-muted">
                The account holds no role; every request it makes is denied.
              </p>
            </div>
          ) : (
            <div className="overflow-x-auto rounded-xl border border-line bg-surface">
              {effective && effective.denied.length > 0 ? (
                <div className="border-b border-line bg-danger-soft px-3 py-2 text-[12px] text-caution">
                  {effective.denied.length} permission(s) are refused by an explicit deny — a deny
                  beats any allow.
                </div>
              ) : null}
              <table className="w-full text-left">
                <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                  <tr>
                    <th className="px-3 py-2 font-medium">Permission</th>
                    <th className="px-3 py-2 font-medium">Granted by</th>
                    <th className="px-3 py-2 font-medium">Via</th>
                  </tr>
                </thead>
                <tbody>
                  {effective?.denied.map((entry) => (
                    <tr key={`denied-${entry.key}`} data-effective-denied={entry.key} className="border-t border-line">
                      <td className="px-3 py-2 font-mono text-[12px] text-caution">{entry.key}</td>
                      <td className="px-3 py-2 text-[12.5px]">{entry.source.role_name}</td>
                      <td className="px-3 py-2 text-[12px] text-caution">{entry.source.via}</td>
                    </tr>
                  ))}
                  {effective?.granted.map((entry) => (
                    <tr key={entry.key} data-effective-granted={entry.key} className="border-t border-line">
                      <td className="px-3 py-2 font-mono text-[12px]">{entry.key}</td>
                      <td className="px-3 py-2 text-[12.5px]">
                        <Link
                          href={`/settings/iam/roles/${entry.source.role_id}`}
                          className="hover:text-accent-strong"
                        >
                          {entry.source.role_name}
                        </Link>
                      </td>
                      <td className="px-3 py-2 text-[12px] text-muted">{entry.source.via}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>
      ) : null}
    </div>
  );
}
