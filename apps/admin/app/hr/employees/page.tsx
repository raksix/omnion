import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EmployeeDirectoryView } from "@/features/hr/employee-directory-view";
import { HrModuleNav } from "@/features/hr/module-nav";

export const metadata = { title: "HR · Employees" };

export default function HrEmployeesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="HR"
        description="People operations: the directory, the department tree, leave, attendance and the org chart"
      >
        <div className="space-y-4">
          <HrModuleNav />
          {/* The directory reads its filters out of the URL, so it needs a search-params boundary. */}
          <Suspense fallback={<p className="text-[13px] text-muted">Loading the directory…</p>}>
            <EmployeeDirectoryView />
          </Suspense>
        </div>
      </AppShell>
    </RequireAuth>
  );
}