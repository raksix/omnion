import { Suspense } from "react";

import { InviteView } from "@/features/invitations/invite-view";

export const metadata = { title: "Invitation" };

/**
 * `/invite/[token]` (REQ-005, slice 1): the public invitation page. It sits outside the panel
 * frame on purpose — the recipient is not signed in yet, and a header with a site switcher
 * would be a lie about what they have access to.
 */
export default async function InvitePage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  return (
    <Suspense>
      <InviteView token={token} />
    </Suspense>
  );
}
