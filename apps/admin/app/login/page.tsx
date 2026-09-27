"use client";

/**
 * Sign-in screen: the only screen that talks to the API without a session.
 *
 * Two steps live here because a sign-in has two: the password, and — when the account holds a
 * confirmed second factor — the factor that finishes it. The second step takes a code from an
 * authenticator app, a recovery code, or a **passkey** (`navigator.credentials.get` runs in
 * this browser; the API verifies the assertion and answers the session cookie).
 */
import { useEffect, useState, type FormEvent } from "react";

import { Fingerprint, KeyRound, ShieldCheck } from "lucide-react";
import { useRouter } from "next/navigation";

import { ApiError, fetchOnboarding } from "@/lib/api";
import { passkeysSupported } from "@/lib/webauthn";
import { useSession } from "@/lib/session";

const inputClass =
  "w-full rounded-lg border border-line bg-surface px-3 py-2 text-[13px] text-ink outline-none transition placeholder:text-muted/70 focus:border-accent focus:ring-2 focus:ring-accent/15";

export default function LoginPage() {
  const router = useRouter();
  const { status, signIn, completeMfa, completePasskey } = useSession();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // The second step: present when the password check answered a challenge.
  const [challenge, setChallenge] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [factorBusy, setFactorBusy] = useState(false);
  const [passkeyBusy, setPasskeyBusy] = useState(false);

  useEffect(() => {
    if (status === "signed-in") {
      router.replace("/");
    }
  }, [status, router]);

  // A fresh installation has no account to sign in with: the wizard is the way in.
  useEffect(() => {
    let cancelled = false;
    fetchOnboarding()
      .then((onboarding) => {
        if (!cancelled && onboarding.needs_setup) {
          router.replace("/setup");
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [router]);

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const outcome = await signIn(email.trim(), password);
      if (outcome.status === "mfa-required") {
        setChallenge(outcome.challenge);
        setSubmitting(false);
        return;
      }
      router.replace("/");
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError ? cause.message : "Sign in failed. Please try again.",
      );
      setSubmitting(false);
    }
  };

  const handleCode = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!challenge) return;
    setFactorBusy(true);
    setError(null);
    try {
      await completeMfa(challenge, code.trim());
      router.replace("/");
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "That code could not be verified. Please try again.",
      );
      setFactorBusy(false);
    }
  };

  const handlePasskey = async () => {
    if (!challenge) return;
    setPasskeyBusy(true);
    setError(null);
    try {
      await completePasskey(challenge);
      router.replace("/");
    } catch (cause: unknown) {
      setError(cause instanceof Error ? cause.message : "The passkey sign-in failed.");
      setPasskeyBusy(false);
    }
  };

  return (
    <main className="flex min-h-screen items-center justify-center px-4 py-10">
      <div className="w-full max-w-sm">
        <div className="mb-6 flex items-center gap-2.5">
          <span
            aria-hidden
            className="flex size-9 items-center justify-center rounded-xl bg-accent text-sm font-semibold text-white"
          >
            O
          </span>
          <span className="flex flex-col leading-tight">
            <span className="text-[15px] font-semibold">Omnion</span>
            <span className="text-[11px] text-muted">Admin panel</span>
          </span>
        </div>

        <div className="rounded-2xl border border-line bg-surface p-6">
          {challenge === null ? (
            <>
              <h1 className="text-[17px] font-semibold">Sign in</h1>
              <p className="mt-1 text-[12.5px] text-muted">
                Use the account this installation was set up with.
              </p>

              <form onSubmit={handleSubmit} className="mt-5 flex flex-col gap-4" noValidate>
                <div className="flex flex-col gap-1.5">
                  <label htmlFor="email" className="text-[12px] font-medium">
                    Email
                  </label>
                  <input
                    id="email"
                    name="email"
                    type="email"
                    autoComplete="username"
                    required
                    value={email}
                    onChange={(event) => setEmail(event.target.value)}
                    placeholder="admin@example.com"
                    className={inputClass}
                  />
                </div>

                <div className="flex flex-col gap-1.5">
                  <label htmlFor="password" className="text-[12px] font-medium">
                    Password
                  </label>
                  <input
                    id="password"
                    name="password"
                    type="password"
                    autoComplete="current-password"
                    required
                    value={password}
                    onChange={(event) => setPassword(event.target.value)}
                    placeholder="••••••••"
                    className={inputClass}
                  />
                </div>

                {error ? (
                  <p
                    role="alert"
                    className="rounded-lg border border-accent/30 bg-accent-soft px-3 py-2 text-[12px] text-accent-strong"
                  >
                    {error}
                  </p>
                ) : null}

                <button
                  type="submit"
                  disabled={submitting}
                  className="mt-1 rounded-lg bg-accent px-3.5 py-2.5 text-[13px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-quiet-soft disabled:text-muted"
                >
                  {submitting ? "Signing in…" : "Sign in"}
                </button>
              </form>
            </>
          ) : (
            <form onSubmit={handleCode} className="flex flex-col gap-4" noValidate>
              <div>
                <h1 className="flex items-center gap-2 text-[17px] font-semibold">
                  <ShieldCheck className="size-4 text-accent" aria-hidden />
                  Second factor
                </h1>
                <p className="mt-1 text-[12.5px] text-muted" data-login-mfa-hint>
                  The password matched. Prove the second factor to finish signing in.
                </p>
              </div>

              <div className="flex flex-col gap-1.5">
                <label htmlFor="factor-code" className="text-[12px] font-medium">
                  Code from your authenticator app (or a recovery code)
                </label>
                <input
                  id="factor-code"
                  name="code"
                  autoComplete="one-time-code"
                  inputMode="numeric"
                  value={code}
                  data-login-mfa-code
                  onChange={(event) => setCode(event.target.value)}
                  placeholder="123456"
                  className={inputClass}
                />
              </div>

              {error ? (
                <p
                  role="alert"
                  className="rounded-lg border border-accent/30 bg-accent-soft px-3 py-2 text-[12px] text-accent-strong"
                >
                  {error}
                </p>
              ) : null}

              <button
                type="submit"
                data-login-mfa-submit
                disabled={factorBusy || passkeyBusy || code.trim().length === 0}
                className="flex items-center justify-center gap-1.5 rounded-lg bg-accent px-3.5 py-2.5 text-[13px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-quiet-soft disabled:text-muted"
              >
                <KeyRound className="size-3.5" aria-hidden />
                {factorBusy ? "Verifying…" : "Verify code"}
              </button>

              {passkeysSupported() ? (
                <button
                  type="button"
                  data-login-passkey
                  disabled={factorBusy || passkeyBusy}
                  onClick={() => void handlePasskey()}
                  className="flex items-center justify-center gap-1.5 rounded-lg border border-line px-3.5 py-2.5 text-[13px] font-medium text-ink transition hover:bg-panel disabled:opacity-50"
                >
                  <Fingerprint className="size-3.5" aria-hidden />
                  {passkeyBusy ? "Waiting for the passkey…" : "Use a passkey"}
                </button>
              ) : null}

              <button
                type="button"
                data-login-mfa-back
                disabled={factorBusy || passkeyBusy}
                onClick={() => {
                  setChallenge(null);
                  setCode("");
                  setError(null);
                }}
                className="text-[11.5px] text-muted transition hover:text-ink"
              >
                Start over
              </button>
            </form>
          )}
        </div>

        <p className="mt-4 text-center text-[11.5px] text-muted">
          Omnion — an open-source enterprise application platform.
        </p>
      </div>
    </main>
  );
}
