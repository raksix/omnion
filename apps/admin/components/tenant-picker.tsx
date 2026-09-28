"use client";

/**
 * The tenant a platform account writes into (REQ-005, slice 4).
 *
 * An account *with* a primary organization never sees this: its tenant is decided by the
 * session, and a control for it would be a decoration. An account *without* one has to name a
 * tenant on every write, and the API says so with `400 organization_required` and a
 * `field: "organization_id"`. That answer is only useful if the panel can act on it, which is
 * what this is for — the refusal names a field, and a field you can be handed is a field you
 * can put a control next to.
 *
 * The failure this replaces is quiet rather than loud: a form that posts
 * `organization_id: null` gets a 400, prints "organization_id is required" in a banner above
 * the form, and offers no way to supply it. The reader is left looking for an input that does
 * not exist. So the control is rendered *before* the first attempt where it can be, and the
 * submission is refused in the browser with the same sentence the server would have used.
 */
import { Building2 } from "lucide-react";

import type { Organization } from "@/lib/types";

/** What the API answers when a write has no tenant to act on. */
export const ORGANIZATION_REQUIRED = "organization_required";

/** The field the API names, kept in one place so a rename is a single edit. */
export const ORGANIZATION_FIELD = "organization_id";

/**
 * `true` when a failure is the platform account naming no tenant.
 *
 * Accepts anything and answers `false` for it, so callers can hand it a caught value without
 * narrowing first.
 */
export function isTenantRequired(cause: unknown): boolean {
  if (typeof cause !== "object" || cause === null) return false;
  const code = (cause as { code?: unknown }).code;
  return code === ORGANIZATION_REQUIRED;
}

/** The sentence the panel shows instead of the raw API message, or `null` for other failures. */
export function tenantRequiredMessage(cause: unknown): string | null {
  if (!isTenantRequired(cause)) return null;
  return "This account works platform-wide, so it has no tenant of its own. Choose the organization this belongs to.";
}

/** Why the Create button is disabled, or `null` when the write may go out. */
export function tenantMissingReason(
  platformAccount: boolean,
  organizationId: string | null,
): string | null {
  if (!platformAccount) return null;
  if (organizationId) return null;
  return "Choose an organization — a platform account has no tenant of its own, so every write has to name one.";
}

/** Props of {@link TenantPicker}. */
export type TenantPickerProps = {
  /** The account that has to pick. `false` renders nothing at all. */
  platformAccount: boolean;
  /** The tenants this account may write into. */
  organizations: Organization[];
  /** The chosen tenant, or `null` for "not chosen yet". */
  value: string | null;
  /** Called with the new tenant. */
  onChange: (organizationId: string) => void;
  /** Attribute name for the select, so a walkthrough can find it. */
  testId: string;
  /** Which form it belongs to, shown in the label. */
  label?: string;
};

/**
 * A labelled organization select, rendered only for a platform account.
 *
 * A `<label>` wrapping the control rather than a placeholder: the rule the whole panel follows
 * is that every input says what it is, and a select whose only label is the placeholder of the
 * *first option* ("Select an organization") is a control that reads as a filter.
 */
export function TenantPicker({
  platformAccount,
  organizations,
  value,
  onChange,
  testId,
  label = "Organization",
}: TenantPickerProps) {
  if (!platformAccount) return null;

  if (organizations.length === 0) {
    return (
      <p
        className="flex items-start gap-1.5 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        data-tenant-required
        data-tenant-empty
      >
        <Building2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        No organization exists yet, so there is nothing to write into. Create one first.
      </p>
    );
  }

  return (
    <label className="flex flex-col gap-1.5 sm:max-w-sm" data-tenant-required>
      <span className="text-[12.5px] font-medium text-ink">{label}</span>
      <select
        value={value ?? ""}
        data-testid={testId}
        onChange={(event) => onChange(event.target.value)}
        className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
      >
        <option value="">Choose an organization…</option>
        {organizations.map((organization) => (
          <option key={organization.id} value={organization.id}>
            {organization.name}
          </option>
        ))}
      </select>
      <span className="text-[11.5px] text-muted">
        A platform account has no tenant of its own, so this is the tenant the new record belongs
        to.
      </span>
    </label>
  );
}
