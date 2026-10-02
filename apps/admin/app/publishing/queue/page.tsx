import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PublishingQueueView } from "@/features/menus/publishing-queue";

export const metadata = { title: "Publishing queue" };

/**
 * `/publishing/queue` — every scheduled publish and unpublish, with what each one did.
 *
 * Carries `content.pages.schedule`, the same key the page editor's scheduling popover writes
 * with, so the queue shows the editor the promises their own screens made.
 */
export default function PublishingQueuePage() {
  return (
    <RequireAuth>
      <AppShell
        title="Publishing queue"
        description="Every promise about when a page appears — and what the worker did about it"
      >
        <PublishingQueueView />
      </AppShell>
    </RequireAuth>
  );
}
