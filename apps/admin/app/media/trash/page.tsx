import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaTrashView } from "@/features/media/trash-view";

export const metadata = { title: "Media trash" };

export default function MediaTrashPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Media trash"
        description="Files that were deleted from the library, and how long they are kept"
      >
        <MediaTrashView />
      </AppShell>
    </RequireAuth>
  );
}
