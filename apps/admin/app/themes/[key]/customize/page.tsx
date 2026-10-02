import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ThemeCustomizeView } from "@/features/themes/theme-customize-view";

export const metadata = { title: "Customize theme" };

/**
 * `/themes/<key>/customize` — the theme settings editor (REQ-062, slice 2).
 *
 * The `key` in the URL is the theme the operator *intends* to customise, but the screen reads
 * the site's own active theme from the API and states it: a settings row is validated against
 * the site's active theme on the server, so a deep link to a theme the site is not on would
 * otherwise render an editor whose every save is rejected. The page does not check the two —
 * the panel shows the theme key it actually loaded (`data-theme-customize-theme-key`) and the
 * gallery is the way in.
 */
export default function ThemeCustomizeRoute({
  params,
}: {
  params: Promise<{ key: string }>;
}) {
  return (
    <RequireAuth>
      <AppShell
        title="Customize theme"
        description="Colours, type, layout and branding for this site. Saving writes a draft; publishing is what visitors see"
      >
        <ThemeCustomizeView />
      </AppShell>
    </RequireAuth>
  );
}
