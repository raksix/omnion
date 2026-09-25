"use client";

/**
 * Session state for the admin panel.
 *
 * The API owns the session — an HttpOnly cookie the browser never reads — and the root layout
 * resolves it on the server, so this provider starts from a known answer instead of probing.
 * Signing in and out keep that answer in step with the cookie.
 */
import { createContext, useCallback, useContext, useMemo, useState } from "react";

import { login as loginRequest, logout as logoutRequest } from "./api";
import type { User } from "./types";

/** Where the panel stands: signed in with an account, or not signed in. */
export type SessionStatus = "signed-in" | "signed-out";

type SessionValue = {
  /** Whether the panel has an account to work with. */
  status: SessionStatus;
  /** The signed-in account, when there is one. */
  user: User | null;
  /** Sign in and adopt the account the API answered with. */
  signIn: (email: string, password: string) => Promise<void>;
  /** End the session. */
  signOut: () => Promise<void>;
};

const SessionContext = createContext<SessionValue | null>(null);

/** Provide the session to the whole panel. */
export function SessionProvider({
  initialUser,
  children,
}: {
  /** The account the API resolved for this request, from the root layout. */
  initialUser: User | null;
  children: React.ReactNode;
}) {
  const [user, setUser] = useState<User | null>(initialUser);

  const signIn = useCallback(async (email: string, password: string) => {
    const me = await loginRequest(email, password);
    setUser(me);
  }, []);

  const signOut = useCallback(async () => {
    // A session that is already gone is still a signed-out panel.
    await logoutRequest().catch(() => undefined);
    setUser(null);
  }, []);

  const value: SessionValue = useMemo(
    () => ({ status: user ? "signed-in" : "signed-out", user, signIn, signOut }),
    [user, signIn, signOut],
  );

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

/** Read the session; only valid inside [`SessionProvider`]. */
export function useSession(): SessionValue {
  const value = useContext(SessionContext);
  if (!value) {
    throw new Error("useSession must be used inside <SessionProvider>");
  }
  return value;
}
