"use client";

/**
 * Which organization's CRM the panel is looking at (REQ-051).
 *
 * The API refuses a read that names no organization for an account that has no **primary** one,
 * and that account is a legitimate one: the platform `owner` created during first-run setup has
 * `organization_id = null` by design, because it is the account that *makes* the tenants. It
 * holds a live binding in whichever organizations it has been given, which is not the same thing
 * as having a primary organization — the column is a convenience, and this screen is where the
 * convenience runs out.
 *
 * So this is one small piece of shared state, in its own file because all six screens need it and
 * a hook re-declared six times is six places to forget. It resolves in three steps, and the order
 * is the whole design:
 *
 * 1. **An organization account answers its own.** No request is made and no picker is drawn: the
 *    screen is not going to offer somebody a choice they do not have.
 * 2. **A named `organization_id` in the URL wins.** The tenant is list state like everything else
 *    on these screens, so a link, the back button and a bookmark all land on the same rows.
 * 3. **Otherwise, one organization is taken automatically; two or more are a decision.** A picker
 *    is drawn and the screens stay in a state that says *choose* rather than in a half-loaded one.
 *
 * Rule 3 is the API's rule, not a panel-side invention: `organization_of` in
 * `apps/api/src/routes/crm.rs` falls back to the caller's single binding and refuses with
 * `organization_ambiguous` when there are two. The two halves used to disagree — the panel drew a
 * chooser for an account the API would have answered, so the screen never reached a list at all,
 * and the walkthrough's whole CRM suite measured that chooser. A panel that is stricter than its
 * API does not protect anything: it just makes the feature unreachable.
 *
 * A platform account with **no** organization is not an error state to apologise for — it is a
 * correct account that has nothing to show yet, and the copy says so in one line with the action
 * that would change it.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import { Building2 } from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter, useSearchParams } from "next/navigation";

import { fetchOrganizations } from "@/lib/api";
import { useSession } from "@/lib/session";

/** Where the panel stands on "which organization". */
export type CrmTenant = {
  /** The organization to read, or `null` while it is still being decided. */
  organizationId: string | null;
  /** `true` for a platform account, which is the only kind that gets a picker. */
  platformAccount: boolean;
  /** The organizations the account may read, once they are known. */
  organizations: { id: string; name: string }[];
  /** Pick another organization. */
  select: (organizationId: string) => void;
  /** `true` while the organization list is on its way. */
  loading: boolean;
  /**
   * The gate every screen draws instead of its list: `children` is the screen, and this is
   * everything a screen would otherwise have to re-decide about tenancy.
   */
  children: ReactNode;
};

const CrmTenantContext = createContext<CrmTenant | null>(null);

/** The tenant in force, and the gate that draws around it. */
export function useCrmTenant(): CrmTenant {
  const value = useContext(CrmTenantContext);
  if (!value) {
    throw new Error("useCrmTenant must be used inside <CrmTenantProvider>");
  }
  return value;
}

export function CrmTenantProvider({ children }: { children: ReactNode }) {
  const { user } = useSession();
  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();

  const platformAccount = user ? user.organization_id === null : false;
  const fromUrl = searchParams.get("organization_id");
  const [organizations, setOrganizations] = useState<{ id: string; name: string }[] | null>(null);

  const select = useCallback(
    (organizationId: string) => {
      const next = new URLSearchParams(searchParams.toString());
      next.set("organization_id", organizationId);
      router.replace(`${pathname}?${next.toString()}`, { scroll: false });
    },
    [pathname, router, searchParams],
  );

  useEffect(() => {
    // Only a platform account ever needs the list; asking for it as an organization account
    // would be a request whose answer is always one row and always the wrong shape to draw.
    if (!platformAccount || organizations !== null) return;
    let live = true;
    void fetchOrganizations()
      .then((list) => {
        if (live) setOrganizations(list.map((organization) => ({ id: organization.id, name: organization.name })));
      })
      .catch(() => {
        // A picker that cannot list the tenants is worse than none: it would offer a choice it
        // cannot keep. The screen below reports the failure in its own error state instead.
        if (live) setOrganizations([]);
      });
    return () => {
      live = false;
    };
  }, [organizations, platformAccount]);

  const value = useMemo<CrmTenant>(() => {
    // The panel's rule and the API's rule have to be the same rule, or one of the two is
    // unreachable. `organization_of` in `apps/api/src/routes/crm.rs` falls back to the caller's
    // single binding when a platform account names no organization and refuses with
    // `organization_ambiguous` when there are two or more — so a platform account in exactly one
    // organization gets that organization, and only a real choice draws a chooser.
    //
    // The panel used to require an explicit choice even with one organization to choose from, so
    // it never reached a list the API would have answered. The chooser is not a safety feature:
    // the API already refuses the ambiguous case, and this screen is the place that refusal is
    // turned into a sentence.
    const single = organizations?.length === 1 ? organizations[0].id : null;
    const organizationId = fromUrl || user?.organization_id || single || null;
    return {
      organizationId,
      platformAccount,
      organizations: organizations ?? [],
      select,
      loading: platformAccount && organizations === null,
      children,
    };
  }, [children, fromUrl, organizations, platformAccount, select, user?.organization_id]);

  return <CrmTenantContext.Provider value={value}>{gate(value)}</CrmTenantContext.Provider>;
}

