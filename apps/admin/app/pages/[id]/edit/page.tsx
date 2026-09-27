import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BlockEditor } from "@/features/blocks/block-editor";

export const metadata = { title: "Page editor" };

export default function PageEditorRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Page editor"
        description="Build the page out of blocks; the canvas, the outline and the inspector are the same tree"
      >
        <BlockEditor />
      </AppShell>
    </RequireAuth>
  );
}
