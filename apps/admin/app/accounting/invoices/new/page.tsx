import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { InvoiceForm } from "@/features/accounting/invoice-form";

export const metadata = { title: "New invoice" };

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <InvoiceForm />
    </div>
  );
}
