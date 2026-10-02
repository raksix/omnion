import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ThemesView } from "@/features/themes/themes-view";

export const metadata = { title: "Themes" };

/**
 * `/themes` — the theme gallery (REQ-062, slice 1).
 *
 * Carries `themes.read` for the gallery and `themes.activate` for the two buttons that change
 * what a visitor sees, so the page checks no permission itself: the route guard answers `403`
 * and the panel renders its own state. A second, client-side check would be a second answer to
 * the same question, and the two would eventually disagree.
 */
export default function ThemesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Themes"
        description="What this site renders with, what else it could render with, and one click back to the theme it had"
      >
        <ThemesView />
      </AppShell>
    </RequireAuth>
  );
}
