"use client";

/**
 * Session state for the admin panel.
 *
 * The API owns the session — an HttpOnly cookie the browser never reads — and the root layout
 * resolves it on the server, so this provider starts from a known answer instead of probing.
 * Signing in and out keep that answer in step with the cookie.
 */
import { createContext, useCallback, useContext, useMemo, useState } from "react";

import {
  beginPasskeySignIn,
  completePasskeySignIn,
  login as loginRequest,
  logout as logoutRequest,
  verifyMfaChallenge,
} from "./api";
import { ceremonyMessage, getPasskeyAssertion } from "./webauthn";
import type { User } from "./types";

/** Where the panel stands: signed in with an account, or not signed in. */
export type SessionStatus = "signed-in" | "signed-out";

/** What a password check did: start a session, or park the sign-in behind its second factor. */
export type SignInResult =
  | { status: "signed-in"; user: User }
  | { status: "mfa-required"; challenge: string };

type SessionValue = {
  /** Whether the panel has an account to work with. */
  status: SessionStatus;
  /** The signed-in account, when there is one. */
  user: User | null;
  /** Sign in with email and password; an account with a confirmed factor answers a challenge. */
  signIn: (email: string, password: string) => Promise<SignInResult>;
  /** Finish a parked sign-in with a code from an enrolled factor (TOTP or recovery). */
  completeMfa: (challenge: string, code: string) => Promise<User>;
  /** Finish a parked sign-in with a passkey — the ceremony runs in this browser. */
  completePasskey: (challenge: string) => Promise<User>;
  /** Adopt an account the API already signed in (the first-run wizard creates one). */
  adoptUser: (user: User) => void;
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
    const outcome = await loginRequest(email, password);
    if (outcome.status === "mfa-required") {
      return { status: "mfa-required" as const, challenge: outcome.challenge };
    }
    setUser(outcome.user);
    return { status: "signed-in" as const, user: outcome.user };
  }, []);

  const completeMfa = useCallback(async (challenge: string, code: string) => {
    const body = await verifyMfaChallenge(challenge, code);
    setUser(body.user);
    return body.user;
  }, []);

  const completePasskey = useCallback(async (challenge: string) => {
    // The ceremony runs here; the API verifies it and answers the session cookie with the
    // account, exactly like a password sign-in does.
    const options = await beginPasskeySignIn(challenge);
    let credential;
    try {
      credential = await getPasskeyAssertion(options);
    } catch (cause: unknown) {
      throw new Error(ceremonyMessage(cause));
    }
    const body = await completePasskeySignIn({
      challenge,
      ceremonyChallenge: options.challenge,
      credential,
    });
    setUser(body.user);
    return body.user;
  }, []);

  const adoptUser = useCallback((me: User) => {
    setUser(me);
  }, []);

  const signOut = useCallback(async () => {
    // A session that is already gone is still a signed-out panel.
    await logoutRequest().catch(() => undefined);
    setUser(null);
  }, []);

  const value: SessionValue = useMemo(
    () => ({
      status: user ? "signed-in" : "signed-out",
      user,
      signIn,
      completeMfa,
      completePasskey,
      adoptUser,
      signOut,
    }),
    [user, signIn, completeMfa, completePasskey, adoptUser, signOut],
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
