import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ThemeBuilderView } from "@/features/themes/theme-builder-view";

export const metadata = { title: "Theme builder" };

/**
 * `/themes/<key>/builder` — the eight-slot block builder (REQ-062, slice 3).
 *
 * The `key` in the path is the theme the *gallery* came from; the builder itself reads the
 * site's active theme, because a slot row is `(site, theme, slot)` and the site's own theme is
 * the one whose copy of each slot the save writes. Accepting a key here and then editing a
 * different theme's slots would be the same lie `/themes/<key>/customize` had to be careful
 * about, and this route inherits the rule rather than re-deriving it.
 */
export default function ThemeBuilderRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Theme builder"
        description="The regions the renderer draws, built out of the same blocks a page is built from"
      >
        <ThemeBuilderView />
      </AppShell>
    </RequireAuth>
  );
}
