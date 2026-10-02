import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PatternLibrary } from "@/features/blocks/pattern-library";

export const metadata = { title: "Patterns" };

export default function PatternsRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Patterns"
        description="Reusable block groups — build a page once, drop the group into the next"
      >
        <PatternLibrary />
      </AppShell>
    </RequireAuth>
  );
}
