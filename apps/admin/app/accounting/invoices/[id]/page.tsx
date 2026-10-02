import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { InvoiceDetail } from "@/features/accounting/invoice-detail";

export const metadata = { title: "Invoice" };

export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <InvoiceDetail id={id} />
    </div>
  );
}
