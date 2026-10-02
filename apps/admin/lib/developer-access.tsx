"use client";

/**
 * Whether the signed-in account may see the Developer group at all (REQ-022, slice 2).
 *
 * ## Why the navigation asks, rather than showing everything and letting the API refuse
 *
 * The REQ's acceptance criterion is explicit: *"a user without `developer.*` sees no Developer
 * nav group and gets `403` from the endpoints."* Those are two halves of one claim, and the
 * second half is already walked — a member holding none of the six keys is refused `403` on all
 * eight routes. This file is the first half.
 *
 * A nav entry that every account can see and that answers `403` on click is not a permission, it
 * is a trap: the reader learns the group exists, learns they cannot use it, and has no way to
 * tell whether the platform is broken or they are simply not allowed. Hiding it is the honest
 * answer, and it is also what the command palette and search already do — a screen the palette
 * will not suggest is a screen that should not be in the sidebar either.
 *
 * ## It asks one route, and treats failure as "no"
 *
 * The answer comes from `GET /developer/scopes`, because that is the *read* a key author needs
 * before they have a key, and because it is the one developer route that answers the question
 * "may this account be here at all" without also answering "what does it hold". A failure —
 * including a `403` — means the group is hidden. A navigation group is not the place to surface
 * an error: if the answer cannot be established, the conservative reading is correct, and the
 * user who *can* see it will see it on the next load.
 */
import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import { fetchDeveloperScopes } from "@/lib/api";

type AccessValue = {
  /** `null` while the answer is still in flight, so the nav does not flicker. */
  canOpen: boolean | null;
  /** Re-ask, for after a role change. */
  reload: () => void;
};

const AccessContext = createContext<AccessValue>({
  canOpen: null,
  reload: () => undefined,
});

/** Whether the signed-in account may see the Developer navigation group. */
export function useDeveloperAccess(): AccessValue {
  return useContext(AccessContext);
}

/** Provide the answer once, for the whole panel. */
export function DeveloperAccessProvider({ children }: { children: ReactNode }) {
  const [canOpen, setCanOpen] = useState<boolean | null>(null);
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setCanOpen(null);
    fetchDeveloperScopes()
      .then((catalogue) => {
        // A catalogue with no categories is not an empty portal, it is a broken answer, and it
        // is treated the same as a refusal: hidden.
        if (!cancelled) setCanOpen(catalogue.categories.length > 0);
      })
      .catch(() => {
        if (!cancelled) setCanOpen(false);
      });
    return () => {
      cancelled = true;
    };
  }, [nonce]);

  const value = useMemo<AccessValue>(
    () => ({ canOpen, reload: () => setNonce((current) => current + 1) }),
    [canOpen],
  );

  return <AccessContext.Provider value={value}>{children}</AccessContext.Provider>;
}
