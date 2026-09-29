import { SalesReportsView } from "@/features/sales/reports-view";

/**
 * The sales report (REQ-052, slice 4b): `/sales/reports`.
 *
 * A page rather than a route component, because the view reads its filter from the URL — the
 * window, the status and the unassigned toggle all live there, so a report somebody is looking at
 * over somebody's shoulder is a link and not a story about what they had typed.
 */
export default function SalesReportsPage() {
  return <SalesReportsView />;
}
