import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { PaymentsView } from "@/features/accounting/payments-view";

export const metadata = { title: "Payments" };

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <PaymentsView />
    </div>
  );
}
