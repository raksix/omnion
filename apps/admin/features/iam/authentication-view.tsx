"use client";

/**
 * `/settings/iam/authentication` — the connected sign-in providers (REQ-006, slice 4b-2;
 * docs/07-IAM.md §11).
 *
 * A screen about three things an administrator has to be able to answer without reading a
 * deployment guide: **is this provider working**, **what does it grant**, and **where does its
 * secret live**. That order drives the layout — the list first, the drawer second, the sign-in
 * log under the drawer.
 *
 * Two rules the screen enforces rather than merely documents:
 *
 * * **A secret is a name, never a value.** The client credential lives in the environment; the
 *   screen edits the *name* and shows a green/red "defined / not defined" chip derived from
 *   whether this installation has that variable. So an operator can see a broken provider is
 *   broken because the secret is missing, without any code path that could display the value.
 * * **A provider is created switched off.** `Test connection` is how it is proved, `Enable` is how
 *   it is published, and a half-configured provider is never reachable by a person trying to sign
 *   in. Local sign-in is untouched by everything on this screen and cannot be turned off here.
 */
import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  CircleSlash,
  KeyRound,
  Link2,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
  XCircle,
} from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamProvider,
  deleteIamProvider,
  fetchIamProviderEvents,
  fetchIamProviders,
  fetchOrganizations,
  testIamProvider,
  updateIamProvider,
  type IamAuthProvider,
  type IamProviderEvent,
  type IamProviderTest,
} from "@/lib/api";

/** Kind labels, in the order the form offers them. */
const KIND_LABELS: Record<string, string> = {
  oidc: "OpenID Connect",
  oauth2: "OAuth 2.0",
  saml: "SAML 2.0",
};

/** What a SAML provider needs instead of a discovery document. */
const SAML_FIELDS = [
  { key: "issuer", label: "Issuer (entity ID)" },
  { key: "audience", label: "Audience" },
  { key: "certificate_pem", label: "Signing certificate (PEM)" },
  { key: "email_attribute", label: "E-mail attribute" },
  { key: "group_attribute", label: "Group attribute" },
  { key: "display_name_attribute", label: "Display name attribute" },
] as const;

/** What an OIDC/OAuth2 provider needs. */
const OIDC_FIELDS = [
  { key: "issuer", label: "Issuer", hint: "Discovery is read from it" },
  { key: "client_id", label: "Client ID" },
  { key: "authorization_endpoint", label: "Authorization endpoint", optional: true },
  { key: "token_endpoint", label: "Token endpoint", optional: true },
  { key: "jwks_uri", label: "JWKS URI", optional: true },
  { key: "userinfo_endpoint", label: "Userinfo endpoint", optional: true },
  { key: "redirect_uri", label: "Redirect URI", optional: true, hint: "Defaults to this installation" },
  { key: "email_claim", label: "E-mail claim", optional: true },
] as const;

/** Outcome colours of the sign-in log. */
const OUTCOME_STYLES: Record<string, string> = {
  success: "border-emerald-500/40 bg-emerald-500/10 text-emerald-700",
  provisioned: "border-sky-500/40 bg-sky-500/10 text-sky-700",
  updated: "border-line bg-panel text-muted",
  refused: "border-danger/40 bg-danger-soft text-caution",
  error: "border-danger/40 bg-danger-soft text-caution",
};

/** The draft a provider form holds. */
type Draft = {
  id: string | null;
  slug: string;
  kind: "oidc" | "oauth2" | "saml";
  name: string;
  secretRef: string;
  groupClaim: string;
  jitEnabled: boolean;
  enabled: boolean;
  config: Record<string, string>;
};

/** An empty form for `kind`. */
function emptyDraft(kind: Draft["kind"] = "oidc"): Draft {
  return {
    id: null,
    slug: "",
    kind,
    name: "",
    secretRef: "",
    groupClaim: kind === "saml" ? "groups" : "groups",
    // Provisioning is off until it is asked for: a provider that silently creates accounts is a
    // provider that can be used to fill an organization with strangers.
    jitEnabled: false,
    // And a new provider is created switched off — `test` proves it, `enabled` publishes it.
    enabled: false,
    config: {},
  };
}

