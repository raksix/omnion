import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DepartmentsView } from "@/features/hr/departments-view";
import { HrModuleNav } from "@/features/hr/module-nav";

export const metadata = { title: "HR · Departments" };

export default function HrDepartmentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="HR"
        description="People operations: the directory, the department tree, leave, attendance and the org chart"
      >
        <div className="space-y-4">
          <HrModuleNav />
          <Suspense fallback={<p className="text-[13px] text-muted">Loading the departments…</p>}>
            <DepartmentsView />
          </Suspense>
        </div>
      </AppShell>
    </RequireAuth>
  );
}