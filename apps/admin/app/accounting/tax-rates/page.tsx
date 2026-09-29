import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { TaxRatesView } from "@/features/accounting/tax-rates-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <TaxRatesView />
    </div>
  );
}
