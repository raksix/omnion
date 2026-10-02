import { Suspense } from "react";

import { MyLeaveView, MyWorkspaceNav } from "@/features/hr/my-workspace";

export default function Page() {
  return (
    <div className="space-y-4">
      <MyWorkspaceNav />
      {/*
        The year switch reads the URL through `useSearchParams`, which Next requires a Suspense
        boundary around during static rendering. Without it the whole route fails to build — and
        the build error names the hook rather than this page, so the boundary is worth its two
        lines of comment.
      */}
      <Suspense fallback={<p className="text-sm text-muted" aria-busy="true">Loading…</p>}>
        <MyLeaveView />
      </Suspense>
    </div>
  );
}
