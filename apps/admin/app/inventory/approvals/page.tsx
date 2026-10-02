import { ApprovalsView } from "@/features/inventory/approvals-view";

import { InventoryModuleNav } from "@/features/inventory/module-nav";

export default function Page() {
  return (
    <div className="space-y-4">
      <InventoryModuleNav />
      <ApprovalsView />
    </div>
  );
}
