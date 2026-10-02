"use client";

/**
 * `/invite/[token]` — the public invitation page (REQ-005, slice 1).
 *
 * It shows what the invitation is before anybody signs in: the organization, who sent it, the
 * role it offers and when the link runs out. Accepting works two ways — a signed-in account
 * accepts as itself, a signed-out one creates the account the invitation was addressed to, with
 * the address locked so a link cannot sign somebody else up.
 *
 * Every dead end gets its own sentence: expired, revoked, already used and unknown all read
 * differently to the person holding the link, even though the API deliberately answers the last
 * three the same way.
 */
import { useCallback, useEffect, useState } from "react";

import { Building2, CheckCircle2, Loader2 } from "lucide-react";
import { useRouter } from "next/navigation";

import { LoadingTable } from "@/components/loading-table";
import { ApiError, acceptInvitation, fetchInvitationPreview } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSession } from "@/lib/session";

/** Shortest password the sign-up form accepts. */
const MIN_PASSWORD_LENGTH = 12;

/** A plain strength hint: length is what actually matters, so it says so. */
function strengthOf(password: string): { label: string; ok: boolean } {
  if (password.length === 0) return { label: "", ok: false };
  if (password.length < MIN_PASSWORD_LENGTH) {
    return {
      label: `Too short — ${MIN_PASSWORD_LENGTH - password.length} more character${
        MIN_PASSWORD_LENGTH - password.length === 1 ? "" : "s"
      } needed`,
      ok: false,
    };
  }
  if (password.length < 16) return { label: "Acceptable — longer is harder to guess", ok: true };
  return { label: "Strong", ok: true };
}

