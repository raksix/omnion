import { InventoryModuleNav } from "@/features/inventory/module-nav";
import { ReportsView } from "@/features/inventory/reports-view";

export const metadata = {
  title: "Inventory reports",
};

export default function InventoryReportsPage() {
  return (
    <div className="mx-auto w-full max-w-6xl space-y-6 px-4 py-6">
      <InventoryModuleNav />
      <ReportsView />
    </div>
  );
}