/** Draw the screen, or the one thing that has to come first. */
function gate(tenant: CrmTenant): ReactNode {
  const { children, loading, organizationId, organizations, platformAccount, select } = tenant;

  if (loading) {
    return (
      <div className="mt-4 rounded-xl border border-line bg-surface px-4 py-6" aria-busy="true">
        <p className="text-[12px] text-muted">Loading organizations…</p>
      </div>
    );
  }

  if (!platformAccount) {
    return children;
  }

  // A platform account with a tenant can work; the picker is offered but not demanded.
  if (organizationId) {
    return (
      <>
        {organizations.length > 1 ? <TenantPicker tenant={tenant} /> : null}
        {children}
      </>
    );
  }

  return <NoTenant organizations={organizations} />;
}

/** The chooser, drawn above the screen rather than inside its toolbar. */
function TenantPicker({ tenant }: { tenant: CrmTenant }) {
  const { organizationId, organizations, select } = tenant;
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2 rounded-xl border border-line bg-surface px-3 py-2">
      <label className="flex items-center gap-1.5 text-[12px] text-muted">
        <Building2 className="size-3.5" aria-hidden />
        <span className="sr-only">Organization</span>
        Organization
      </label>
      <select
        value={organizationId ?? ""}
        data-crm-organization
        onChange={(event) => select(event.target.value)}
        className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
      >
        {organizations.map((organization) => (
          <option key={organization.id} value={organization.id}>
            {organization.name}
          </option>
        ))}
      </select>
    </div>
  );
}

/** A platform account with nothing to read yet. */
function NoTenant({ organizations }: { organizations: { id: string; name: string }[] }) {
  return (
    <div className="mt-4 rounded-xl border border-line bg-surface px-4 py-10 text-center">
      <Building2 className="mx-auto size-5 text-muted" aria-hidden />
      <h2 className="mt-2 text-[13.5px] font-medium">No organization selected</h2>
      <p className="mx-auto mt-1 max-w-md text-[12px] text-muted">
        {organizations.length === 0
          ? "This account belongs to no organization yet. Create one, or ask an owner for a role in an existing one, and the CRM screens will fill in."
          : `This account is a member of ${organizations.length} organizations. Pick one above to read its CRM — the API will not guess, because the wrong guess is somebody else's records.`}
      </p>
      <div className="mt-3 flex justify-center gap-2">
        {organizations.length > 0 ? (
          <OrganizationLinks organizations={organizations} />
        ) : (
          <Link
            href="/tenancy/organizations"
            className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            Create an organization
          </Link>
        )}
      </div>
    </div>
  );
}

/** One link per organization, so "pick one" is clickable without a select. */
function OrganizationLinks({ organizations }: { organizations: { id: string; name: string }[] }) {
  return (
    <>
      {organizations.map((organization) => (
        <Link
          key={organization.id}
          href={`/crm/contacts?organization_id=${encodeURIComponent(organization.id)}`}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-ink transition hover:bg-panel"
        >
          {organization.name}
        </Link>
      ))}
    </>
  );
}
