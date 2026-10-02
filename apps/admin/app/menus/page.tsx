import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MenusView } from "@/features/menus/menus-view";

export const metadata = { title: "Menus" };

/**
 * `/menus` — the site's navigation (REQ-064, slice 1).
 *
 * Carries `menus.read` for the list and `menus.manage` for every write, so the page does not
 * check a permission itself: the route guard answers `403` and the panel renders its own state.
 * A second, client-side permission check would be a second answer to the same question, and the
 * two would eventually disagree.
 */
export default function MenusPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Menus"
        description="What a visitor can reach, and where each menu renders"
      >
        <MenusView />
      </AppShell>
    </RequireAuth>
  );
}
