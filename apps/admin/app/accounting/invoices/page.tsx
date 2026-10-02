import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { InvoicesView } from "@/features/accounting/invoices-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <InvoicesView />
    </div>
  );
}
