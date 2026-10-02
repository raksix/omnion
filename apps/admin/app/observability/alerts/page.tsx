import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AlertsView } from "@/features/observability/alerts-view";

export const metadata = { title: "Alerts" };

/**
 * `/observability/alerts` — the rules, their live state, the timeline and the silences
 * (REQ-126, slice 4).
 *
 * The screen makes four distinctions visible that otherwise render as the same chip: pending vs
 * firing, no-data vs under-threshold, a rule whose expression stopped parsing vs a healthy rule,
 * and a silence vs a resolution. Each of those is a case where the lazy rendering tells an
 * operator the opposite of the truth.
 */
export default function ObservabilityAlertsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Alerts"
        description="Rules evaluated against the live metric registry, with their dwell, their silences and the notification each firing event claimed exactly once"
      >
        <AlertsView />
      </AppShell>
    </RequireAuth>
  );
}
