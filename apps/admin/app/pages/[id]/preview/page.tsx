import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BlockPreview } from "@/features/blocks/block-preview";

export const metadata = { title: "Preview" };

/** The renderer frame of one page, with inline editing that only ever writes a draft. */
export default function PagePreviewRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Preview"
        description="The page as the renderer sees it — switch screens, edit text in place, save a draft"
      >
        <BlockPreview />
      </AppShell>
    </RequireAuth>
  );
}
