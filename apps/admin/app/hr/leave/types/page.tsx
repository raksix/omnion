import { HrModuleNav } from "@/features/hr/module-nav";
import { LeaveTypesView } from "@/features/hr/leave-types-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <HrModuleNav />
      <LeaveTypesView />
    </div>
  );
}
