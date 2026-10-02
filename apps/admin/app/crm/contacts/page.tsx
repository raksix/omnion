import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ContactsView } from "@/features/crm/contacts-view";

export const metadata = { title: "CRM · Contacts" };

export default function CrmContactsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="CRM"
        description="The relationship layer: the people, the companies they work for, and the deals in between"
      >
        {/* The list reads its filters out of the URL, so it needs a boundary for the search params. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the contacts…</p>}>
          <ContactsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
