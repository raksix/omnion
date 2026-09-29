import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ThemeHistoryView } from "@/features/themes/theme-history-view";

export const metadata = { title: "Theme settings history" };

/** `/themes/<key>/history` — the settings revision list with per-field diffs and a restore. */
export default function ThemeHistoryRoute({
  params,
}: {
  params: Promise<{ key: string }>;
}) {
  return (
    <RequireAuth>
      <AppShell
        title="Settings history"
        description="Every save appends a revision. Restoring one writes another, so nothing is ever lost"
      >
        <ThemeHistoryView />
      </AppShell>
    </RequireAuth>
  );
}
