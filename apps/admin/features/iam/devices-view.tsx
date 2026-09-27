"use client";

/**
 * `/settings/iam/devices` — the devices the platform has seen (REQ-006, slice 3).
 *
 * A device is registered by the sign-in it came from, so first and last seen are observations
 * rather than declarations. Trust is a window the reader sets; forgetting a device revokes it
 * **and ends its live sessions**, because removing a device you no longer hold should mean it
 * has to sign in again.
 */
import { useCallback, useEffect, useState } from "react";

import { Fingerprint, RefreshCw, Search, ShieldCheck, Trash2 } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  fetchIamDevices,
  fetchOrganizations,
  forgetIamDevice,
  trustIamDevice,
  type IamDevice,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** `/settings/iam/devices`. */
export function DevicesView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [devices, setDevices] = useState<IamDevice[]>([]);
  const [trustDays, setTrustDays] = useState(30);
  const [search, setSearch] = useState("");
  const [includeRevoked, setIncludeRevoked] = useState(false);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmForget, setConfirmForget] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;
  // A platform account starts on every organization (see the sessions screen for the reason).
  const activeOrg = platformAccount ? (selectedOrg ?? "") : (user?.organization_id ?? null);

  const load = useCallback(
    async (organizationId: string | null, query: { search: string; includeRevoked: boolean }) => {
      setStatus("loading");
      setLoadError(null);
      try {
        const body = await fetchIamDevices({
          organizationId,
          search: query.search || undefined,
          includeRevoked: query.includeRevoked,
        });
        setDevices(body.devices);
        setTrustDays(body.device_trust_days);
        setStatus("ready");
      } catch (cause) {
        setStatus("error");
        setLoadError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The devices could not be read." },
        );
      }
    },
    [],
  );

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void load(null, { search: "", includeRevoked: false });
      return;
    }
    // A platform account: every organization at once (the picker narrows it afterwards).
    void load(null, { search: "", includeRevoked: false });
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg("");
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, load]);

  useEffect(() => {
    if (!platformAccount || !selectedOrg) return;
    void load(selectedOrg, { search, includeRevoked });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [platformAccount, selectedOrg]);

  const trust = async (device: IamDevice, days: number) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await trustIamDevice(device.id, { days });
      setNotice(
        days > 0
          ? `${device.label} is trusted for ${days} days.`
          : `${device.label} is no longer trusted.`,
      );
      await load(activeOrg, { search, includeRevoked });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The trust window could not be saved.");
    } finally {
      setBusy(false);
    }
  };

  const forget = async (device: IamDevice) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const forgotten = await forgetIamDevice(device.id);
      setNotice(
        `${forgotten.label} was forgotten${forgotten.session_count === 0 ? "" : " and its sessions ended"}.`,
      );
      setConfirmForget(null);
      await load(activeOrg, { search, includeRevoked });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The device could not be forgotten.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <form
        className="flex flex-wrap items-end gap-2 rounded-xl border border-line bg-surface p-3"
        onSubmit={(event) => {
          event.preventDefault();
          void load(activeOrg, { search, includeRevoked });
        }}
      >
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Search</span>
          <span className="flex h-8 items-center gap-1.5 rounded-lg border border-line bg-surface px-2">
            <Search className="size-3.5 text-muted" aria-hidden />
            <input
              value={search}
              data-devices-search
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Chrome, Linux, qa-user@…"
              className="w-52 bg-transparent text-[12.5px] text-ink outline-none"
            />
          </span>
        </label>
        <label className="flex items-center gap-2 pb-1.5 text-[12.5px] text-muted">
          <input
            type="checkbox"
            checked={includeRevoked}
            data-devices-revoked
            onChange={(event) => setIncludeRevoked(event.target.checked)}
            className="size-4 rounded border-line"
          />
          Include forgotten devices
        </label>
        <button
          type="submit"
          className="h-8 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
        >
          Apply
        </button>
        <button
          type="button"
          onClick={() => void load(activeOrg, { search, includeRevoked })}
          className="flex h-8 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] text-ink transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Reload
        </button>
        {platformAccount ? (
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-devices-org
              onChange={(event) => setSelectedOrg(event.target.value)}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">All organizations</option>
              {(organizations ?? []).map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        <span className="ml-auto pb-1.5 text-[11.5px] text-muted">
          Policy trust window {trustDays} days — the default a new device gets.
        </span>
      </form>

      {notice ? (
        <span data-devices-notice className="text-[12.5px] text-muted">
          {notice}
        </span>
      ) : null}

      {error ? (
        <p
          role="alert"
          data-devices-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {error}
        </p>
      ) : null}

      {status === "loading" ? (
        <div className="flex flex-col gap-2" data-devices-loading>
          {[0, 1, 2].map((index) => (
            <div key={index} className="h-11 animate-pulse rounded-lg bg-panel" />
          ))}
        </div>
      ) : null}

      {status === "error" && loadError ? (
        <div
          role="alert"
          data-devices-load-error
          className="flex flex-col gap-2 rounded-xl border border-danger/40 bg-danger-soft p-4 text-[12.5px] text-caution"
        >
          <span>{loadError.message}</span>
          <button
            type="button"
            onClick={() => void load(activeOrg, { search, includeRevoked })}
            className="w-fit rounded-lg border border-caution/40 px-3 py-1 text-[12px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "ready" && devices.length === 0 ? (
        <div
          data-devices-empty
          className="flex flex-col items-center gap-2 rounded-xl border border-dashed border-line bg-surface p-8 text-center"
        >
          <Fingerprint className="size-5 text-muted" aria-hidden />
          <p className="text-[13px] font-medium text-ink">No devices yet</p>
          <p className="max-w-md text-[12px] text-muted">
            A device appears the first time an account signs in from it. Sign in from another
            browser and it shows up here with its own trust window.
          </p>
        </div>
      ) : null}

      {status === "ready" && devices.length > 0 ? (
        <>
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface lg:block">
            <table className="w-full min-w-[900px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <tr>
                  <th className="px-3 py-2">Account</th>
                  <th className="px-3 py-2">Device</th>
                  <th className="px-3 py-2">Fingerprint</th>
                  <th className="px-3 py-2">First seen</th>
                  <th className="px-3 py-2">Last seen</th>
                  <th className="px-3 py-2">Sessions</th>
                  <th className="px-3 py-2">Trusted until</th>
                  <th className="px-3 py-2" />
                </tr>
              </thead>
              <tbody>
                {devices.map((device) => (
                  <tr key={device.id} data-device-row className="border-b border-line/60 last:border-0">
                    <td className="px-3 py-2">
                      <span className="block text-ink">{device.user_email}</span>
                      <span className="text-[11.5px] text-muted">{device.user_display_name}</span>
                    </td>
                    <td className="px-3 py-2">
                      <span className="block text-ink">{device.label}</span>
                      <span className="text-[11.5px] text-muted">
                        {device.browser} · {device.platform}
                        {device.revoked ? " · forgotten" : ""}
                      </span>
                    </td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">
                      {device.fingerprint_hint}…
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {new Date(device.first_seen_at).toLocaleDateString()}
                    </td>
                    <td className="px-3 py-2 text-muted">
                      {new Date(device.last_seen_at).toLocaleString()}
                    </td>
                    <td className="px-3 py-2 text-muted">{device.session_count}</td>
                    <td className="px-3 py-2">
                      {device.trusted && device.trusted_until ? (
                        <span className="rounded-full border border-emerald-500/40 bg-emerald-500/10 px-2 py-0.5 text-[11px] text-emerald-700">
                          trusted until {new Date(device.trusted_until).toLocaleDateString()}
                        </span>
                      ) : (
                        <span className="rounded-full border border-line bg-panel px-2 py-0.5 text-[11px] text-muted">
                          not trusted
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex items-center justify-end gap-1.5">
                        <button
                          type="button"
                          disabled={busy}
                          data-device-trust
                          aria-label={`Trust ${device.label} for ${trustDays} days`}
                          onClick={() => void trust(device, trustDays)}
                          className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink transition hover:bg-panel"
                        >
                          <ShieldCheck className="size-3.5" aria-hidden />
                          Trust {trustDays}d
                        </button>
                        <button
                          type="button"
                          disabled={busy}
                          data-device-clear-trust
                          onClick={() => void trust(device, 0)}
                          className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted transition hover:bg-panel"
                        >
                          Clear
                        </button>
                        {confirmForget === device.id ? (
                          <span className="flex items-center gap-1.5">
                            <button
                              type="button"
                              disabled={busy}
                              data-device-forget-confirm
                              data-qa-guard="device-forget"
                              onClick={() => void forget(device)}
                              className="rounded-lg border border-danger/40 bg-danger-soft px-2 py-1 text-[11.5px] text-caution"
                            >
                              Forget it
                            </button>
                            <button
                              type="button"
                              onClick={() => setConfirmForget(null)}
                              className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
                            >
                              Cancel
                            </button>
                          </span>
                        ) : (
                          <button
                            type="button"
                            disabled={busy || device.revoked}
                            data-device-forget
                            data-qa-guard="device-forget"
                            aria-label={`Forget ${device.label}`}
                            onClick={() => setConfirmForget(device.id)}
                            className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution transition hover:bg-panel disabled:opacity-50"
                          >
                            <Trash2 className="size-3.5" aria-hidden />
                            Forget
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="flex flex-col gap-2 lg:hidden" data-device-cards>
            {devices.map((device) => (
              <li
                key={device.id}
                data-device-row
                className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface p-3 text-[12.5px]"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="font-medium text-ink">{device.label}</span>
                  <span className="text-[11.5px] text-muted">
                    {device.trusted ? "trusted" : "not trusted"}
                  </span>
                </div>
                <span className="text-[11.5px] text-muted">
                  {device.user_email} · {device.browser} · {device.platform}
                </span>
                <span className="text-[11.5px] text-muted">
                  Last seen {new Date(device.last_seen_at).toLocaleString()} · {device.session_count}{" "}
                  session{device.session_count === 1 ? "" : "s"}
                </span>
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    disabled={busy}
                    data-device-trust
                    onClick={() => void trust(device, trustDays)}
                    className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-ink"
                  >
                    Trust {trustDays}d
                  </button>
                  <button
                    type="button"
                    disabled={busy}
                    data-device-clear-trust
                    onClick={() => void trust(device, 0)}
                    className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-muted"
                  >
                    Clear
                  </button>
                  <button
                    type="button"
                    disabled={busy || device.revoked}
                    data-device-forget
                    data-qa-guard="device-forget"
                    onClick={() => void forget(device)}
                    className="rounded-lg border border-line px-2 py-1 text-[11.5px] text-caution disabled:opacity-50"
                  >
                    Forget
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </>
      ) : null}
    </div>
  );
}
