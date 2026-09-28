import type { ReactNode } from "react";

import { CrmTenantProvider } from "@/features/crm/crm-tenant";

/**
 * The CRM section's frame: the one place that decides **which organization's** CRM the panel is
 * looking at (REQ-051).
 *
 * It is a layout rather than a hook each screen calls because the decision is not the screen's
 * business. `/crm/contacts` and `/crm/deals` opened on the same account must not be able to show
 * two different tenants, and six screens each resolving their own is six chances to disagree.
 */
export default function CrmLayout({ children }: { children: ReactNode }) {
  return <CrmTenantProvider>{children}</CrmTenantProvider>;
}
