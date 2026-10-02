import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiTelemetry } from "@/features/ai/ai-telemetry";

export const metadata = { title: "Tool telemetry" };

/**
 * `/ai/telemetry` — the tool telemetry screen (REQ-107, slice 5).
 *
 * A top-level screen rather than a tab inside `/ai`, because the question it answers is not about
 * configuration: it is "which tool is costing us, and is that cost going into something that
 * works". The AI Hub is where you change the platform; this is where you read what the change did.
 *
 * It reads `GET /ai/telemetry/tools`, which is scoped to `ai.telemetry.read` — deliberately a
 * different key from `ai.evals.read`, because a run's score and a tool's failure rate are
 * different secrets and a panel that merged them would hand one permission both.
 */
export default function AiTelemetryPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Tool telemetry"
        description="What every tool has cost and how often it worked. Denials are counted apart from failures — a denial is a permission saying no, a failure is a tool breaking, and the fix is different"
      >
        <AiTelemetry />
      </AppShell>
    </RequireAuth>
  );
}