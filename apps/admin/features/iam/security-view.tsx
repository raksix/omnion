"use client";

/**
 * `/settings/iam/security` — the organization's security policy (REQ-006, slice 3).
 *
 * Five tabs, each holding one family of thresholds: the password rules, the lockout, the address
 * lists, the session lifetimes and the device trust window. Every field carries the range the
 * server enforces, so a mistyped value is refused in the field it belongs to instead of becoming
 * a failed round trip; a save answers with the diff it applied and that diff is what the reader
 * sees (and what the audit entry records).
 */
import { useCallback, useEffect, useState } from "react";

import {
  Fingerprint,
  KeyRound,
  LockKeyhole,
  Network,
  RefreshCw,
  Save,
  Timer,
} from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  fetchOrganizations,
  fetchSecurityPolicy,
  updateSecurityPolicy,
  type IamPolicyChange,
  type IamSecurityPolicy,
} from "@/lib/api";
import type { Organization } from "@/lib/types";

/** Fields the five tabs edit. */
type NumericKey =
  | "password_min_length"
  | "password_require_classes"
  | "password_history"
  | "password_expiry_days"
  | "lockout_attempts"
  | "lockout_minutes"
  | "session_idle_minutes"
  | "session_absolute_days"
  | "session_concurrent_max"
  | "device_trust_days";

/** One numeric control. */
type FieldSpec = {
  key: NumericKey;
  label: string;
  hint: string;
  min: number;
  max: number;
  step?: number;
};

/** One tab of the policy screen. */
type TabSpec = {
  id: string;
  label: string;
  icon: typeof KeyRound;
  blurb: string;
  fields: FieldSpec[];
};

/** The five tabs, in the order a reader meets them. */
const TABS: TabSpec[] = [
  {
    id: "password",
    label: "Password",
    icon: KeyRound,
    blurb: "What a password has to look like, and how long it is remembered.",
    fields: [
      {
        key: "password_min_length",
        label: "Minimum length",
        hint: "8–128 characters",
        min: 8,
        max: 128,
      },
      {
        key: "password_require_classes",
        label: "Character classes",
        hint: "1–4 of lower, upper, digit, symbol",
        min: 1,
        max: 4,
      },
      {
        key: "password_history",
        label: "Remembered passwords",
        hint: "0–24 previous passwords cannot be reused",
        min: 0,
        max: 24,
      },
      {
        key: "password_expiry_days",
        label: "Expiry (days)",
        hint: "0–730, where 0 never expires",
        min: 0,
        max: 730,
      },
    ],
  },
  {
    id: "lockout",
    label: "Lockout",
    icon: LockKeyhole,
    blurb: "How many failures are tolerated before an account or an address is refused.",
    fields: [
      {
        key: "lockout_attempts",
        label: "Failed attempts",
        hint: "3–50 failures before the lockout",
        min: 3,
        max: 50,
      },
      {
        key: "lockout_minutes",
        label: "Lockout window (minutes)",
        hint: "1–1440 minutes an account stays locked",
        min: 1,
        max: 1440,
      },
    ],
  },
  {
    id: "addresses",
    label: "Addresses",
    icon: Network,
    blurb: "Deny wins; an empty allowlist means any address may sign in.",
    fields: [],
  },
  {
    id: "sessions",
    label: "Sessions",
    icon: Timer,
    blurb: "How long a session lives, and how many one account may hold.",
    fields: [
      {
        key: "session_idle_minutes",
        label: "Idle lifetime (minutes)",
        hint: "5–10080 minutes without activity ends the session",
        min: 5,
        max: 10080,
      },
      {
        key: "session_absolute_days",
        label: "Absolute lifetime (days)",
        hint: "1–365 days, however active the session is",
        min: 1,
        max: 365,
      },
      {
        key: "session_concurrent_max",
        label: "Concurrent sessions",
        hint: "1–100; opening one more retires the oldest",
        min: 1,
        max: 100,
      },
    ],
  },
  {
    id: "devices",
    label: "Devices",
    icon: Fingerprint,
    blurb: "How long a device stays trusted after it first signs in.",
    fields: [
      {
        key: "device_trust_days",
        label: "Trust window (days)",
        hint: "0–365, where 0 trusts no device by default",
        min: 0,
        max: 365,
      },
    ],
  },
];

/** Validate one address-list line the way the API does: an address, or a network with a prefix. */
function invalidAddressLine(line: string): string | null {
  const trimmed = line.trim();
  const [address, prefix] = trimmed.split("/");
  const octets = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(address);
  const isIpv4 = octets !== null && octets.slice(1).every((octet) => Number(octet) <= 255);
  const isIpv6 = address.includes(":") && /^[0-9a-fA-F:]+$/.test(address) && address.split(":").length <= 8;
  if (!isIpv4 && !isIpv6) {
    return `“${trimmed}” is not an address or network.`;
  }
  if (prefix !== undefined) {
    const value = Number(prefix);
    const max = isIpv4 ? 32 : 128;
    if (!Number.isInteger(value) || value < 0 || value > max) {
      return `“${trimmed}” has a prefix that must be 0–${max}.`;
    }
  }
  return null;
}

