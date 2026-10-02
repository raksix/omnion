import { StockListView } from "@/features/inventory/stock-list-view";

import { InventoryModuleNav } from "@/features/inventory/module-nav";

export default function Page() {
  return (
    <div className="space-y-4">
      <InventoryModuleNav />
      <StockListView />
    </div>
  );
}
