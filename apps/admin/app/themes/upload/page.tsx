import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ThemeUploadView } from "@/features/themes/theme-upload-view";

export const metadata = { title: "Upload a theme" };

/**
 * `/themes/upload` — package validation, install and removal (REQ-062, slice 3).
 *
 * A top-level route rather than `/themes/<key>/upload`, because the whole point is that an
 * uploaded theme has no key yet: the key is *inside* the file, and naming the route after it
 * would ask the operator to already know the answer the screen is about to give them.
 */
export default function ThemeUploadRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Upload a theme"
        description="Validate a package, read every problem it has, and install it inactive"
      >
        <ThemeUploadView />
      </AppShell>
    </RequireAuth>
  );
}
