import { HrModuleNav } from "@/features/hr/module-nav";
import { LeaveRequestForm } from "@/features/hr/leave-form-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <HrModuleNav />
      <LeaveRequestForm />
    </div>
  );
}
