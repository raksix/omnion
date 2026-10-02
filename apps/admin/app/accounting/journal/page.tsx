import { AccountingModuleNav } from "@/features/accounting/module-nav";
import { JournalView } from "@/features/accounting/journal-view";

export default function Page() {
  return (
    <div className="space-y-4">
      <AccountingModuleNav />
      <JournalView />
    </div>
  );
}
