import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ExportersView } from "@/features/observability/exporters-view";

export const metadata = { title: "Exporters" };

/**
 * `/observability/exporters` — the telemetry sinks, their health and their drop counters
 * (REQ-126, slice 3).
 *
 * The screen exists to make one trade-off legible: buffered telemetry loses data by design when a
 * backend is down, and the drop counter plus the health chip make that honest instead of silent.
 * The egress statement is on the payload and above the table, because "what leaves this instance"
 * is a sentence a security review asks for and a sentence that lives only in a component is a
 * sentence nobody can audit.
 */
export default function ObservabilityExportersPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Exporters"
        description="Where this instance sends its telemetry, whether the backend is answering, and what the buffering has cost so far"
      >
        <ExportersView />
      </AppShell>
    </RequireAuth>
  );
}
