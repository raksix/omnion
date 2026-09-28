import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { TracesView } from "@/features/observability/traces-view";

export const metadata = { title: "Traces" };

/**
 * `/observability/traces` — the trace search and the waterfall for one trace (REQ-126, slice 3).
 *
 * The screen is fed from the index, never from a hard-coded list, and it says so on every row:
 * the `sampled because` chip is the reason a trace is in the index at all, and "why do I have
 * this one but not the one next to it" is the question an operator arrives with. The waterfall is
 * drawn from the span records the index kept, and a trace over the cap says it is truncated
 * rather than rendering a short chart that looks complete.
 */
export default function ObservabilityTracesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Traces"
        description="Search the span index by request id, route, status or duration, and read the waterfall for one trace"
      >
        <TracesView />
      </AppShell>
    </RequireAuth>
  );
}
