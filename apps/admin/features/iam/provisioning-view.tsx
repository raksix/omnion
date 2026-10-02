"use client";

/**
 * `/settings/iam/provisioning` — SCIM tokens and the sync log (REQ-006, slice 4b).
 *
 * A directory (Okta, Entra ID, Keycloak …) talks to `/api/v1/scim/v2` with one of the tokens
 * minted here. The secret is shown **once**: the store keeps only its hash, so a lost secret is
 * replaced by rotating the token, never recovered. Every call the token makes is listed below —
 * ids, actions and outcomes, never payloads.
 */
import { useCallback, useEffect, useState } from "react";

import { KeyRound, RefreshCw, ShieldCheck, Trash2, Upload } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  createIamProvisioningToken,
  fetchIamProvisioningLog,
  fetchIamProvisioningTokens,
  fetchOrganizations,
  revokeIamProvisioningToken,
  type IamProvisioningToken,
  type IamSyncLogEntry,
} from "@/lib/api";

/** Outcome colours of the sync log. */
const OUTCOME_STYLES: Record<string, string> = {
  created: "border-emerald-500/40 bg-emerald-500/10 text-emerald-700",
  updated: "border-sky-500/40 bg-sky-500/10 text-sky-700",
  deactivated: "border-line bg-panel text-muted",
  skipped: "border-line bg-panel text-muted",
  failed: "border-danger/40 bg-danger-soft text-caution",
};

