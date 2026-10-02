"use client";

/**
 * The organization a request works on.
 *
 * The API resolves the target of every request with `resolve_organization`, which follows one
 * rule: an account *with* a primary organization always works inside it, and an account
 * *without* one — a platform account, a superuser — has to name the tenant it is working on.
 * Without a field to put that name in, the API answers `organization_required`, and the screen
 * renders a refusal where a list should be.
 *
 * Three things this hook exists to stop happening, all of which were live bugs:
 *
 * 1. **A platform account's screen silently unreadable.** Its own `organization_id` is `null`,
 *    so a screen that sends the session's value and nothing else asks for nothing and is
 *    refused. The node-package installer had exactly this and the browser pass proved it.
 * 2. **A tenant's screen broken by the same fix.** So the tenant's value is still used and
 *    still wins: an account that names its own organization never has to see a control.
 * 3. **The picker copied by hand into every screen.** It was three near-identical blocks
 *    before this file, and a fourth copy is how a fix misses one. Here it is written once.
 */
import { useEffect, useState } from "react";

import { fetchOrganizations } from "./api";
import { useSession } from "./session";
import type { Organization } from "./types";

export type OrganizationScope = {
  /** Whether the signed-in account works across tenants rather than inside one. */
  platformAccount: boolean;
  /** The organization the next request should name; `null` for a tenant that needs none. */
  organizationId: string | null;
  /** The tenants a platform account may pick from, once the list has arrived. */
  organizations: Organization[] | null;
  /** Pick a tenant. Meaningless for a tenant's own account, which has only one. */
  setOrganizationId: (id: string | null) => void;
  /**
   * Whether there is nothing to read *yet*.
   *
   * True for a platform account before the organization list arrives. A screen that reads
   * anyway renders the API's `organization_required` — an error the reader caused by
   * arriving, which is worse than saying nothing has been chosen.
   */
  needsOrganization: boolean;
};

/** The organization this panel's requests work on, and the picker that changes it. */
export function useOrganizationScope(): OrganizationScope {
  const { user } = useSession();
  const platformAccount = user ? user.organization_id === null : false;
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [picked, setPicked] = useState<string | null>(null);

  // A tenant never sees the picker, so it is never loaded: the list is a platform account's
  // read, and a tenant making it on every screen is a request it may not be allowed to make.
  useEffect(() => {
    if (!platformAccount || organizations !== null) return;
    let alive = true;
    void fetchOrganizations()
      .then((list) => {
        if (!alive) return;
        setOrganizations(list);
        // Default to the first tenant so the screen has something to show on arrival. An
        // empty list leaves the picker empty, which is the honest answer for it.
        setPicked(list[0]?.id ?? null);
      })
      .catch(() => {
        // A platform account that cannot read the tenant list is not a failed screen; it
        // simply has nothing to pick, and the empty picker says so.
        if (alive) setOrganizations([]);
      });
    return () => {
      alive = false;
    };
  }, [platformAccount, organizations]);

  const organizationId = platformAccount ? picked : (user?.organization_id ?? null);
  return {
    platformAccount,
    organizationId,
    organizations,
    setOrganizationId: setPicked,
    needsOrganization: platformAccount && picked === null,
  };
}

/** `?organization_id=…` for the read routes, or an empty string. */
export function scopeQuery(organizationId: string | null | undefined): string {
  return organizationId ? `?organization_id=${encodeURIComponent(organizationId)}` : "";
}
