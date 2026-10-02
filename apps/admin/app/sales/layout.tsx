import type { ReactNode } from "react";

import { SalesTenantProvider } from "@/features/sales/sales-parts";

/**
 * The sales section's frame: the one place that decides **which organization's** sales the panel is
 * looking at (REQ-052).
 *
 * A layout rather than a hook each screen calls, because the decision is not a screen's business:
 * `/sales/catalog` and `/sales/pricelists` opened on the same platform account must not be able to
 * show two different tenants, and the API refuses such an account that has not named one — advice
 * that is impossible to follow if the name is held in one screen only.
 */
export default function SalesLayout({ children }: { children: ReactNode }) {
  return <SalesTenantProvider>{children}</SalesTenantProvider>;
}
