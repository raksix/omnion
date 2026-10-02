import { Suspense } from "react";

import { RosterView } from "@/features/hr/attendance-view";
import { HrModuleNav } from "@/features/hr/module-nav";

export default function Page() {
  return (
    <div className="space-y-4">
      <HrModuleNav />
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h1 className="text-[17px] font-semibold text-ink">Attendance roster</h1>
        <p className="text-[12.5px] text-muted">
          Who is in, who is out and who is away on one day.
        </p>
      </div>
      <Suspense fallback={<p className="text-[13px] text-muted" aria-busy="true">Loading…</p>}>
        <RosterView />
      </Suspense>
    </div>
  );
}
