"use client";

/**
 * The status of the tenant the panel is working in (REQ-005, slice 3).
 *
 * A suspended organization refuses every write at the API and keeps every read, which means the
 * panel has to *say* so rather than let a person discover it one refused click at a time: the
 * banner is the difference between "this tenant is suspended" and "the Save button is broken".
 *
 * It does fetch `/me/organizations` itself, which the switcher beside it also fetches. That is
 * one duplicate request per signed-in mount, and it is worth it: the switcher owns the list
 * because it has to (search, active row, its own re-read after a switch), while this owns the
 * *answer* the shell needs on every screen. Lifting the switcher's copy into a shared store
 * would couple the header chrome to the banner, and a banner that only exists where the
 * switcher happens to be is a banner that disappears from half the panel.
 */
import { createContext, useContext, useEffect, useMemo, useState } from "react";

import { ApiError, fetchMyOrganizations, type AccountOrganization } from "./api";
import { useSession } from "./session";

type TenantStatusValue = {
  /** The tenant the session is working in, once the list has loaded. */
  organization: AccountOrganization | null;
  /** `active`, `suspended`, `archived`, or `null` while unknown. */
  status: string | null;
  /** `true` when the tenant accepts writes — the banner is hidden for an active one. */
  isFrozen: boolean;
  /** Why writes are refused, in the panel's own words. */
  reason: string | null;
  /** Re-read the list (a status change is made from the list screen). */
  reload: () => void;
};

const TenantStatusContext = createContext<TenantStatusValue | null>(null);

/** The sentence the banner shows for a frozen tenant, keyed by status. */
const REASONS: Record<string, string> = {
  suspended:
    "This organization is suspended. Everything stays readable, and nobody — not even an owner — can change anything until it is reactivated.",
  archived:
    "This organization is archived. It is kept for the record: its content is still readable, and it accepts no changes until it is reactivated.",
};

/** Provide the current tenant's status to the panel. */
export function TenantStatusProvider({ children }: { children: React.ReactNode }) {
  const { status: sessionStatus } = useSession();
  const [organizations, setOrganizations] = useState<AccountOrganization[]>([]);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    if (sessionStatus !== "signed-in") {
      setOrganizations([]);
      setCurrentId(null);
      return;
    }
    let cancelled = false;
    fetchMyOrganizations()
      .then((body) => {
        if (cancelled) return;
        setOrganizations(body.organizations);
        setCurrentId(body.current_organization_id);
      })
      .catch(() => {
        // A banner that cannot load must not take the panel down: the API is the enforcement
        // point, and a person who hits a refused write is told the reason there.
        if (!cancelled) setOrganizations([]);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionStatus, reloadToken]);

  const value = useMemo<TenantStatusValue>(() => {
    const organization = organizations.find((entry) => entry.organization_id === currentId) ?? null;
    const status = organization?.organization_status ?? null;
    const isFrozen = status !== null && status !== "active";
    return {
      organization,
      status,
      isFrozen,
      reason: isFrozen ? (REASONS[status!] ?? REASONS.suspended!) : null,
      reload: () => setReloadToken((token) => token + 1),
    };
  }, [organizations, currentId]);

  return <TenantStatusContext.Provider value={value}>{children}</TenantStatusContext.Provider>;
}

/** Read the current tenant's status; only valid inside [`TenantStatusProvider`]. */
export function useTenantStatus(): TenantStatusValue {
  const value = useContext(TenantStatusContext);
  if (!value) {
    throw new Error("useTenantStatus must be used inside <TenantStatusProvider>");
  }
  return value;
}

/** The `ApiError` message for a refused write, or `null` when the failure is something else. */
export function isFrozenTenantError(cause: unknown): cause is ApiError {
  return cause instanceof ApiError && cause.code === "organization_not_writable";
}
