import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PlanConsole } from "@/features/app-builder/plan-console";

export const metadata = { title: "App builder" };

/**
 * `/app-builder` — the plan console (docs/requests/REQ-045, slice 2).
 *
 * A sentence goes in, a plan comes out, and both live on this page: the composer and the
 * plans it produced, with the review workspace one click away. The composer and the table are
 * together on purpose — a generator whose output you cannot see next to the button that made it
 * is a generator you run twice.
 */
export default function AppBuilderPage() {
  return (
    <RequireAuth>
      <AppShell
        title="App builder"
        description="Describe an app in one sentence; review every artifact before anything is created"
      >
        <PlanConsole />
      </AppShell>
    </RequireAuth>
  );
}