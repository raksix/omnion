import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { PagesView } from "@/features/pages/pages-view";

export const metadata = { title: "Pages" };

export default function PagesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Pages"
        description="The content of the selected site, with its draft and live revisions"
      >
        {/* The view reads `?focus=` (a search hit opens that page), which needs a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the pages…</p>}>
          <PagesView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