/** A draft from a stored row. */
function draftOf(provider: IamAuthProvider): Draft {
  const config: Record<string, string> = {};
  for (const [key, value] of Object.entries(provider.config ?? {})) {
    if (typeof value === "string") config[key] = value;
  }
  return {
    id: provider.id,
    slug: provider.slug,
    kind: provider.kind,
    name: provider.name,
    secretRef: provider.secret_ref ?? "",
    groupClaim: provider.group_claim ?? "",
    jitEnabled: provider.jit_enabled,
    enabled: provider.enabled,
    config,
  };
}

/** `/settings/iam/authentication`. */
export function AuthenticationView() {
  const { user } = useSession();
  const [providers, setProviders] = useState<IamAuthProvider[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [draft, setDraft] = useState<Draft | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [tests, setTests] = useState<Record<string, IamProviderTest>>({});
  const [events, setEvents] = useState<Record<string, IamProviderEvent[]>>({});
  const [showLog, setShowLog] = useState<string | null>(null);

  // A platform account names the organization it manages providers for; an organization account
  // never sees the picker, because it can only ever work inside its own.
  const platformAccount = user ? user.organization_id === null : false;
  const [organizations, setOrganizations] = useState<{ id: string; name: string }[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string>("");
  const activeOrg = platformAccount ? selectedOrg || null : (user?.organization_id ?? null);

  const load = useCallback(async (organizationId: string | null) => {
    setStatus("loading");
    setLoadError(null);
    try {
      const body = await fetchIamProviders(organizationId);
      setProviders(body.providers);
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The connected providers could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    if (!user) return;
    if (!platformAccount) {
      void load(null);
      return;
    }
    if (organizations === null) {
      void fetchOrganizations()
        .then((list) => {
          setOrganizations(list.map((o) => ({ id: o.id, name: o.name })));
          if (list.length > 0) setSelectedOrg((current) => current || list[0].id);
        })
        .catch(() => setOrganizations([]));
      return;
    }
    if (selectedOrg) void load(selectedOrg);
  }, [user, platformAccount, organizations, selectedOrg, load]);

  const selected = useMemo(
    () => providers.find((p) => p.id === showLog) ?? null,
    [providers, showLog],
  );

  /** Read a provider's sign-in log on demand. */
  const loadEvents = useCallback(
    async (providerId: string) => {
      try {
        const body = await fetchIamProviderEvents(providerId, { organizationId: activeOrg, limit: 25 });
        setEvents((current) => ({ ...current, [providerId]: body.events }));
      } catch {
        setEvents((current) => ({ ...current, [providerId]: [] }));
      }
    },
    [activeOrg],
  );

  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!draft) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const config: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(draft.config)) {
        if (value.trim()) config[key] = value.trim();
      }
      if (draft.id) {
        await updateIamProvider(draft.id, {
          name: draft.name,
          config,
          secretRef: draft.secretRef || null,
          groupClaim: draft.groupClaim || null,
          jitEnabled: draft.jitEnabled,
          enabled: draft.enabled,
        });
        setNotice(`${draft.name || draft.slug} was saved.`);
      } else {
        const created = await createIamProvider({
          slug: draft.slug,
          kind: draft.kind,
          name: draft.name,
          config,
          secretRef: draft.secretRef || null,
          groupClaim: draft.groupClaim || null,
          jitEnabled: draft.jitEnabled,
          organizationId: activeOrg,
        });
        setNotice(
          `${created.name} was connected. Test the connection, then switch it on when it passes.`,
        );
      }
      setDraft(null);
      await load(activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The provider could not be saved.");
    } finally {
      setBusy(false);
    }
  };

  const runTest = async (provider: IamAuthProvider) => {
    setBusy(true);
    setError(null);
    try {
      const result = await testIamProvider(provider.id);
      setTests((current) => ({ ...current, [provider.id]: result }));
      setNotice(
        result.status === "ok"
          ? `${provider.name}: ${result.detail}`
          : `${provider.name} is not working yet — ${result.detail}`,
      );
    } catch (cause) {
      setTests((current) => ({
        ...current,
        [provider.id]: {
          provider_id: provider.id,
          slug: provider.slug,
          kind: provider.kind,
          status: "failed",
          detail:
            cause instanceof ApiError ? cause.message : "the provider could not be reached",
          secret_present: false,
        },
      }));
    } finally {
      setBusy(false);
    }
  };

  const toggle = async (provider: IamAuthProvider) => {
    setBusy(true);
    setError(null);
    try {
      await updateIamProvider(provider.id, { enabled: !provider.enabled });
      setNotice(
        provider.enabled
          ? `${provider.name} is switched off; nobody can sign in with it.`
          : `${provider.name} is live — people can sign in with it now.`,
      );
      await load(activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The provider could not be changed.");
    } finally {
      setBusy(false);
    }
  };

  const remove = async (provider: IamAuthProvider) => {
    setBusy(true);
    setError(null);
    try {
      await deleteIamProvider(provider.id);
      setNotice(`${provider.name} was removed, with its sign-in log.`);
      setConfirmDelete(null);
      await load(activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The provider could not be removed.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="flex flex-col gap-4" data-iam-authentication>
      <header className="flex flex-col gap-1">
        <h1 className="text-[17px] font-semibold text-ink">Sign-in providers</h1>
        <p className="max-w-2xl text-[12.5px] text-muted">
          Connect a directory over OpenID Connect, OAuth 2.0 or SAML 2.0. Local sign-in stays
          available whatever happens here — a provider is an extra way in, never the only one.
        </p>
      </header>

      <div className="flex flex-wrap items-center gap-2">
        {platformAccount ? (
          <label className="flex items-center gap-2 text-[12.5px] text-muted">
            <span>Organization</span>
            <select
              value={selectedOrg}
              onChange={(event) => setSelectedOrg(event.target.value)}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink"
            >
              {(organizations ?? []).map((o) => (
                <option key={o.id} value={o.id}>
                  {o.name}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        <button
          type="button"
          data-iam-auth-reload
          onClick={() => void load(activeOrg)}
          className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] text-ink transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Reload
        </button>
        <button
          type="button"
          data-iam-auth-new
          onClick={() => {
            setError(null);
            setNotice(null);
            setDraft(emptyDraft());
          }}
          className="flex h-8 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          <Plus className="size-3.5" aria-hidden />
          Connect a provider
        </button>
      </div>

      {notice ? (
        <span data-iam-auth-notice className="text-[12.5px] text-muted">
          {notice}
        </span>
      ) : null}
      {error ? (
        <p
          role="alert"
          data-iam-auth-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {status === "loading" ? (
        <div className="flex flex-col gap-2" data-iam-auth-loading>
          {[0, 1].map((index) => (
            <div key={index} className="h-14 animate-pulse rounded-lg bg-panel" />
          ))}
        </div>
      ) : null}

      {status === "error" && loadError ? (
        <div
          role="alert"
          data-iam-auth-load-error
          className="flex flex-col gap-2 rounded-lg border border-danger/40 bg-danger-soft p-3 text-[12.5px] text-caution"
        >
          <span>{loadError.message}</span>
          <button
            type="button"
            onClick={() => void load(activeOrg)}
            className="w-fit rounded-lg border border-caution/40 px-3 py-1 text-[12px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "ready" && providers.length === 0 ? (
        <div
          data-providers-empty
          className="flex flex-col items-center gap-2 rounded-lg border border-dashed border-line p-6 text-center"
        >
          <ShieldCheck className="size-5 text-muted" aria-hidden />
          <p className="text-[13px] font-medium text-ink">No provider connected</p>
          <p className="max-w-md text-[12px] text-muted">
            People sign in with an e-mail and a password. Connect a directory to offer single sign-on
            as well — nothing else has to change.
          </p>
          <button
            type="button"
            onClick={() => setDraft(emptyDraft())}
            className="mt-1 flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] text-ink"
          >
            <Plus className="size-3.5" aria-hidden />
            Connect the first one
          </button>
        </div>
      ) : null}

      {providers.length > 0 ? (
        <ul className="flex flex-col gap-2" data-provider-list>
          {providers.map((provider) => {
            const test = tests[provider.id];
            return (
              <li
                key={provider.id}
                data-provider-row
                data-provider-slug={provider.slug}
                data-provider-enabled={provider.enabled ? "true" : "false"}
                className="flex flex-col gap-3 rounded-lg border border-line p-3"
              >
                <div className="flex flex-wrap items-start justify-between gap-3">
                  <div className="flex min-w-0 flex-col gap-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="text-[13.5px] font-medium text-ink">{provider.name}</span>
                      <span className="rounded-full border border-line bg-panel px-2 py-0.5 text-[11px] text-muted">
                        {KIND_LABELS[provider.kind] ?? provider.kind}
                      </span>
                      <code className="font-mono text-[11.5px] text-muted">/{provider.slug}</code>
                      {provider.enabled ? (
                        <span className="rounded-full border border-emerald-500/40 bg-emerald-500/10 px-2 py-0.5 text-[11px] text-emerald-700">
                          live
                        </span>
                      ) : (
                        <span className="rounded-full border border-line bg-panel px-2 py-0.5 text-[11px] text-muted">
                          off
                        </span>
                      )}
                      {provider.jit_enabled ? (
                        <span className="rounded-full border border-sky-500/40 bg-sky-500/10 px-2 py-0.5 text-[11px] text-sky-700">
                          provisions new accounts
                        </span>
                      ) : null}
                    </div>
                    <span className="text-[12px] text-muted">
                      {provider.sign_in_count} sign-in{provider.sign_in_count === 1 ? "" : "s"}
                      {provider.last_sign_in_at
                        ? ` · last ${new Date(provider.last_sign_in_at).toLocaleString()}`
                        : " · never used"}
                      {provider.group_claim ? ` · groups from "${provider.group_claim}"` : ""}
                    </span>
                  </div>

                  <div className="flex flex-wrap items-center gap-1.5">
                    <button
                      type="button"
                      data-provider-test={provider.slug}
                      disabled={busy}
                      onClick={() => void runTest(provider)}
                      className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink transition hover:bg-panel disabled:opacity-50"
                    >
                      {busy && test === undefined ? (
                        <Loader2 className="size-3.5 animate-spin" aria-hidden />
                      ) : (
                        <Link2 className="size-3.5" aria-hidden />
                      )}
                      Test connection
                    </button>
                    <button
                      type="button"
                      data-provider-edit={provider.slug}
                      onClick={() => {
                        setError(null);
                        setNotice(null);
                        setDraft(draftOf(provider));
                      }}
                      className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink transition hover:bg-panel"
                    >
                      <Pencil className="size-3.5" aria-hidden />
                      Edit
                    </button>
                    <button
                      type="button"
                      data-provider-toggle={provider.slug}
                      disabled={busy}
                      onClick={() => void toggle(provider)}
                      className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink transition hover:bg-panel disabled:opacity-50"
                    >
                      {provider.enabled ? (
                        <CircleSlash className="size-3.5" aria-hidden />
                      ) : (
                        <CheckCircle2 className="size-3.5" aria-hidden />
                      )}
                      {provider.enabled ? "Switch off" : "Switch on"}
                    </button>
                    <button
                      type="button"
                      data-provider-log={provider.slug}
                      onClick={() => {
                        const next = showLog === provider.id ? null : provider.id;
                        setShowLog(next);
                        if (next) void loadEvents(provider.id);
                      }}
                      className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink transition hover:bg-panel"
                    >
                      <KeyRound className="size-3.5" aria-hidden />
                      Sign-in log
                    </button>
                    {confirmDelete === provider.id ? (
                      <span className="flex items-center gap-1.5">
                        <button
                          type="button"
                          data-provider-delete-confirm={provider.slug}
                          disabled={busy}
                          onClick={() => void remove(provider)}
                          className="flex h-8 items-center gap-1.5 rounded-lg border border-danger/50 px-2.5 text-[12px] text-caution"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                          Remove for good
                        </button>
                        <button
                          type="button"
                          onClick={() => setConfirmDelete(null)}
                          className="h-8 rounded-lg border border-line px-2.5 text-[12px] text-ink"
                        >
                          Keep
                        </button>
                      </span>
                    ) : (
                      <button
                        type="button"
                        data-provider-delete={provider.slug}
                        onClick={() => setConfirmDelete(provider.id)}
                        className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12px] text-ink transition hover:bg-panel"
                      >
                        <Trash2 className="size-3.5" aria-hidden />
                        Remove
                      </button>
                    )}
                  </div>
                </div>

                {/* The secret is a name. This chip is the whole reason an operator can tell a
                    missing variable from a misconfigured provider. */}
                <div className="flex flex-wrap items-center gap-2 text-[11.5px]">
                  {provider.secret_ref ? (
                    <span
                      data-provider-secret={provider.slug}
                      data-secret-present={provider.secret_present ? "true" : "false"}
                      className={`flex items-center gap-1.5 rounded-full border px-2 py-0.5 ${
                        provider.secret_present
                          ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700"
                          : "border-amber-500/40 bg-amber-500/10 text-amber-800"
                      }`}
                    >
                      {provider.secret_present ? (
                        <CheckCircle2 className="size-3" aria-hidden />
                      ) : (
                        <AlertTriangle className="size-3" aria-hidden />
                      )}
                      secret in <code className="font-mono">{provider.secret_ref}</code>
                      {provider.secret_present ? " (defined)" : " (not defined here)"}
                    </span>
                  ) : (
                    <span className="flex items-center gap-1.5 rounded-full border border-line bg-panel px-2 py-0.5 text-muted">
                      <KeyRound className="size-3" aria-hidden />
                      public client — no secret
                    </span>
                  )}
                </div>

                {test ? (
                  <p
                    data-provider-test-result={provider.slug}
                    data-test-status={test.status}
                    className={`flex items-start gap-1.5 rounded-lg border px-2.5 py-1.5 text-[12px] ${
                      test.status === "ok"
                        ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700"
                        : "border-amber-500/40 bg-amber-500/10 text-amber-800"
                    }`}
                  >
                    {test.status === "ok" ? (
                      <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                    ) : (
                      <XCircle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                    )}
                    <span>{test.detail}</span>
                  </p>
                ) : null}

                {showLog === provider.id ? (
                  <div className="overflow-x-auto rounded-lg border border-line" data-provider-events>
                    {(events[provider.id] ?? []).length === 0 ? (
                      <p className="p-3 text-[12px] text-muted">
                        No sign-in has been attempted through this provider yet.
                      </p>
                    ) : (
                      <table className="w-full min-w-[640px] text-left text-[12px]">
                        <thead className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                          <tr>
                            <th className="px-3 py-1.5">When</th>
                            <th className="px-3 py-1.5">Outcome</th>
                            <th className="px-3 py-1.5">Subject</th>
                            <th className="px-3 py-1.5">Roles</th>
                            <th className="px-3 py-1.5">From</th>
                          </tr>
                        </thead>
                        <tbody>
                          {(events[provider.id] ?? []).map((event, index) => (
                            <tr
                              key={`${event.created_at}-${index}`}
                              data-event-outcome={event.outcome}
                              className="border-b border-line/60 last:border-0"
                            >
                              <td className="px-3 py-1.5 text-muted">
                                {new Date(event.created_at).toLocaleString()}
                              </td>
                              <td className="px-3 py-1.5">
                                <span
                                  className={`rounded-full border px-2 py-0.5 text-[11px] ${
                                    OUTCOME_STYLES[event.outcome] ?? "border-line bg-panel text-muted"
                                  }`}
                                >
                                  {event.outcome}
                                </span>
                                {event.reason ? (
                                  <span className="ml-1 text-[11px] text-muted">{event.reason}</span>
                                ) : null}
                              </td>
                              <td className="px-3 py-1.5 font-mono text-[11px] text-muted">
                                {event.external_subject ?? "—"}
                              </td>
                              <td className="px-3 py-1.5 text-muted">
                                {event.roles_applied || "—"}
                              </td>
                              <td className="px-3 py-1.5 text-muted">{event.ip_address ?? "—"}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    )}
                  </div>
                ) : null}
              </li>
            );
          })}
        </ul>
      ) : null}

      {selected ? null : null}

      {draft ? (
        <div
          data-provider-drawer
          role="dialog"
          aria-label={draft.id ? "Edit the provider" : "Connect a provider"}
          className="fixed inset-0 z-50 flex justify-end bg-ink/20"
        >
          <button
            type="button"
            aria-label="Close"
            onClick={() => setDraft(null)}
            className="flex-1 cursor-default"
          />
          <form
            onSubmit={(event) => void save(event)}
            className="flex h-full w-full max-w-lg flex-col gap-4 overflow-y-auto border-l border-line bg-surface p-4"
          >
            <header className="flex items-center justify-between">
              <h2 className="text-[15px] font-semibold text-ink">
                {draft.id ? "Edit the provider" : "Connect a provider"}
              </h2>
              <button
                type="button"
                data-provider-drawer-close
                onClick={() => setDraft(null)}
                className="rounded-lg border border-line px-2 py-1 text-[12px] text-ink"
              >
                Close
              </button>
            </header>

            {!draft.id ? (
              <div className="flex flex-col gap-1">
                <span className="text-[11.5px] text-muted">Protocol</span>
                <div className="flex gap-1.5" data-provider-kind>
                  {Object.entries(KIND_LABELS).map(([value, label]) => (
                    <button
                      key={value}
                      type="button"
                      data-kind={value}
                      onClick={() =>
                        setDraft((current) =>
                          current
                            ? {
                                ...current,
                                kind: value as Draft["kind"],
                                config: {},
                                secretRef: value === "saml" ? "" : current.secretRef,
                              }
                            : current,
                        )
                      }
                      className={`h-8 rounded-lg border px-3 text-[12px] transition ${
                        draft.kind === value
                          ? "border-accent bg-accent/10 text-ink"
                          : "border-line text-muted hover:bg-panel"
                      }`}
                    >
                      {label}
                    </button>
                  ))}
                </div>
              </div>
            ) : (
              <p className="text-[12px] text-muted">
                {KIND_LABELS[draft.kind]} · <code className="font-mono">/{draft.slug}</code>
              </p>
            )}

            <label className="flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">Name on the sign-in screen</span>
              <input
                value={draft.name}
                data-provider-name
                onChange={(e) => setDraft({ ...draft, name: e.target.value })}
                placeholder="e.g. Company directory"
                className="h-9 rounded-lg border border-line bg-panel px-2.5 text-[13px] text-ink outline-none focus:border-accent"
              />
            </label>

            {!draft.id ? (
              <label className="flex flex-col gap-1">
                <span className="text-[11.5px] text-muted">
                  URL name (letters, digits and dashes, used in the sign-in link)
                </span>
                <input
                  value={draft.slug}
                  data-provider-slug-input
                  onChange={(e) => setDraft({ ...draft, slug: e.target.value })}
                  placeholder="e.g. okta"
                  className="h-9 rounded-lg border border-line bg-panel px-2.5 font-mono text-[13px] text-ink outline-none focus:border-accent"
                />
              </label>
            ) : null}

            {draft.kind !== "saml" ? (
              <label className="flex flex-col gap-1">
                <span className="text-[11.5px] text-muted">
                  Client secret — the <em>name</em> of the environment variable, never the value
                </span>
                <input
                  value={draft.secretRef}
                  data-provider-secret-ref
                  onChange={(e) => setDraft({ ...draft, secretRef: e.target.value })}
                  placeholder="e.g. OMNION_SSO_OKTA_SECRET"
                  className="h-9 rounded-lg border border-line bg-panel px-2.5 font-mono text-[13px] text-ink outline-none focus:border-accent"
                />
                <span className="text-[11px] text-muted">
                  Set it in the environment of the API process. This screen can only check whether
                  the name is defined — it can never show the value.
                </span>
              </label>
            ) : null}

            <div className="flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">
                {draft.kind === "saml" ? "Assertion settings" : "Endpoints and claims"}
              </span>
              <div className="grid gap-2 sm:grid-cols-2">
                {(draft.kind === "saml" ? SAML_FIELDS : OIDC_FIELDS).map((field) => (
                  <label
                    key={field.key}
                    className={`flex flex-col gap-1 ${field.key === "certificate_pem" || field.key === "redirect_uri" ? "sm:col-span-2" : ""}`}
                  >
                    <span className="text-[11px] text-muted">
                      {field.label}
                      {"optional" in field && field.optional ? " (optional)" : ""}
                    </span>
                    <input
                      value={draft.config[field.key] ?? ""}
                      data-provider-field={field.key}
                      onChange={(e) =>
                        setDraft({ ...draft, config: { ...draft.config, [field.key]: e.target.value } })
                      }
                      placeholder={("hint" in field && field.hint) || ""}
                      className="h-9 rounded-lg border border-line bg-panel px-2.5 font-mono text-[12.5px] text-ink outline-none focus:border-accent"
                    />
                  </label>
                ))}
              </div>
              {draft.kind !== "saml" ? (
                <span className="text-[11px] text-muted">
                  With only an issuer, everything else is discovered from it. The endpoints are only
                  needed for a provider that publishes no discovery document.
                </span>
              ) : null}
            </div>

            <label className="flex flex-col gap-1">
              <span className="text-[11.5px] text-muted">Claim carrying group membership</span>
              <input
                value={draft.groupClaim}
                data-provider-group-claim
                onChange={(e) => setDraft({ ...draft, groupClaim: e.target.value })}
                placeholder="e.g. groups"
                className="h-9 rounded-lg border border-line bg-panel px-2.5 font-mono text-[13px] text-ink outline-none focus:border-accent"
              />
            </label>

            <label className="flex items-start gap-2 rounded-lg border border-line p-2.5">
              <input
                type="checkbox"
                data-provider-jit
                checked={draft.jitEnabled}
                onChange={(e) => setDraft({ ...draft, jitEnabled: e.target.checked })}
                className="mt-0.5"
              />
              <span className="flex flex-col gap-0.5 text-[12px] text-ink">
                <span>Create an account on the first sign-in</span>
                <span className="text-muted">
                  A person the provider vouches for gets an account without anybody creating it by
                  hand. Off by default: an account nobody approved is worse than a failed sign-in.
                </span>
              </span>
            </label>

            {draft.id ? (
              <label className="flex items-start gap-2 rounded-lg border border-line p-2.5">
                <input
                  type="checkbox"
                  data-provider-enabled
                  checked={draft.enabled}
                  onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })}
                  className="mt-0.5"
                />
                <span className="flex flex-col gap-0.5 text-[12px] text-ink">
                  <span>Reachable — people can sign in with this provider</span>
                  <span className="text-muted">
                    Switching it off leaves the configuration alone; the sign-in link simply stops
                    working.
                  </span>
                </span>
              </label>
            ) : null}

            <div className="mt-auto flex items-center justify-end gap-2 border-t border-line pt-3">
              <button
                type="button"
                onClick={() => setDraft(null)}
                className="h-9 rounded-lg border border-line px-3 text-[12.5px] text-ink"
              >
                Cancel
              </button>
              <button
                type="submit"
                data-provider-save
                disabled={busy}
                className="flex h-9 items-center gap-1.5 rounded-lg bg-accent px-4 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                {draft.id ? "Save" : "Connect"}
              </button>
            </div>
          </form>
        </div>
      ) : null}
    </section>
  );
}