/** `/settings/iam/security`. */
export function SecurityView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [policy, setPolicy] = useState<IamSecurityPolicy | null>(null);
  const [draft, setDraft] = useState<Record<string, string>>({});
  const [allowlist, setAllowlist] = useState("");
  const [denylist, setDenylist] = useState("");
  const [mfaRequired, setMfaRequired] = useState(false);
  const [tab, setTab] = useState("password");
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [changes, setChanges] = useState<IamPolicyChange[] | null>(null);
  const [busy, setBusy] = useState(false);

  const platformAccount = user ? user.organization_id === null : false;
  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  const apply = useCallback((loaded: IamSecurityPolicy) => {
    setPolicy(loaded);
    setDraft({
      password_min_length: String(loaded.password_min_length),
      password_require_classes: String(loaded.password_require_classes),
      password_history: String(loaded.password_history),
      password_expiry_days: String(loaded.password_expiry_days),
      lockout_attempts: String(loaded.lockout_attempts),
      lockout_minutes: String(loaded.lockout_minutes),
      session_idle_minutes: String(loaded.session_idle_minutes),
      session_absolute_days: String(loaded.session_absolute_days),
      session_concurrent_max: String(loaded.session_concurrent_max),
      device_trust_days: String(loaded.device_trust_days),
    });
    setAllowlist(loaded.ip_allowlist.join("\n"));
    setDenylist(loaded.ip_denylist.join("\n"));
    setMfaRequired(loaded.mfa_required);
    setChanges(null);
    setError(null);
    setFieldError(null);
  }, []);

  const load = useCallback(
    async (organizationId: string | null) => {
      setStatus("loading");
      setLoadError(null);
      try {
        apply(await fetchSecurityPolicy(organizationId));
        setStatus("ready");
      } catch (cause) {
        setStatus("error");
        setLoadError(
          cause instanceof ApiError
            ? { code: cause.code, message: cause.message }
            : { code: "unknown_error", message: "The security policy could not be read." },
        );
      }
    },
    [apply],
  );

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
    if (!platformAccount || !selectedOrg) return;
    void load(selectedOrg);
  }, [platformAccount, selectedOrg, load]);

  /** Every numeric field, validated against the range the server enforces. */
  const numericPatch = ():
    | { ok: true; patch: Record<string, number> }
    | { ok: false; spec: FieldSpec } => {
    const patch: Record<string, number> = {};
    for (const spec of TABS.flatMap((entry) => entry.fields)) {
      const raw = (draft[spec.key] ?? "").trim();
      const value = Number(raw);
      if (raw === "" || Number.isNaN(value) || !Number.isInteger(value)) {
        return { ok: false, spec };
      }
      if (value < spec.min || value > spec.max) {
        return { ok: false, spec };
      }
      patch[spec.key] = value;
    }
    return { ok: true, patch };
  };

  /** Split a textarea into entries, refusing a line that is not an address or network. */
  const listFrom = (text: string): string[] =>
    text
      .split("\n")
      .map((entry) => entry.trim())
      .filter((entry) => entry.length > 0);

  /**
   * Check both address lists before the request leaves the browser.
   *
   * The API refuses the same shapes (and that refusal is proven in the integration walk), but a
   * reader who mistypes an address deserves the message on the field, not a console error.
   */
  const invalidListEntry = (field: "ip_allowlist" | "ip_denylist", text: string): string | null => {
    for (const line of listFrom(text)) {
      const problem = invalidAddressLine(line);
      if (problem) {
        setTab("addresses");
        setFieldError({ field, message: problem });
        return problem;
      }
    }
    return null;
  };

  const save = async () => {
    if (!policy) return;
    setBusy(true);
    setError(null);
    setFieldError(null);
    setNotice(null);

    const numeric = numericPatch();
    if (!numeric.ok) {
      const spec = numeric.spec;
      setTab(TABS.find((entry) => entry.fields.some((field) => field.key === spec.key))?.id ?? tab);
      setFieldError({
        field: spec.key,
        message: `${spec.label} must be a whole number between ${spec.min} and ${spec.max}.`,
      });
      setBusy(false);
      return;
    }

    // The address lists are checked here too: a mistyped network belongs on its field, and the
    // API's own refusal stays the authority the integration walk pins.
    if (invalidListEntry("ip_allowlist", allowlist) || invalidListEntry("ip_denylist", denylist)) {
      setBusy(false);
      return;
    }

    try {
      const saved = await updateSecurityPolicy(
        {
          ...numeric.patch,
          ip_allowlist: listFrom(allowlist),
          ip_denylist: listFrom(denylist),
          mfa_required: mfaRequired,
        },
        platformAccount ? selectedOrg : null,
      );
      apply(saved.after);
      setChanges(saved.changes);
      setNotice(
        saved.changes.length === 0
          ? "Nothing needed saving — the policy already matched."
          : `Saved ${saved.changes.length} change${saved.changes.length === 1 ? "" : "s"}.`,
      );
    } catch (cause) {
      if (cause instanceof ApiError) {
        const field = typeof cause.details?.field === "string" ? cause.details.field : null;
        if (field) {
          setFieldError({ field, message: cause.message });
        }
        setError(cause.message);
      } else {
        setError("The policy could not be saved.");
      }
    } finally {
      setBusy(false);
    }
  };

  const activeTab = TABS.find((entry) => entry.id === tab) ?? TABS[0];

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="max-w-2xl text-[12.5px] text-muted">
          Every threshold on this screen is read by the sign-in path itself: the lockout decides
          when an account is refused, the session lifetimes decide when a session ends, and the
          address lists are checked before a password is ever verified.
        </p>
        <div className="flex items-center gap-2">
          {platformAccount ? (
            <label className="flex items-center gap-2 text-[12.5px] text-muted">
              Organization
              <select
                value={selectedOrg ?? ""}
                data-security-org
                onChange={(event) => setSelectedOrg(event.target.value)}
                className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              >
                {(organizations ?? []).map((organization) => (
                  <option key={organization.id} value={organization.id}>
                    {organization.name}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <button
            type="button"
            onClick={() => void load(activeOrg)}
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:bg-panel"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Reload
          </button>
        </div>
      </div>

      {status === "loading" ? (
        <div className="flex flex-col gap-3" data-security-loading>
          <div className="h-9 w-72 animate-pulse rounded-lg bg-panel" />
          <div className="h-40 animate-pulse rounded-xl bg-panel" />
        </div>
      ) : null}

      {status === "error" && loadError ? (
        <div
          role="alert"
          data-security-load-error
          className="flex flex-col gap-2 rounded-xl border border-danger/40 bg-danger-soft p-4 text-[12.5px] text-caution"
        >
          <span className="font-medium">{loadError.message}</span>
          <button
            type="button"
            onClick={() => void load(activeOrg)}
            className="w-fit rounded-lg border border-caution/40 px-3 py-1 text-[12px] text-caution"
          >
            Try again
          </button>
        </div>
      ) : null}

      {status === "ready" && policy ? (
        <>
          <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="Policy families">
            {TABS.map((entry) => {
              const Icon = entry.icon;
              const active = entry.id === tab;
              return (
                <button
                  key={entry.id}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  data-policy-tab={entry.id}
                  onClick={() => setTab(entry.id)}
                  className={`flex items-center gap-1.5 rounded-lg border px-3 py-1.5 text-[12.5px] transition ${
                    active
                      ? "border-accent bg-accent-soft text-accent-strong"
                      : "border-line text-muted hover:bg-panel"
                  }`}
                >
                  <Icon className="size-3.5" aria-hidden />
                  {entry.label}
                </button>
              );
            })}
          </div>

          <section
            className="flex flex-col gap-4 rounded-xl border border-line bg-surface p-4"
            data-policy-panel={activeTab.id}
          >
            <p className="text-[12.5px] text-muted">{activeTab.blurb}</p>

            {activeTab.fields.length > 0 ? (
              <div className="grid gap-3 sm:grid-cols-2">
                {activeTab.fields.map((spec) => {
                  const invalid = fieldError?.field === spec.key;
                  return (
                    <label key={spec.key} className="flex flex-col gap-1.5">
                      <span className="text-[12.5px] font-medium text-ink">{spec.label}</span>
                      <input
                        value={draft[spec.key] ?? ""}
                        data-policy-input={spec.key}
                        inputMode="numeric"
                        onChange={(event) =>
                          setDraft((current) => ({ ...current, [spec.key]: event.target.value }))
                        }
                        className={`h-9 rounded-lg border bg-surface px-2 text-[13px] text-ink outline-none focus:ring-2 ${
                          invalid
                            ? "border-danger focus:border-danger focus:ring-danger/20"
                            : "border-line focus:border-accent focus:ring-accent/15"
                        }`}
                      />
                      <span className="text-[11.5px] text-muted">{spec.hint}</span>
                      {invalid ? (
                        <span
                          role="alert"
                          data-policy-field-error={spec.key}
                          className="text-[11.5px] text-caution"
                        >
                          {fieldError?.message}
                        </span>
                      ) : null}
                    </label>
                  );
                })}
              </div>
            ) : null}

            {activeTab.id === "lockout" ? (
              <label className="flex items-center gap-2 text-[12.5px] text-ink">
                <input
                  type="checkbox"
                  checked={mfaRequired}
                  data-policy-mfa-required
                  onChange={(event) => setMfaRequired(event.target.checked)}
                  className="size-4 rounded border-line"
                />
                Require a second factor for this organization
                <span className="text-[11.5px] text-muted">
                  (accounts that enrol one are asked for it at sign-in)
                </span>
              </label>
            ) : null}

            {activeTab.id === "addresses" ? (
              <div className="grid gap-3 lg:grid-cols-2">
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Allowed networks</span>
                  <textarea
                    value={allowlist}
                    data-policy-input="ip_allowlist"
                    rows={5}
                    spellCheck={false}
                    onChange={(event) => setAllowlist(event.target.value)}
                    placeholder={"10.0.0.0/8\n192.168.1.5"}
                    className={`rounded-lg border bg-surface p-2 font-mono text-[12px] text-ink outline-none focus:ring-2 ${
                      fieldError?.field === "ip_allowlist"
                        ? "border-danger focus:border-danger focus:ring-danger/20"
                        : "border-line focus:border-accent focus:ring-accent/15"
                    }`}
                  />
                  <span className="text-[11.5px] text-muted">
                    One address or network per line; empty means any address may sign in.
                  </span>
                  {fieldError?.field === "ip_allowlist" ? (
                    <span role="alert" data-policy-field-error="ip_allowlist" className="text-[11.5px] text-caution">
                      {fieldError.message}
                    </span>
                  ) : null}
                </label>
                <label className="flex flex-col gap-1.5">
                  <span className="text-[12.5px] font-medium text-ink">Denied networks</span>
                  <textarea
                    value={denylist}
                    data-policy-input="ip_denylist"
                    rows={5}
                    spellCheck={false}
                    onChange={(event) => setDenylist(event.target.value)}
                    placeholder={"203.0.113.0/24"}
                    className={`rounded-lg border bg-surface p-2 font-mono text-[12px] text-ink outline-none focus:ring-2 ${
                      fieldError?.field === "ip_denylist"
                        ? "border-danger focus:border-danger focus:ring-danger/20"
                        : "border-line focus:border-accent focus:ring-accent/15"
                    }`}
                  />
                  <span className="text-[11.5px] text-muted">
                    Deny wins over allow; a refused address never reaches the password check.
                  </span>
                  {fieldError?.field === "ip_denylist" ? (
                    <span role="alert" data-policy-field-error="ip_denylist" className="text-[11.5px] text-caution">
                      {fieldError.message}
                    </span>
                  ) : null}
                </label>
              </div>
            ) : null}
          </section>

          <div className="sticky bottom-0 flex flex-wrap items-center gap-3 rounded-xl border border-line bg-surface/95 p-3 backdrop-blur">
            <button
              type="button"
              onClick={() => void save()}
              disabled={busy}
              data-policy-save
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
            >
              <Save className="size-3.5" aria-hidden />
              {busy ? "Saving…" : "Save changes"}
            </button>
            <button
              type="button"
              onClick={() => apply(policy)}
              disabled={busy}
              className="rounded-lg border border-line px-3.5 py-1.5 text-[12.5px] text-ink transition hover:bg-panel"
            >
              Discard
            </button>
            {notice ? (
              <span data-policy-notice className="text-[12.5px] text-muted">
                {notice}
              </span>
            ) : null}
            <span className="ml-auto text-[11.5px] text-muted">
              Last change {new Date(policy.updated_at).toLocaleString()}
            </span>
          </div>

          {error ? (
            <p
              role="alert"
              data-policy-error
              className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
            >
              {error}
            </p>
          ) : null}

          {changes && changes.length > 0 ? (
            <section
              aria-label="Saved changes"
              data-policy-diff
              className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4"
            >
              <h2 className="text-[12.5px] font-medium text-ink">Saved changes</h2>
              <ul className="flex flex-col gap-1.5">
                {changes.map((change) => (
                  <li key={change.field} className="flex flex-wrap items-center gap-2 text-[12px]">
                    <span className="font-mono text-ink">{change.field}</span>
                    <span className="text-muted line-through">{change.before || "—"}</span>
                    <span className="text-muted">→</span>
                    <span className="text-ink">{change.after || "—"}</span>
                  </li>
                ))}
              </ul>
              <p className="text-[11.5px] text-muted">
                The audit trail carries the same list; the security centre reads it back.
              </p>
            </section>
          ) : null}
        </>
      ) : null}
    </div>
  );
}
