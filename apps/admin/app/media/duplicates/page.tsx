import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaDuplicatesView } from "@/features/media/duplicates-view";

export const metadata = { title: "Media duplicates" };

export default function MediaDuplicatesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Media duplicates"
        description="Files stored more than once, and the choice of which copy your pages keep using"
      >
        <MediaDuplicatesView />
      </AppShell>
    </RequireAuth>
  );
}