/** `/invite/[token]`. */
export function InviteView({ token }: { token: string }) {
  const router = useRouter();
  const { status: sessionStatus, signOut } = useSession();
  const [preview, setPreview] = useState<{
    organization_name: string | null;
    organization_slug: string | null;
    invited_by_name: string | null;
    role_name: string | null;
    email_masked: string | null;
    expires_at: string | null;
    usable: boolean;
    reason: string | null;
  } | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [done, setDone] = useState<{ organization_id: string; name: string } | null>(null);

  const load = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      setPreview(await fetchInvitationPreview(token));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      if (cause instanceof ApiError) {
        setError(
          cause.code === "invitation_expired"
            ? "This invitation has expired. Ask whoever invited you for a new link."
            : cause.message,
        );
      } else {
        setError("This invitation link could not be read.");
      }
    }
  }, [token]);

  useEffect(() => {
    void load();
  }, [load]);

  const accept = async () => {
    setFieldError(null);
    const strength = strengthOf(password);
    if (!strength.ok && sessionStatus !== "signed-in") {
      setFieldError(
        `The password needs at least ${MIN_PASSWORD_LENGTH} characters — ${
          MIN_PASSWORD_LENGTH - password.length
        } more to go.`,
      );
      return;
    }
    setBusy(true);
    try {
      if (sessionStatus === "signed-in") {
        const body = await acceptInvitation(token, {});
        setDone({ organization_id: body.organization_id, name: body.organization_name });
        return;
      }
      // Sign up and accept in one step: the account is created against the invited address (the
      // API locks it, the panel only sends a name and a password) and the acceptance answers
      // with the session cookie, so the reader lands on the organization already signed in.
      const body = await acceptInvitation(token, {
        display_name: displayName.trim() || undefined,
        password,
      });
      setDone({ organization_id: body.organization_id, name: body.organization_name });
    } catch (cause) {
      setFieldError(
        cause instanceof ApiError ? cause.message : "The invitation could not be accepted.",
      );
    } finally {
      setBusy(false);
    }
  };

  if (status === "loading") {
    return (
      <div className="mx-auto max-w-md">
        <LoadingTable columns={2} rows={3} />
      </div>
    );
  }

  if (status === "error" || !preview) {
    return (
      <main className="flex min-h-screen items-center justify-center px-4">
        <div className="flex max-w-md flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <h1 className="text-[15px] font-semibold">This invitation cannot be used</h1>
          <p className="text-[13px] text-muted">{error}</p>
          <button
            type="button"
            onClick={() => router.replace("/login")}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Go to sign in
          </button>
        </div>
      </main>
    );
  }

  if (done) {
    return (
      <main className="flex min-h-screen items-center justify-center px-4">
        <div
          role="status"
          className="flex max-w-md flex-col items-center gap-3 rounded-xl border border-positive/40 bg-positive-soft px-6 py-10 text-center"
        >
          <CheckCircle2 className="size-6 text-positive" aria-hidden />
          <h1 className="text-[15px] font-semibold">Welcome to {done.name}</h1>
          <p className="text-[13px] text-muted">
            You are a member now. Everything in the panel you are about to see belongs to this
            organization.
          </p>
          <button
            type="button"
            onClick={() => router.replace(`/organizations/${done.organization_id}`)}
            className="rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white"
          >
            Open the organization
          </button>
        </div>
      </main>
    );
  }

  // A token that was never issued is not an error any more: the preview answers `200` with
  // `usable: false` and no organization, so the public endpoint cannot be walked to find out
  // which organizations exist. The screen then has to say the same thing it says for a revoked
  // link, and it must not print "invited to null" on the way — hence the separate branch rather
  // than a fallback name in the template.
  const isKnownInvitation = preview.usable || preview.organization_name !== null;

  if (!isKnownInvitation) {
    return (
      <main className="flex min-h-screen items-center justify-center px-4">
        <div className="flex max-w-md flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
          <h1 className="text-[15px] font-semibold">This invitation link is not valid</h1>
          <p className="text-[13px] text-muted">
            It has already been used, revoked or replaced — ask whoever invited you for a new
            link.
          </p>
          <button
            type="button"
            onClick={() => router.replace("/login")}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Go to sign in
          </button>
        </div>
      </main>
    );
  }

  return (
    <main className="flex min-h-screen items-center justify-center px-4 py-10">
      <div className="flex w-full max-w-md flex-col gap-4 rounded-xl border border-line bg-surface p-6">
        <div className="flex flex-col items-center gap-2 text-center">
          <span className="flex size-10 items-center justify-center rounded-full bg-accent-soft">
            <Building2 className="size-5 text-accent-strong" aria-hidden />
          </span>
          <h1 className="text-[16px] font-semibold">You have been invited to {preview.organization_name}</h1>
          <p className="text-[12.5px] text-muted">
            {preview.invited_by_name
              ? `${preview.invited_by_name} invited ${preview.email_masked}.`
              : `The invitation was sent to ${preview.email_masked}.`}
            {preview.role_name ? ` It carries the ${preview.role_name} role.` : ""}
          </p>
        </div>

        {preview.usable ? (
          <>
            {sessionStatus === "signed-in" ? (
              <p className="rounded-lg border border-line bg-canvas px-3 py-2.5 text-[12.5px] text-muted">
                You are signed in. Accepting joins this organization to your account.
              </p>
            ) : (
              <div className="flex flex-col gap-3">
                <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
                  Your name
                  <input
                    value={displayName}
                    onChange={(event) => setDisplayName(event.target.value)}
                    placeholder="Ada Lovelace"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                </label>
                <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
                  E-mail address
                  <input
                    value={preview.email_masked ?? ""}
                    readOnly
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal text-muted outline-none"
                  />
                  <span className="text-[11.5px] font-normal text-muted">
                    The invitation was addressed to this account; ask for a new one to use another
                    address.
                  </span>
                </label>
                <label className="flex flex-col gap-1.5 text-[12.5px] font-medium">
                  Password
                  <input
                    type="password"
                    value={password}
                    onChange={(event) => setPassword(event.target.value)}
                    autoComplete="new-password"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] font-normal outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
                  />
                  {password ? (
                    <span
                      className={`text-[11.5px] font-normal ${
                        strengthOf(password).ok ? "text-positive" : "text-muted"
                      }`}
                    >
                      {strengthOf(password).label}
                    </span>
                  ) : (
                    <span className="text-[11.5px] font-normal text-muted">
                      At least {MIN_PASSWORD_LENGTH} characters.
                    </span>
                  )}
                </label>
              </div>
            )}

            {fieldError ? (
              <p role="alert" className="text-[12.5px] text-accent-strong">
                {fieldError}
              </p>
            ) : null}

            <button
              type="button"
              data-qa-guard="write"
              onClick={() => void accept()}
              disabled={busy || !preview.usable}
              className="flex items-center justify-center gap-2 rounded-lg bg-accent px-3.5 py-2 text-[13px] font-medium text-white transition disabled:opacity-60"
            >
              {busy ? (
                <>
                  <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  Joining…
                </>
              ) : (
                `Accept and join ${preview.organization_name}`
              )}
            </button>
            <p className="text-center text-[11.5px] text-muted">
              The link works once and expires {formatTimestamp(preview.expires_at ?? "")}.
            </p>
          </>
        ) : (
          <div className="flex flex-col items-center gap-3 rounded-lg border border-caution/40 bg-caution-soft px-4 py-6 text-center">
            <p className="text-[13px] text-caution">
              This invitation has already been used, revoked or expired
              {preview.expires_at ? ` — it ran out ${formatTimestamp(preview.expires_at)}` : ""}.
            </p>
            <button
              type="button"
              onClick={() => router.replace("/login")}
              className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Sign in instead
            </button>
          </div>
        )}

        {sessionStatus === "signed-in" ? (
          <p className="text-center text-[11.5px] text-muted">
            <button
              type="button"
              onClick={() => void signOut().then(() => router.refresh())}
              className="underline underline-offset-2"
            >
              Not you?
            </button>{" "}
            Sign out and accept as the invited address instead.
          </p>
        ) : null}
      </div>
    </main>
  );
}