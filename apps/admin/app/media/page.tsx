import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaView } from "@/features/media/media-view";

export const metadata = { title: "Media" };

export default function MediaPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Media"
        description="The files of the selected site, stored in the platform's object store"
      >
        <MediaView />
      </AppShell>
    </RequireAuth>
  );
}
