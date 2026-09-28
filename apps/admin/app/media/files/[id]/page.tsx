import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MediaFileDetail } from "@/features/media/file-detail";

export const metadata = { title: "File detail" };

export default function MediaFilePage() {
  return (
    <RequireAuth>
      <AppShell
        title="File detail"
        description="The preview, the metadata and the version history of one file"
      >
        {/* The route reads `params.id`; the detail view is a client component because the preview
            zooms, the history is fetched per file and the replace control is a file input. */}
        <MediaFileDetail />
      </AppShell>
    </RequireAuth>
  );
}
