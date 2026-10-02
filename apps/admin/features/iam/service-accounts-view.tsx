"use client";

/**
 * `/settings/iam/service-accounts` — machine identities and their keys (REQ-006, slice 2).
 *
 * A key is shown exactly once: the screen copies the token the API answered with into a callout
 * that has no way back — after this view closes, only the prefix stays. Roles attach to the
 * identity the same way they attach to a person.
 */
import { useCallback, useEffect, useState } from "react";

import { Bot, Copy, KeyRound, Plus, RefreshCw, ShieldCheck, Trash2, X } from "lucide-react";

import { useSession } from "@/lib/session";
import { StepUpPrompt } from "@/features/iam/step-up-prompt";
import { tenantMissingReason, tenantRequiredMessage } from "@/components/tenant-picker";
import {
  ApiError,
  createIamBinding,
  createIamServiceAccount,
  deleteIamServiceAccount,
  fetchIamServiceAccount,
  fetchIamServiceAccounts,
  fetchOrganizations,
  fetchRoles,
  issueIamServiceAccountKey,
  revokeIamBinding,
  revokeIamServiceAccountKey,
  type IamRole,
  type IamServiceAccount,
  type IamServiceAccountDetail,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** `/settings/iam/service-accounts`. */
export function ServiceAccountsView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [accounts, setAccounts] = useState<IamServiceAccount[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [issueFirstKey, setIssueFirstKey] = useState(true);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [issuedToken, setIssuedToken] = useState<{ name: string; token: string } | null>(null);
  // A refused dangerous action, parked until a step-up lets it run again.
  const [stepUpAction, setStepUpAction] = useState<string | null>(null);
  // The open identity.
  const [openId, setOpenId] = useState<string | null>(null);
  const [detail, setDetail] = useState<IamServiceAccountDetail | null>(null);
  const [roles, setRoles] = useState<IamRole[]>([]);
  const [roleToAttach, setRoleToAttach] = useState("");
  const [keyLabel, setKeyLabel] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;
  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  const load = useCallback(async (organizationId: string | null) => {
    setStatus("loading");
    setLoadError(null);
    try {
      setAccounts(await fetchIamServiceAccounts(organizationId));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The service accounts could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void load(null);
      return;
    }
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, load]);

  useEffect(() => {
    if (platformAccount && selectedOrg) {
      void load(selectedOrg);
    }
  }, [platformAccount, selectedOrg, load]);

  const openAccount = useCallback(
    async (accountId: string) => {
      setBusy(true);
      setError(null);
      try {
        const [identity, roleList] = await Promise.all([
          fetchIamServiceAccount(accountId),
          fetchRoles(activeOrg),
        ]);
        setDetail(identity);
        setRoles(roleList);
        setOpenId(accountId);
      } catch (cause) {
        setError(
          cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The identity could not be opened.",
        );
      } finally {
        setBusy(false);
      }
    },
    [activeOrg],
  );

  const create = async () => {
    // A platform account has no tenant of its own, so the create has nothing to write into
    // until one is chosen. Refusing here rather than sending the request keeps the API's
    // `400 organization_required` as the *last* line of defence rather than the first thing a
    // reader meets — and the sentence it would have shown is the one shown here.
    const missing = tenantMissingReason(platformAccount, activeOrg);
    if (missing) {
      setError(missing);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await createIamServiceAccount({
        name,
        description,
        organizationId: activeOrg,
        keyLabel: issueFirstKey ? "first key" : undefined,
      });
      setName("");
      setDescription("");
      setCreating(false);
      setNotice(`“${created.name}” was created.`);
      if (created.key) {
        setIssuedToken({ name: created.name, token: created.key });
      }
      await load(activeOrg);
      await openAccount(created.id);
    } catch (cause) {
      // A platform account whose tenant was not chosen gets the API's own refusal back — and
      // this is the second place it can arrive from, the first being the guard above, because
      // a form submitted by keyboard, a walkthrough, or a stale render can still reach the
      // network without `activeOrg`.
      setError(
        tenantRequiredMessage(cause) ??
          (cause instanceof ApiError
            ? `${cause.message} (${cause.code})`
            : "The identity was not created."),
      );
    } finally {
      setBusy(false);
    }
  };

  /**
   * Issue a key.
   *
   * Issuing a credential is a dangerous operation (REQ-006, slice 3): the API demands a fresh
   * step-up, so a refusal parks the action and the prompt below proves identity before it runs
   * again.
   */
  const issueKey = async () => {
    if (!openId || !detail) return;
    setBusy(true);
    setError(null);
    try {
      const issued = await issueIamServiceAccountKey(openId, { label: keyLabel || "key" });
      setIssuedToken({ name: detail.account.name, token: issued.token });
      setKeyLabel("");
      await openAccount(openId);
    } catch (cause) {
      if (cause instanceof ApiError && cause.code === "step_up_required") {
        setStepUpAction("Issue a machine key");
        return;
      }
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The key was not issued.",
      );
    } finally {
      setBusy(false);
    }
  };

  const revokeKey = async (keyId: string) => {
    if (!openId) return;
    setBusy(true);
    setError(null);
    try {
      await revokeIamServiceAccountKey(openId, keyId);
      setNotice("The key was revoked — a request that presents it is refused from now on.");
      await openAccount(openId);
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The key was not revoked.",
      );
    } finally {
      setBusy(false);
    }
  };

  const attachRole = async () => {
    if (!openId || !roleToAttach || !detail) return;
    setBusy(true);
    setError(null);
    try {
      await createIamBinding({
        subjectType: "service_account",
        subjectId: openId,
        roleId: roleToAttach,
        scopeType: "organization",
        organizationId: detail.account.organization_id,
      });
      setRoleToAttach("");
      setNotice("The role was attached to the machine identity.");
      await openAccount(openId);
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The role was not attached.",
      );
    } finally {
      setBusy(false);
    }
  };

  const remove = async (accountId: string) => {
    setBusy(true);
    setError(null);
    try {
      await deleteIamServiceAccount(accountId);
      setNotice("The identity was deleted; its keys and roles went with it.");
      setConfirmDelete(null);
      if (openId === accountId) {
        setOpenId(null);
        setDetail(null);
      }
      await load(activeOrg);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The identity was not deleted.",
      );
    } finally {
      setBusy(false);
    }
  };

  if (status === "error") {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <Bot className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The service accounts are unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load(activeOrg)}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-sa-organization
              onChange={(event) => setSelectedOrg(event.target.value)}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              {organizations.map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : (
          <span />
        )}
        <p className="text-[12.5px] text-muted">
          {accounts.length} machine identit{accounts.length === 1 ? "y" : "ies"} — a key is shown
          once, when it is issued.
        </p>
        <button
          type="button"
          onClick={() => setCreating(true)}
          data-sa-create-open
          className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          New identity
        </button>
      </div>

      {creating ? (
        <form
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
          data-sa-create-form
          onSubmit={(event) => {
            event.preventDefault();
            void create();
          }}
        >
          <p className="text-[13px] font-semibold">New service account</p>
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Name</span>
              <input
                value={name}
                data-sa-new-name
                onChange={(event) => setName(event.target.value)}
                placeholder="CI runner"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Description</span>
              <input
                value={description}
                data-sa-new-description
                onChange={(event) => setDescription(event.target.value)}
                placeholder="What it is for"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={issueFirstKey}
              data-sa-new-first-key
              onChange={(event) => setIssueFirstKey(event.target.checked)}
              className="size-3.5 rounded border-line"
            />
            <span>Issue its first key now (the token is shown once)</span>
          </label>
          <div className="flex items-center gap-2">
            <button
              type="submit"
              disabled={busy || !name.trim()}
              data-sa-create-submit
              data-qa-guard="iam-sa-create"
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              Create identity
            </button>
            <button
              type="button"
              onClick={() => setCreating(false)}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}

      {issuedToken ? (
        <div
          role="status"
          data-sa-token-callout
          className="flex flex-col gap-2 rounded-xl border border-accent/40 bg-accent-soft p-3"
        >
          <p className="text-[12.5px] font-medium text-accent-strong">
            Key for “{issuedToken.name}” — copy it now; the server keeps only its hash.
          </p>
          <div className="flex items-center gap-2">
            <code className="flex-1 overflow-x-auto rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px]" data-sa-token>
              {issuedToken.token}
            </code>
            <button
              type="button"
              onClick={() => {
                void navigator.clipboard?.writeText(issuedToken.token);
                setNotice("The key was copied to the clipboard.");
              }}
              className="flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
            >
              <Copy className="size-3" aria-hidden />
              Copy
            </button>
            <button
              type="button"
              data-sa-token-dismiss
              onClick={() => setIssuedToken(null)}
              aria-label="Hide the key"
              className="rounded-lg border border-line bg-surface p-1.5 text-muted transition hover:bg-quiet-soft"
            >
              <X className="size-3.5" aria-hidden />
            </button>
          </div>
        </div>
      ) : null}

      {notice ? (
        <p role="status" data-sa-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p role="alert" data-sa-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      <div className="flex flex-col gap-4 lg:flex-row">
        <div className="flex-1 overflow-x-auto rounded-xl border border-line bg-surface">
          {status === "loading" && accounts.length === 0 ? (
            <div className="p-4" aria-live="polite">
              {[0, 1, 2].map((row) => (
                <div key={row} className="mb-2 h-10 animate-pulse rounded-lg bg-quiet-soft" />
              ))}
            </div>
          ) : accounts.length === 0 ? (
            <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
              <Bot className="size-4 text-muted" aria-hidden />
              <p className="text-[13.5px] font-medium">No machine identity yet</p>
              <p className="max-w-md text-[12.5px] text-muted">
                Service accounts exist for integrations and scripts: create one, attach the roles it
                needs, and issue a key.
              </p>
            </div>
          ) : (
            <table className="w-full text-left">
              <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                <tr>
                  <th className="px-3 py-2 font-medium">Identity</th>
                  <th className="px-3 py-2 font-medium">Prefix</th>
                  <th className="px-3 py-2 text-right font-medium">Keys</th>
                  <th className="px-3 py-2 text-right font-medium">Roles</th>
                  <th className="px-3 py-2 font-medium">Last used</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {accounts.map((account) => (
                  <tr key={account.id} data-sa-row={account.name} className="border-t border-line">
                    <td className="px-3 py-2.5">
                      <button
                        type="button"
                        onClick={() => void openAccount(account.id)}
                        data-sa-open={account.name}
                        className="text-left text-[13px] font-medium text-ink hover:text-accent-strong"
                      >
                        {account.name}
                      </button>
                      <p className="text-[11.5px] text-muted">{account.description || "no description"}</p>
                    </td>
                    <td className="px-3 py-2.5 font-mono text-[11.5px] text-muted">{account.prefix}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12.5px]">{account.active_keys}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12.5px]">{account.role_count}</td>
                    <td className="px-3 py-2.5 text-[12px] text-muted">
                      {account.last_used_at ? account.last_used_at.slice(0, 16).replace("T", " ") : "never"}
                    </td>
                    <td className="px-3 py-2.5 text-right">
                      {confirmDelete === account.id ? (
                        <span className="flex items-center justify-end gap-1">
                          <button
                            type="button"
                            data-sa-delete-confirm={account.name}
                            data-qa-guard="iam-sa-delete"
                            disabled={busy}
                            onClick={() => void remove(account.id)}
                            className="rounded-lg border border-danger/50 bg-danger-soft px-2.5 py-1 text-[12px] font-medium text-caution transition hover:bg-danger/20 disabled:opacity-60"
                          >
                            Confirm delete
                          </button>
                          <button
                            type="button"
                            onClick={() => setConfirmDelete(null)}
                            className="rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-quiet-soft"
                          >
                            Keep
                          </button>
                        </span>
                      ) : (
                        <span className="flex items-center justify-end gap-1.5">
                          <button
                            type="button"
                            onClick={() => void openAccount(account.id)}
                            className="rounded-lg border border-line px-2.5 py-1 text-[12px] font-medium transition hover:bg-quiet-soft"
                          >
                            Open
                          </button>
                          <button
                            type="button"
                            data-sa-delete={account.name}
                            onClick={() => setConfirmDelete(account.id)}
                            className="rounded-lg border border-line px-2 py-1 text-[12px] transition hover:bg-quiet-soft"
                          >
                            <Trash2 className="size-3" aria-hidden />
                          </button>
                        </span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        {openId && detail ? (
          <aside className="flex w-full flex-col gap-3 rounded-xl border border-line bg-surface p-4 lg:w-[400px]" data-sa-panel>
            <div className="flex items-start justify-between gap-2">
              <div>
                <p className="text-[13.5px] font-semibold" data-sa-panel-name>
                  {detail.account.name}
                </p>
                <p className="font-mono text-[11.5px] text-muted">omsa_{detail.account.prefix}_…</p>
              </div>
              <button
                type="button"
                onClick={() => {
                  setOpenId(null);
                  setDetail(null);
                }}
                aria-label="Close the identity panel"
                className="rounded-lg border border-line p-1 text-muted transition hover:bg-quiet-soft"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            </div>

            <div className="flex flex-col gap-1.5">
              <p className="text-[12.5px] font-medium text-ink">Keys ({detail.keys.length})</p>
              {detail.keys.length === 0 ? (
                <p className="text-[12px] text-muted">No key yet — the identity cannot authenticate.</p>
              ) : (
                <ul className="flex flex-col gap-1" data-sa-keys>
                  {detail.keys.map((key) => (
                    <li
                      key={key.id}
                      data-sa-key={key.prefix}
                      className="flex items-center justify-between gap-2 rounded-lg border border-line px-2.5 py-1.5"
                    >
                      <span className="flex flex-col">
                        <span className="font-mono text-[11.5px]">
                          {key.prefix} · {key.label || "key"}
                        </span>
                        <span className="text-[11px] text-muted">
                          {key.active
                            ? key.last_used_at
                              ? `used ${key.last_used_at.slice(0, 16).replace("T", " ")}`
                              : "never used"
                            : key.revoked_at
                              ? "revoked"
                              : "expired"}
                        </span>
                      </span>
                      {key.active ? (
                        <button
                          type="button"
                          data-sa-key-revoke={key.prefix}
                          data-qa-guard="iam-sa-key-revoke"
                          disabled={busy}
                          onClick={() => void revokeKey(key.id)}
                          className="rounded-lg border border-line px-2 py-0.5 text-[11.5px] transition hover:bg-quiet-soft disabled:opacity-60"
                        >
                          Revoke
                        </button>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )}
              <div className="flex items-center gap-2">
                <input
                  value={keyLabel}
                  data-sa-key-label
                  onChange={(event) => setKeyLabel(event.target.value)}
                  placeholder="Label (deploy, ci…)"
                  className="h-8 flex-1 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
                />
                <button
                  type="button"
                  disabled={busy}
                  data-sa-key-issue
                  data-qa-guard="iam-sa-key-issue"
                  onClick={() => void issueKey()}
                  className="flex items-center gap-1 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                >
                  <KeyRound className="size-3" aria-hidden />
                  Issue key
                </button>
              </div>
            </div>

            <div className="flex flex-col gap-1.5 border-t border-line pt-3">
              <p className="text-[12.5px] font-medium text-ink">Roles ({detail.roles.filter((entry) => entry.active).length})</p>
              {detail.roles.length === 0 ? (
                <p className="text-[12px] text-muted">
                  Nothing is attached — the identity authenticates but may do nothing.
                </p>
              ) : (
                <ul className="flex flex-col gap-1">
                  {detail.roles.map((binding) => (
                    <li
                      key={binding.id}
                      data-sa-role={binding.id}
                      className="flex items-center justify-between gap-2 rounded-lg border border-line px-2.5 py-1.5"
                    >
                      <span className="text-[12.5px]">
                        {roles.find((role) => role.id === binding.role_id)?.name ?? binding.role_id.slice(0, 8)}
                        <span className="ml-1.5 font-mono text-[11px] text-muted">{binding.scope.type}</span>
                      </span>
                      {binding.active ? (
                        <button
                          type="button"
                          data-sa-role-revoke={binding.id}
                          data-qa-guard="iam-sa-role-revoke"
                          disabled={busy}
                          onClick={() => {
                            void (async () => {
                              setBusy(true);
                              setError(null);
                              try {
                                await revokeIamBinding(binding.id);
                                setNotice("The role was detached from the identity.");
                                await openAccount(openId);
                                await load(activeOrg);
                              } catch (cause) {
                                setError(
                                  cause instanceof ApiError
                                    ? `${cause.message} (${cause.code})`
                                    : "The role was not detached.",
                                );
                              } finally {
                                setBusy(false);
                              }
                            })();
                          }}
                          className="rounded-lg border border-line px-2 py-0.5 text-[11.5px] transition hover:bg-quiet-soft disabled:opacity-60"
                        >
                          Detach
                        </button>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )}
              <div className="flex items-center gap-2">
                <select
                  value={roleToAttach}
                  data-sa-role-select
                  onChange={(event) => setRoleToAttach(event.target.value)}
                  className="h-8 flex-1 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent"
                >
                  <option value="">Pick a role…</option>
                  {roles.map((role) => (
                    <option key={role.id} value={role.id}>
                      {role.name} ({role.key})
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  disabled={busy || !roleToAttach}
                  data-sa-role-attach
                  data-qa-guard="iam-sa-role-attach"
                  onClick={() => void attachRole()}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft disabled:opacity-60"
                >
                  Attach
                </button>
              </div>
            </div>

            <p className="flex items-start gap-1.5 rounded-lg border border-line bg-canvas px-2.5 py-2 text-[11.5px] text-muted">
              <ShieldCheck className="mt-0.5 size-3 shrink-0" aria-hidden />
              A key authenticates over `Authorization: Bearer`; it can never open an interactive
              session.
            </p>
          </aside>
        ) : null}
      </div>

      <StepUpPrompt
        open={stepUpAction !== null}
        action={stepUpAction ?? ""}
        onClose={() => setStepUpAction(null)}
        onDone={() => {
          setStepUpAction(null);
          void issueKey();
        }}
      />
    </div>
  );
}