/** `/settings/iam/provisioning`. */
export function ProvisioningView() {
  const { user } = useSession();
  const [tokens, setTokens] = useState<IamProvisioningToken[]>([]);
  const [log, setLog] = useState<IamSyncLogEntry[]>([]);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [name, setName] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const [confirmRevoke, setConfirmRevoke] = useState<string | null>(null);

  // A platform account (the first-run owner) names the organization it provisions into; an
  // account with its own organization never sees the picker.
  const platformAccount = user ? user.organization_id === null : false;
  const [organizations, setOrganizations] = useState<{ id: string; name: string }[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string>("");
  const activeOrg = platformAccount ? (selectedOrg || null) : (user?.organization_id ?? null);

  const load = useCallback(async (organizationId: string | null) => {
    setStatus("loading");
    setLoadError(null);
    try {
      const [tokenBody, logBody] = await Promise.all([
        fetchIamProvisioningTokens(organizationId),
        fetchIamProvisioningLog({ organizationId, limit: 100 }),
      ]);
      setTokens(tokenBody.tokens);
      setLog(logBody.log);
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The provisioning state could not be read." },
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
          setOrganizations(list.map((organization) => ({ id: organization.id, name: organization.name })));
          if (list.length > 0) setSelectedOrg((current) => current || list[0].id);
        })
        .catch(() => setOrganizations([]));
      return;
    }
    if (!selectedOrg) return;
    void load(selectedOrg);
  }, [user, load, platformAccount, organizations, selectedOrg]);

  const mint = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    setSecret(null);
    try {
      const issued = await createIamProvisioningToken({ name: name.trim(), organizationId: activeOrg });
      setSecret(issued.secret);
      setNotice(`Token ${issued.token.prefix} minted. Copy the secret now — it is shown once.`);
      setName("");
      await load(activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The token could not be minted.");
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (token: IamProvisioningToken) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await revokeIamProvisioningToken(token.id);
      setNotice(
        result.revoked
          ? `Token ${token.prefix} was revoked — a client presenting it is refused from now on.`
          : `Token ${token.prefix} was already revoked.`,
      );
      setConfirmRevoke(null);
      await load(activeOrg);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The token could not be revoked.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-provisioning-view>
      <section className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="flex items-center gap-1.5 text-[12.5px] font-medium text-ink">
            <KeyRound className="size-3.5 text-muted" aria-hidden />
            Provisioning tokens
          </h2>
          <span className="text-[11.5px] text-muted">
            A directory presents one as <span className="font-mono">Authorization: Bearer …</span>{" "}
            against <span className="font-mono">/api/v1/scim/v2</span>.
          </span>
          {platformAccount && organizations && organizations.length > 0 ? (
            <label className="ml-auto flex flex-col">
              <span className="sr-only">Organization</span>
              <select
                value={selectedOrg}
                data-provisioning-organization
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
          ) : null}
          <button
            type="button"
            data-provisioning-reload
            onClick={() => void load(activeOrg)}
            className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] text-ink transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Reload
          </button>
        </div>

        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            void mint();
          }}
        >
          <label className="flex min-w-56 flex-1 flex-col gap-1">
            <span className="text-[11.5px] text-muted">Token name</span>
            <input
              value={name}
              data-token-name
              onChange={(event) => setName(event.target.value)}
              placeholder="e.g. Okta production"
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>
          <button
            type="submit"
            data-token-mint
            disabled={busy}
            className="flex h-8 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
          >
            <Upload className="size-3.5" aria-hidden />
            Mint token
          </button>
        </form>

        {secret ? (
          <div
            data-token-secret
            className="flex flex-col gap-1 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3"
          >
            <span className="text-[11.5px] font-medium text-amber-800">
              Copy this secret now — it is stored only as a hash and cannot be shown again.
            </span>
            <code className="break-all font-mono text-[12px] text-ink">{secret}</code>
          </div>
        ) : null}

        {notice ? (
          <span data-provisioning-notice className="text-[12.5px] text-muted">
            {notice}
          </span>
        ) : null}

        {error ? (
          <p
            role="alert"
            data-provisioning-error
            className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
          >
            {error}
          </p>
        ) : null}

        {status === "loading" ? (
          <div className="flex flex-col gap-2" data-provisioning-loading>
            {[0, 1].map((index) => (
              <div key={index} className="h-10 animate-pulse rounded-lg bg-panel" />
            ))}
          </div>
        ) : null}

        {status === "error" && loadError ? (
          <div
            role="alert"
            data-provisioning-load-error
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

        {status === "ready" && tokens.length === 0 ? (
          <div
            data-tokens-empty
            className="flex flex-col items-center gap-2 rounded-lg border border-dashed border-line p-6 text-center"
          >
            <ShieldCheck className="size-5 text-muted" aria-hidden />
            <p className="text-[13px] font-medium text-ink">No token yet</p>
            <p className="max-w-md text-[12px] text-muted">
              Mint one, paste it into the directory's SCIM configuration, and every call it makes
              shows up in the sync log below.
            </p>
          </div>
        ) : null}

        {tokens.length > 0 ? (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[720px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2">Name</th>
                  <th className="px-3 py-2">Prefix</th>
                  <th className="px-3 py-2">Created</th>
                  <th className="px-3 py-2">Last used</th>
                  <th className="px-3 py-2">State</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {tokens.map((token) => (
                  <tr key={token.id} data-token-row data-token-revoked={token.revoked_at ? "true" : "false"} className="border-b border-line/60 last:border-0">
                    <td className="px-3 py-2 text-ink">{token.name || "—"}</td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">{token.prefix}…</td>
                    <td className="px-3 py-2 text-muted">
                      {new Date(token.created_at).toLocaleString()}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {token.last_used_at ? new Date(token.last_used_at).toLocaleString() : "never"}
                    </td>
                    <td className="px-3 py-2">
                      {token.revoked_at ? (
                        <span className="rounded-full border border-line bg-panel px-2 py-0.5 text-[11px] text-muted">
                          revoked
                        </span>
                      ) : (
                        <span className="rounded-full border border-emerald-500/40 bg-emerald-500/10 px-2 py-0.5 text-[11px] text-emerald-700">
                          live
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      {token.revoked_at ? null : confirmRevoke === token.id ? (
                        <span className="flex items-center justify-end gap-1.5">
                          <button
                            type="button"
                            disabled={busy}
                            data-token-revoke-confirm
                            onClick={() => void revoke(token)}
                            className="rounded-lg border border-danger/40 bg-danger-soft px-2 py-1 text-[11.5px] text-caution"
                          >
                            Revoke it
                          </button>
                          <button
                            type="button"
                            onClick={() => setConfirmRevoke(null)}
                            className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
                          >
                            Cancel
                          </button>
                        </span>
                      ) : (
                        <button
                          type="button"
                          disabled={busy}
                          data-token-revoke
                          onClick={() => setConfirmRevoke(token.id)}
                          className="ml-auto flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-panel"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                          Revoke
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : null}
      </section>

      <section className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-[12.5px] font-medium text-ink">Sync log</h2>
          <span className="text-[11.5px] text-muted">
            What the directory did, newest first — ids and outcomes, never payloads. Deactivation
            is the SCIM default: <span className="font-mono">DELETE /Users/{"{id}"}</span> disables
            an account instead of deleting it.
          </span>
        </div>

        {status === "ready" && log.length === 0 ? (
          <p data-sync-log-empty className="rounded-lg border border-dashed border-line p-6 text-center text-[12.5px] text-muted">
            No sync activity yet. Create a user through the SCIM endpoint and its outcome lands
            here.
          </p>
        ) : null}

        {log.length > 0 ? (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[820px] text-left text-[12px]">
              <thead className="border-b border-line text-[11px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2">When</th>
                  <th className="px-3 py-2">Resource</th>
                  <th className="px-3 py-2">Action</th>
                  <th className="px-3 py-2">Outcome</th>
                  <th className="px-3 py-2">Detail</th>
                </tr>
              </thead>
              <tbody>
                {log.map((entry) => (
                  <tr key={entry.id} data-sync-log-row data-sync-outcome={entry.outcome} className="border-b border-line/60 last:border-0">
                    <td className="px-3 py-1.5 text-muted">
                      {new Date(entry.created_at).toLocaleString()}
                    </td>
                    <td className="px-3 py-1.5 text-muted">{entry.resource}</td>
                    <td className="px-3 py-1.5 text-ink">{entry.action}</td>
                    <td className="px-3 py-1.5">
                      <span
                        className={`rounded-full border px-2 py-0.5 text-[11px] ${
                          OUTCOME_STYLES[entry.outcome] ?? "border-line bg-panel text-muted"
                        }`}
                      >
                        {entry.outcome}
                      </span>
                    </td>
                    <td className="px-3 py-1.5 text-muted">{entry.detail}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : null}
      </section>
    </div>
  );
}
