import { Suspense } from "react";

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
        {/* The view reads `?focus=` (a search hit marks its file), which needs a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the library…</p>}>
          <MediaView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
