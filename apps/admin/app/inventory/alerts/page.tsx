import { AlertsView } from "@/features/inventory/alerts-view";
import { InventoryModuleNav } from "@/features/inventory/module-nav";

export default function Page() {
  return (
    <div className="space-y-4">
      <InventoryModuleNav />
      <AlertsView />
    </div>
  );
}
