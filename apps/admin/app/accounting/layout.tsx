import type { ReactNode } from "react";

/**
 * The accounting section's frame (REQ-054).
 *
 * A layout rather than a hook each screen calls, for the reason the sales and inventory sections
 * have one: a `/accounting/journal` and an `/accounting/accounts` opened on the same platform
 * account must not be able to resolve two different tenants, and advice about naming one is
 * impossible to follow when the name is held in one screen only.
 */
export default function AccountingLayout({ children }: { children: ReactNode }) {
  return <>{children}</>;
}
