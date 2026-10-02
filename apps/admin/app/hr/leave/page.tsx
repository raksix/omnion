import { HrModuleNav } from "@/features/hr/module-nav";
import { LeaveView } from "@/features/hr/leave-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <HrModuleNav />
      <LeaveView />
    </div>
  );
}
