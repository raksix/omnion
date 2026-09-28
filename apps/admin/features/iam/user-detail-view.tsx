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

import { Fingerprint, KeyRound, Plus, RefreshCw, ShieldCheck, ShieldOff, Trash2, UserCog } from "lucide-react";
import Link from "next/link";

import { useSession } from "@/lib/session";
import {
  ApiError,
  beginPasskeyRegistration,
  completePasskeyRegistration,
  confirmIamTotp,
  createIamBinding,
  enrollIamTotp,
  fetchEffectivePermissions,
  fetchIamBindings,
  fetchIamFactors,
  fetchIamUser,
  fetchOrganizations,
  fetchPasskeys,
  fetchRoles,
  fetchSites,
  resetIamMfa,
  revokeIamBinding,
  revokeIamFactor,
  revokePasskey,
  updateIamUser,
  type IamBinding,
  type IamEffectivePermissions,
  type IamFactor,
  type IamFactorList,
  type IamRole,
  type IamUserDetail,
} from "@/lib/api";
import { ceremonyMessage, createPasskey, passkeysSupported } from "@/lib/webauthn";
import { StepUpPrompt } from "@/features/iam/step-up-prompt";
import {
  TenantPicker,
  isTenantRequired,
  tenantMissingReason,
  tenantRequiredMessage,
} from "@/components/tenant-picker";
import type { Organization, Site } from "@/lib/types";

