import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { PaymentDetail } from "@/features/accounting/payment-detail";

export const metadata = { title: "Payment" };

export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <PaymentDetail id={id} />
    </div>
  );
}
