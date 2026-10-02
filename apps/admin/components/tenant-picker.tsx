"use client";

/**
 * The tenant selector a content screen shows when the signed-in account is a platform account.
 *
 * The IAM screens already carry one of these; the content galleries did not, and that is why they
 * answered `400 organization_required` on the first-run account. The control is here rather than
 * inlined three times because "which tenant is this screen about" is one question, and three
 * copies of it drift — the third one is what shipped without it.
 *
 * It renders nothing for a member of a tenant: their own organization is the answer and a selector
 * over a single value is a dead control.
 */
import { Building2 } from "lucide-react";

import type { Organization } from "@/lib/types";

type Props = {
  /** Organizations the account may pick from. */
  organizations: Organization[];
  /** The tenant currently addressed, or `null` when none could be resolved. */
  organizationId: string | null;
  /** Choose another tenant. */
  onSelect: (organizationId: string) => void;
  /** `data-` attribute the walkthrough asserts on. */
  testId?: string;
};

/** The tenant selector, or nothing when the account belongs to exactly one tenant. */
export function TenantPicker({ organizations, organizationId, onSelect, testId }: Props) {
  if (organizations.length === 0) {
    return null;
  }
  return (
    <label className="flex items-center gap-2 text-[12.5px]">
      <span className="flex items-center gap-1.5 text-muted">
        <Building2 className="h-3.5 w-3.5" aria-hidden="true" />
        Organization
      </span>
      <select
        value={organizationId ?? ""}
        data-tenant-picker={testId ?? "organization"}
        onChange={(event) => onSelect(event.target.value)}
        className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
      >
        {organizationId === null ? <option value="">Select an organization</option> : null}
        {organizations.map((organization) => (
          <option key={organization.id} value={organization.id}>
            {organization.name}
          </option>
        ))}
      </select>
    </label>
  );
}
