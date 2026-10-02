import { Suspense } from "react";

import { TransfersView } from "@/features/inventory/transfers-view";
import { InventoryModuleNav } from "@/features/inventory/module-nav";

export default function Page() {
  // The view reads the list out of the query string, which is a `useSearchParams` hook and
  // therefore needs a boundary when the page is rendered on a server that has no request.
  return (
    <Suspense fallback={null}>
      <div className="space-y-4">
        <InventoryModuleNav />
        <TransfersView />
      </div>
    </Suspense>
  );
}
