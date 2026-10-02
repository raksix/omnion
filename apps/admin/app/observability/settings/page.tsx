import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SettingsView } from "@/features/observability/settings-view";

export const metadata = { title: "Observability settings" };

/**
 * `/observability/settings` — sampling, retention, log levels, the cardinality budget, and what
 * leaves the instance when an exporter is configured (REQ-126, slice 4).
 *
 * Every field on this screen takes effect without a restart, which is the request's own stated
 * reason for it existing ("debugging does not need a redeploy") and the half a unit test cannot
 * see. The caps come from the database, so the refusal a save gets back names the field and the
 * limit it enforced.
 */
export default function ObservabilitySettingsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Observability settings"
        description="Sampling, retention, temporary log-level raises and the cardinality budget — every one applied without a restart"
      >
        <SettingsView />
      </AppShell>
    </RequireAuth>
  );
}