type Tab = "profile" | "bindings" | "effective" | "factors";

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
  /**
   * The tenant a binding is made in, for an account that has none of its own.
   *
   * The subject's own organization is the right answer for everybody else and is used
   * automatically; this is the only way a platform account can say *where* a grant applies.
   * Without it, binding a role to a platform account at organization scope posts
   * `organization_id: null` and the API answers `400 organization_required` — a sentence about
   * a field the form does not have.
   */
  const [bindingOrganizationId, setBindingOrganizationId] = useState<string>("");
  const [organizations, setOrganizations] = useState<Organization[]>([]);
  // Second factors (REQ-006, slice 3).
  const [factors, setFactors] = useState<IamFactorList | null>(null);
  const [factorLabel, setFactorLabel] = useState("Authenticator app");
  const [enrolment, setEnrolment] = useState<{
    factor: IamFactor;
    secret: string;
    otpauth_uri: string;
  } | null>(null);
  const [confirmCode, setConfirmCode] = useState("");
  const [recoveryCodes, setRecoveryCodes] = useState<string[] | null>(null);
  const [factorBusy, setFactorBusy] = useState(false);
  // Passkeys (REQ-006, slice 3b) — enrolment only ever happens for the account at the keyboard.
  const [passkeys, setPasskeys] = useState<IamFactor[] | null>(null);
  const [passkeyBusy, setPasskeyBusy] = useState(false);
  const isSelf = user?.id === userId;
  // A refused dangerous action, parked until a step-up lets it run again.
  const [pendingAction, setPendingAction] = useState<{
    label: string;
    run: () => Promise<void>;
  } | null>(null);

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

  // The tenants a grant can be made in. Loaded for every account, because a subject that has
  // one of its own uses it implicitly while an operator still has to be able to read what the
  // picker would have offered — and a control that appears only in one case is a control whose
  // absence silently means "this is normal" in the other.
  useEffect(() => {
    void fetchOrganizations()
      .then(setOrganizations)
      .catch(() => setOrganizations([]));
  }, []);

  useEffect(() => {
    if (!detail) return;
    void fetchRoles(detail.user.organization_id).then(setRoles).catch(() => setRoles([]));
    if (detail.user.organization_id) {
      void fetchSites(detail.user.organization_id).then(setSites).catch(() => setSites([]));
    }
  }, [detail]);

  /** Read the account's factors (when the tab opens, and after every change). */
  const loadFactors = useCallback(async () => {
    try {
      setFactors(await fetchIamFactors(userId));
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The second factors could not be read." },
      );
    }
    // A passkey is the caller's own credential: the list is read from the self-service route,
    // and only for the account that is signed in.
    if (user?.id === userId) {
      try {
        setPasskeys((await fetchPasskeys()).passkeys);
      } catch {
        setPasskeys([]);
      }
    }
  }, [userId, user?.id]);

  useEffect(() => {
    if (tab !== "factors") return;
    void loadFactors();
  }, [tab, loadFactors]);

  /** Enrol a passkey on this device: the browser runs the ceremony, the API verifies it. */
  const enrolPasskey = async () => {
    setPasskeyBusy(true);
    setError(null);
    setNotice(null);
    try {
      const options = await beginPasskeyRegistration(factorLabel || "Passkey");
      const credential = await createPasskey(options);
      const body = await completePasskeyRegistration({
        challenge: options.challenge,
        label: factorLabel || "Passkey",
        credential,
      });
      setNotice(
        `The passkey is enrolled (${body.algorithm}). Sign-ins now ask for it — or for a code — before they open a session.`,
      );
      await loadFactors();
    } catch (cause) {
      setError({
        code: cause instanceof ApiError ? cause.code : "passkey_failed",
        message:
          cause instanceof ApiError ? cause.message : ceremonyMessage(cause),
      });
    } finally {
      setPasskeyBusy(false);
    }
  };

  /** Remove one of the caller's own passkeys (a step-up is demanded, like every factor). */
  const removePasskey = async (passkey: IamFactor) => {
    setPasskeyBusy(true);
    setError(null);
    setNotice(null);
    await withStepUp(`Remove the passkey "${passkey.label}"`, async () => {
      await revokePasskey(passkey.id);
      setNotice(`The passkey "${passkey.label}" was removed.`);
      await loadFactors();
    });
    setPasskeyBusy(false);
  };

  /**
   * Run a dangerous action; when the API answers `step_up_required`, park it and ask for a fresh
   * proof — the same action runs again once the session carries one.
   */
  const withStepUp = async (label: string, run: () => Promise<void>) => {
    try {
      await run();
    } catch (cause) {
      if (cause instanceof ApiError && cause.code === "step_up_required") {
        setPendingAction({ label, run });
        return;
      }
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The action failed." },
      );
    }
  };

  const startEnrolment = async () => {
    setFactorBusy(true);
    setError(null);
    setNotice(null);
    setRecoveryCodes(null);
    try {
      setEnrolment(await enrollIamTotp(userId, factorLabel));
      setConfirmCode("");
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The enrolment could not be started." },
      );
    } finally {
      setFactorBusy(false);
    }
  };

  const confirmEnrolment = async () => {
    if (!enrolment) return;
    setFactorBusy(true);
    setError(null);
    try {
      const body = await confirmIamTotp(userId, enrolment.factor.id, confirmCode);
      setRecoveryCodes(body.recovery_codes);
      setEnrolment(null);
      setConfirmCode("");
      setNotice("The factor is confirmed. Store the recovery codes — they are shown once.");
      await loadFactors();
    } catch (cause) {
      setError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The code could not be confirmed." },
      );
    } finally {
      setFactorBusy(false);
    }
  };

  const removeFactor = async (factor: IamFactor) => {
    setFactorBusy(true);
    setError(null);
    setNotice(null);
    await withStepUp(`Remove the ${factor.kind} factor`, async () => {
      await revokeIamFactor(userId, factor.id);
      setNotice(`The ${factor.kind} factor was removed.`);
      await loadFactors();
    });
    setFactorBusy(false);
  };

  const resetFactors = async () => {
    setFactorBusy(true);
    setError(null);
    setNotice(null);
    await withStepUp("Reset every second factor of this account", async () => {
      const body = await resetIamMfa(userId);
      setNotice(
        `Every factor was cleared (${body.factors_revoked} removed); the account signs in with its password again.`,
      );
      setRecoveryCodes(null);
      await loadFactors();
    });
    setFactorBusy(false);
  };

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
    // The tenant the grant is made in: the subject's own when it has one, otherwise the one
    // this form picked. `global` scope carries no tenant at all — it is the scope where
    // "no organization" is the *point*, so it must not be refused for lacking one.
    const needsTenant = scopeType !== "global";
    const organizationId = detail.user.organization_id ?? (bindingOrganizationId || null);
    if (needsTenant && !organizationId) {
      setError({
        code: "organization_required",
        message:
          "This account works platform-wide, so it has no tenant of its own. Choose the organization the grant applies to.",
      });
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await createIamBinding({
        subjectType: "user",
        subjectId: userId,
        roleId: newRoleId,
        scopeType,
        organizationId: scopeType === "global" ? null : organizationId,
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
      // The API's own refusal, rendered as guidance rather than as a sentence about a field
      // this form used not to have.
      if (isTenantRequired(cause)) {
        setError({ code: "organization_required", message: tenantRequiredMessage(cause)! });
        return;
      }
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
              ["factors", `Second factors (${factors?.confirmed ?? 0})`],
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
              {/*
               * The tenant a non-global grant is made in. Rendered for a subject that has no
               * organization of its own — a platform account — because for everybody else the
               * subject's own tenant is the only correct answer and asking would be noise. In
               * `global` scope there is deliberately nothing: "no tenant" is what that scope
               * *means*, and a picker there would invite a reader to narrow a grant that is
               *meant* to be platform-wide.
               */}
              {detail && detail.user.organization_id === null && scopeType !== "global" ? (
                <div className="sm:col-span-2">
                  <TenantPicker
                    platformAccount
                    organizations={organizations}
                    value={bindingOrganizationId || null}
                    onChange={setBindingOrganizationId}
                    testId="user-binding-organization"
                    label="Grant applies to"
                  />
                </div>
              ) : null}
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

      {tab === "factors" ? (
        <section className="flex flex-col gap-3" aria-label="Second factors">
          <div className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-line bg-surface p-4">
            <div className="max-w-xl">
              <h2 className="text-[13px] font-medium text-ink">Second factors</h2>
              <p className="text-[12px] text-muted">
                An authenticator app (TOTP) and single-use recovery codes. A confirmed factor is
                demanded at sign-in; removing one or resetting the account needs a fresh proof of
                your own identity.
              </p>
              <p className="mt-1 text-[12px] text-muted" data-factors-recovery-remaining>
                {factors?.recovery_codes_remaining ?? 0} recovery codes left
              </p>
            </div>
            <div className="flex items-center gap-2">
              <button
                type="button"
                disabled={factorBusy || Boolean(enrolment)}
                data-factor-enrol-start
                onClick={() => void startEnrolment()}
                className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
              >
                <Fingerprint className="size-3.5" aria-hidden />
                Enrol an authenticator
              </button>
              <button
                type="button"
                disabled={factorBusy || (factors?.factors.length ?? 0) === 0}
                data-factors-reset
                data-qa-guard="mfa-reset"
                onClick={() => void resetFactors()}
                className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-caution transition hover:bg-panel disabled:opacity-50"
              >
                <ShieldOff className="size-3.5" aria-hidden />
                Reset all
              </button>
            </div>
          </div>

          <label className="flex w-fit flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Label for a new factor</span>
            <input
              value={factorLabel}
              data-factor-label
              onChange={(event) => setFactorLabel(event.target.value)}
              className="h-9 w-64 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>

          {enrolment ? (
            <div
              data-factor-enrolment
              className="flex flex-col gap-2 rounded-xl border border-accent/40 bg-accent-soft/40 p-4"
            >
              <h3 className="text-[12.5px] font-medium text-ink">
                Add this secret to the authenticator app
              </h3>
              <p className="text-[12px] text-muted">
                Scan the link or type the secret; it is shown exactly once.
              </p>
              <code
                data-factor-secret
                className="w-fit rounded-lg border border-line bg-surface px-2 py-1 font-mono text-[12.5px] text-ink"
              >
                {enrolment.secret}
              </code>
              <code
                data-factor-uri
                className="break-all rounded-lg border border-line bg-surface px-2 py-1 font-mono text-[11.5px] text-muted"
              >
                {enrolment.otpauth_uri}
              </code>
              <div className="flex flex-wrap items-end gap-2">
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Code from the app</span>
                  <input
                    value={confirmCode}
                    data-factor-confirm-code
                    inputMode="numeric"
                    autoComplete="one-time-code"
                    placeholder="123456"
                    onChange={(event) => setConfirmCode(event.target.value)}
                    className="h-9 w-32 rounded-lg border border-line bg-surface px-2 font-mono text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                </label>
                <button
                  type="button"
                  disabled={factorBusy || confirmCode.trim().length === 0}
                  data-factor-confirm
                  data-qa-guard="factor-confirm"
                  onClick={() => void confirmEnrolment()}
                  className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
                >
                  <KeyRound className="size-3.5" aria-hidden />
                  Confirm
                </button>
                <button
                  type="button"
                  disabled={factorBusy}
                  data-factor-enrol-cancel
                  onClick={() => {
                    setEnrolment(null);
                    setConfirmCode("");
                  }}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:bg-panel"
                >
                  Cancel
                </button>
              </div>
            </div>
          ) : null}

          {recoveryCodes ? (
            <div
              data-factor-recovery-codes
              className="flex flex-col gap-2 rounded-xl border border-line bg-quiet-soft p-4"
            >
              <h3 className="text-[12.5px] font-medium text-ink">
                Recovery codes — copy them now, they are shown once
              </h3>
              <ul className="grid grid-cols-2 gap-1.5 sm:grid-cols-5">
                {recoveryCodes.map((code) => (
                  <li
                    key={code}
                    className="rounded-lg border border-line bg-surface px-2 py-1 font-mono text-[12.5px] text-ink"
                  >
                    {code}
                  </li>
                ))}
              </ul>
              <button
                type="button"
                data-factor-recovery-done
                onClick={() => setRecoveryCodes(null)}
                className="w-fit rounded-lg border border-line px-3 py-1 text-[12px] text-ink transition hover:bg-panel"
              >
                I have stored them
              </button>
            </div>
          ) : null}

          {factors && factors.factors.length === 0 ? (
            <div
              data-factors-empty
              className="rounded-xl border border-dashed border-line bg-surface p-6 text-center text-[12.5px] text-muted"
            >
              No second factor yet. Enrolling one asks for a code at every sign-in — and no longer
              from the password alone.
            </div>
          ) : null}

          {factors && factors.factors.length > 0 ? (
            <div className="overflow-x-auto rounded-xl border border-line bg-surface">
              <table className="w-full min-w-[640px] text-left text-[12.5px]">
                <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <tr>
                    <th className="px-3 py-2">Kind</th>
                    <th className="px-3 py-2">Label</th>
                    <th className="px-3 py-2">State</th>
                    <th className="px-3 py-2">Last used</th>
                    <th className="px-3 py-2" />
                  </tr>
                </thead>
                <tbody>
                  {factors.factors.map((factor) => (
                    <tr
                      key={factor.id}
                      data-factor-row={factor.kind}
                      className="border-b border-line/60 last:border-0"
                    >
                      <td className="px-3 py-2 font-mono text-[12px] text-ink">{factor.kind}</td>
                      <td className="px-3 py-2 text-ink">{factor.label}</td>
                      <td className="px-3 py-2">
                        {factor.confirmed ? (
                          <span className="rounded-full border border-emerald-500/40 bg-emerald-500/10 px-2 py-0.5 text-[11px] text-emerald-700">
                            confirmed
                          </span>
                        ) : (
                          <span className="rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 text-[11px] text-amber-700">
                            waiting for a code
                          </span>
                        )}
                      </td>
                      <td className="px-3 py-2 text-muted">
                        {factor.last_used_at
                          ? new Date(factor.last_used_at).toLocaleString()
                          : "never"}
                      </td>
                      <td className="px-3 py-2 text-right">
                        <button
                          type="button"
                          disabled={factorBusy}
                          data-factor-remove
                          data-qa-guard="factor-remove"
                          onClick={() => void removeFactor(factor)}
                          className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-panel"
                        >
                          Remove
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : null}

          {/* Passkeys: a credential the browser holds, verified by its own signature. */}
          <section
            data-passkeys
            aria-label="Passkeys"
            className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
          >
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="max-w-xl">
                <h2 className="text-[13px] font-medium text-ink">Passkeys (WebAuthn)</h2>
                <p className="text-[12px] text-muted">
                  {isSelf
                    ? "A passkey lives in this device's authenticator (Windows Hello, Touch ID, a security key). The browser signs a challenge and this installation verifies the signature — no shared secret ever reaches the server."
                    : "A passkey belongs to the account at the keyboard, so only the account holder can enrol one. As an administrator you can remove one, or reset every factor."}
                </p>
              </div>
              {isSelf ? (
                <button
                  type="button"
                  disabled={passkeyBusy || !passkeysSupported()}
                  data-passkey-enrol
                  onClick={() => void enrolPasskey()}
                  className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
                >
                  <Fingerprint className="size-3.5" aria-hidden />
                  {passkeyBusy ? "Waiting for the authenticator…" : "Add a passkey"}
                </button>
              ) : null}
            </div>

            {isSelf && !passkeysSupported() ? (
              <p data-passkey-unsupported className="text-[12px] text-caution">
                This browser cannot run a passkey ceremony.
              </p>
            ) : null}

            {passkeys && passkeys.length === 0 ? (
              <p data-passkeys-empty className="text-[12.5px] text-muted">
                No passkey on this account yet.
              </p>
            ) : null}

            {passkeys && passkeys.length > 0 ? (
              <ul className="flex flex-col gap-1.5">
                {passkeys.map((passkey) => (
                  <li
                    key={passkey.id}
                    data-passkey-row={passkey.id}
                    className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-line bg-panel px-3 py-2"
                  >
                    <span className="flex flex-col">
                      <span className="text-[12.5px] text-ink">{passkey.label}</span>
                      <span className="text-[11.5px] text-muted">
                        added {new Date(passkey.created_at).toLocaleString()} · last used{" "}
                        {passkey.last_used_at
                          ? new Date(passkey.last_used_at).toLocaleString()
                          : "never"}
                      </span>
                    </span>
                    {isSelf ? (
                      <button
                        type="button"
                        disabled={passkeyBusy}
                        data-passkey-remove={passkey.id}
                        data-qa-guard="passkey-remove"
                        onClick={() => void removePasskey(passkey)}
                        className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-surface"
                      >
                        Remove
                      </button>
                    ) : null}
                  </li>
                ))}
              </ul>
            ) : null}
          </section>
        </section>
      ) : null}

      <StepUpPrompt
        open={pendingAction !== null}
        action={pendingAction?.label ?? ""}
        onClose={() => setPendingAction(null)}
        onDone={() => {
          const pending = pendingAction;
          setPendingAction(null);
          if (pending) {
            void pending.run().catch(() => {
              setError({
                code: "step_up_retry_failed",
                message: "The action could not be completed.",
              });
            });
          }
        }}
      />
    </div>
  );
}
