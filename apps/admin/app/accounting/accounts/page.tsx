import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { AccountsView } from "@/features/accounting/accounts-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <AccountsView />
    </div>
  );
}
