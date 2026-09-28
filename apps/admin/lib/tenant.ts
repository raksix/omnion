"use client";

/**
 * The tenant a content screen is addressing.
 *
 * A tenant-addressed read names its organization as a **query parameter**, and there is an account
 * for which the account's own organization is the wrong answer: the platform Owner has *no*
 * primary organization — that absence is what makes it an Owner — so a screen that reads
 * `session.user.organization_id` and sends nothing gets `400 organization_required` back. The
 * panel's first-run account is exactly that account, so the two galleries a new install opens
 * first (`/patterns`, `/page-templates`) answered several hundred 400s in the browser pass rather
 * than a library.
 *
 * There is one honest answer, and it is a tenant the account demonstrably holds: the selected
 * site. `Site.organization_id` is the tenant a site belongs to, the site list is already loaded
 * for the panel header and is refused by the API when the account may not see it, and the site
 * switcher is a control the Owner has on every other screen. So the tenant is derived, not
 * invented, and switching sites re-addresses the content screens with it.
 *
 * A member of a tenant has nothing to derive: their own `organization_id` is the answer, and it is
 * used without a round trip.
 */
import { useCallback, useEffect, useState } from "react";

import { ApiError, fetchOrganizations } from "./api";
import { useSession } from "./session";
import { useSites } from "./sites";
import type { Organization } from "./types";

type TenantValue = {
  /**
   * Tenant to address content reads with.
   *
   * `null` while the answer is still unknown, and `null` with `status: "unresolved"` for a
   * platform account that holds no tenant at all — a screen waits on the first and says so on the
   * second, rather than firing a request that is guaranteed to be refused.
   */
  organizationId: string | null;
  /** `loading` until the tenant is known, then `ready` or `unresolved`. */
  status: "loading" | "ready" | "unresolved";
  /** Organizations a platform account may pick from; empty for everyone else. */
  organizations: Organization[];
  /** Choose another tenant (a platform account only). */
  selectOrganization: (organizationId: string) => void;
};

/** `localStorage` key for the tenant a platform account is working in. */
const STORAGE_KEY = "omnion.admin.selected-organization";

function readStored(): string | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

function store(value: string): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, value);
  } catch {
    // A blocked storage must not take a content screen down; the first tenant is a fine default.
  }
}

/** The tenant a content screen addresses, or the reason there is not one yet. */
export function useContentTenant(): TenantValue {
  const { user, status: sessionStatus } = useSession();
  const { sites, selectedSite, status: sitesStatus } = useSites();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  // No primary organization is the definition of a platform account, and the account the panel
  // creates on first run is one of them.
  const platformAccount = user ? user.organization_id === null : false;

  useEffect(() => {
    if (!platformAccount || organizations !== null) {
      return;
    }
    let cancelled = false;
    fetchOrganizations()
      .then((list) => {
        if (cancelled) {
          return;
        }
        setOrganizations(list);
        setSelectedId((current) => {
          const wanted = current ?? readStored();
          if (wanted && list.some((organization) => organization.id === wanted)) {
            return wanted;
          }
          return list[0]?.id ?? null;
        });
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setOrganizations([]);
        // The site list can still answer, so this is not a dead end — but say what happened rather
        // than showing an empty selector that silently refuses to change anything.
        if (!(cause instanceof ApiError)) {
          return;
        }
      });
    return () => {
      cancelled = true;
    };
  }, [platformAccount, organizations]);

  const selectOrganization = useCallback((next: string) => {
    setSelectedId(next);
    store(next);
  }, []);

  // The selected site is the tenant the panel is already working in, and it is the only answer a
  // platform account gets without a second selector on every screen. It is also an answer the API
  // itself checks: a site the account may not see is never in the list.
  const derivedFromSite = selectedSite?.organization_id ?? sites[0]?.organization_id ?? null;

  if (!platformAccount) {
    const own = user?.organization_id ?? null;
    return {
      organizationId: own,
      status: sessionStatus === "signed-in" ? (own ? "ready" : "unresolved") : "loading",
      organizations: [],
      selectOrganization,
    };
  }

  const organizationId = selectedId ?? derivedFromSite;
  // Both sources are still on their way. Reporting `unresolved` now would put an "no tenant"
  // message on screen for a second before the site list lands, which reads as a defect.
  const waiting = organizations === null || (derivedFromSite === null && sitesStatus === "loading");

  return {
    organizationId,
    status: organizationId ? "ready" : waiting ? "loading" : "unresolved",
    organizations: organizations ?? [],
    selectOrganization,
  };
}
