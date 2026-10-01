import { Suspense } from "react";

import { MyAttendanceView, RosterView } from "@/features/hr/attendance-view";
import { MyWorkspaceNav } from "@/features/hr/my-workspace";

export default function Page() {
  return (
    <div className="space-y-4">
      <MyWorkspaceNav />
      <h1 className="text-[17px] font-semibold text-ink">My attendance</h1>
      {/*
        The month switcher reads the URL through `useSearchParams`, which Next requires a Suspense
        boundary around during static rendering. Without it the route fails to build, and the
        build error names the hook rather than this page.
      */}
      <Suspense fallback={<p className="text-[13px] text-muted" aria-busy="true">Loading…</p>}>
        <MyAttendanceView />
      </Suspense>
    </div>
  );
}
