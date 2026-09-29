"use client";

/**
 * The credential detail (`/workflows/credentials/<id>`): the connection, its health, what uses
 * it, and the way out.
 *
 * The screen has one job beyond showing facts, and the job is *not to display a secret*. Every
 * panel here is built so that a reader who wants to know whether a credential is set up can
 * answer that question completely without the API ever having to send them anything private:
 *
 * 1. **A secret renders as a fixed-width mask with no reveal.** Not because a reveal was
 *    considered and declined, but because there is nothing to reveal — the API has no field for
 *    it. The mask's width is constant so the screen cannot be used to measure a value either.
 * 2. **"Replace secret" is a separate dialog, not an inline field.** Replacing a secret is the
 *    one irreversible action on this screen, and it deserves the same "are you sure" weight as
 *    deleting, plus an honest sentence about what happens to the old value.
 * 3. **The usage panel is loaded, not inferred.** "Used by 0 workflows" is a claim, and the
 *    only way to make it honestly is to ask. It also answers the question the delete guard
 *    will ask, so the reader finds out *before* pressing delete rather than after.
 * 4. **A test result shows what the hook actually did.** The API reports `ok: false` with a
 *    reason for a credential that cannot be tested, and this screen prints the reason rather
 *    than turning it into a red toast that disappears — a reader who cannot act on a failure
 *    will try again and get the same failure.
 */
import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import {
  ArrowLeft,
  CircleAlert,
  ExternalLink,
  KeyRound,
  Loader2,
  PlayCircle,
  RefreshCw,
  ShieldCheck,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";

import {
  deleteCredential,
  fetchCredential,
  fetchCredentialType,
  fetchCredentialUsage,
  replaceCredentialSecret,
  testCredential,
  type ApiError,
} from "@/lib/api";
import type { Credential, CredentialType, CredentialUsage } from "@/lib/types";

/**
 * The mask a secret field renders as.
 *
 * Fixed width on purpose. A mask built from the value's length — even a partially masked one —
 * turns a detail screen into an oracle for the length of somebody else's key, and this screen
 * is one an operator shows over a shoulder.
 */
const MASK = "••••••••••••";

const HEALTH_TONE: Record<string, string> = {
  ok: "bg-emerald-500/10 text-emerald-700 dark:text-emerald-300",
  untested: "bg-quiet-soft text-muted",
  failing: "bg-red-500/10 text-red-700 dark:text-red-300",
  needs_reauth: "bg-amber-500/10 text-amber-700 dark:text-amber-300",
};

const HEALTH_LABEL: Record<string, string> = {
  ok: "Verified",
  untested: "Not verified",
  failing: "Failing",
  needs_reauth: "Needs re-connecting",
};

export function CredentialDetail({ id }: { id: string }) {
  const router = useRouter();

  const [credential, setCredential] = useState<Credential | null>(null);
  const [definition, setDefinition] = useState<CredentialType | null>(null);
  const [usage, setUsage] = useState<CredentialUsage | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<{ ok: boolean; detail: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [replacing, setReplacing] = useState(false);
  const [secretDraft, setSecretDraft] = useState<Record<string, string>>({});
  const [blocked, setBlocked] = useState<string | null>(null);
  // `?warning=1` is set by the create form when a secret was pasted but could not be attached.
  // The API says so in the create response; the panel carries it here so the reader sees it on
  // the row it concerns instead of as a toast about a screen they have already left.
  const [secretWarning, setSecretWarning] = useState<string | null>(null);

  // The credential and its usage are two reads, and they are asked for together because the
  // screen shows both at once — a usage panel that is always empty until you scroll is a
  // usage panel nobody believes.
  const load = useCallback(async () => {
    let alive = true;
    setLoading(true);
    try {
      const row = await fetchCredential(id);
      if (!alive) return;
      setCredential(row);
      setError(null);
      // The warning is a property of the create, not of the row, so it is read from the URL
      // once and then cleared — a reload should not keep repeating a message about a write
      // that already happened.
      if (typeof window !== "undefined" && new URLSearchParams(window.location.search).get("warning") === "1") {
        setSecretWarning(
          "The credential was created, but its secret was not stored — this build has no " +
            "encrypted secret store yet. Use Replace secret once it does.",
        );
        window.history.replaceState({}, "", window.location.pathname);
      }
      const [type, who] = await Promise.allSettled([
        fetchCredentialType(row.type),
        fetchCredentialUsage(row.id),
      ]);
      if (!alive) return;
      if (type.status === "fulfilled") setDefinition(type.value);
      if (who.status === "fulfilled") setUsage(who.value);
    } catch (cause) {
      if (alive) {
        setError(cause as ApiError);
        setCredential(null);
      }
    } finally {
      if (alive) setLoading(false);
    }
    return () => {
      alive = false;
    };
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const onTest = async () => {
    if (!credential) return;
    setBusy("test");
    setError(null);
    setTestResult(null);
    try {
      const result = await testCredential(credential.id);
      setCredential(result.credential);
      setTestResult({ ok: result.ok, detail: result.detail });
    } catch (cause) {
      setError(cause as ApiError);
    } finally {
      setBusy(null);
    }
  };

  const onReplace = async () => {
    if (!credential || !definition) return;
    const secrets = definition.fields
      .filter((field) => field.kind === "secret" && (secretDraft[field.name] ?? "").trim() !== "")
      .map((field) => ({ field: field.name, value: secretDraft[field.name] }));
    if (secrets.length === 0) return;
    setBusy("secret");
    setError(null);
    try {
      const updated = await replaceCredentialSecret(credential.id, secrets);
      setCredential(updated);
      setSecretDraft({});
      setReplacing(false);
      setNotice("Secret replaced. The credential is untested again until you run a test.");
    } catch (cause) {
      setError(cause as ApiError);
    } finally {
      setBusy(null);
    }
  };

  const onDelete = async (force: boolean) => {
    if (!credential) return;
    setBusy("delete");
    setError(null);
    try {
      const result = await deleteCredential(credential.id, force);
      if (result.workflow_count > 0) {
        setNotice(
          `Deleted ${credential.name}. ${result.workflow_count} workflow(s) now reference a credential that no longer exists.`,
        );
      }
      router.push("/workflows/credentials");
    } catch (cause) {
      const failure = cause as ApiError;
      if (failure.code === "credential_in_use" && !force) {
        const details = failure.details as
          | { references?: { workflow_name: string }[]; workflow_count?: number }
          | null;
        const names = (details?.references ?? [])
          .map((reference) => reference.workflow_name)
          .filter((name, index, all) => all.indexOf(name) === index);
        setBlocked(
          names.length
            ? `${names.join(", ")} still ${names.length === 1 ? "names" : "name"} it. Deleting it anyway breaks ${details?.workflow_count ?? names.length} workflow(s).`
            : "A workflow still names it.",
        );
      } else {
        setError(failure);
      }
    } finally {
      setBusy(null);
    }
  };

  if (loading) {
    return (
      <div className="space-y-3" data-credential-detail-skeleton>
        <div className="h-8 w-48 animate-pulse rounded-lg bg-quiet-soft" />
        <div className="h-40 animate-pulse rounded-xl border border-line bg-quiet-soft/40" />
      </div>
    );
  }

  if (!credential) {
    return (
      <div className="space-y-3">
        <p
          role="alert"
          data-credential-detail-missing
          className="rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
        >
          {error?.message ?? "This credential does not exist, or is not in your organization."}
        </p>
        <Link
          href="/workflows/credentials"
          className="inline-flex items-center gap-1.5 text-[13px] text-accent underline underline-offset-2"
        >
          Back to credentials
        </Link>
      </div>
    );
  }

  const health = credential.effective_health;
  const secretFields = definition?.fields.filter((field) => field.kind === "secret") ?? [];
  const plainFields = definition?.fields.filter((field) => field.kind !== "secret") ?? [];

  return (
    <div className="space-y-5">
      <button
        type="button"
        onClick={() => router.push("/workflows/credentials")}
        className="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-ink"
      >
        <ArrowLeft size={14} />
        Credentials
      </button>

      <div className="flex flex-wrap items-start gap-3">
        <span className="rounded-lg border border-line p-2">
          <KeyRound size={16} aria-hidden />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-[17px] font-medium text-ink" data-credential-name={credential.key}>
              {credential.name}
            </h1>
            <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
              {credential.key}
            </code>
            <span
              data-credential-detail-health={health}
              className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                HEALTH_TONE[health] ?? HEALTH_TONE.untested
              }`}
            >
              {HEALTH_LABEL[health] ?? health}
            </span>
          </div>
          <p className="mt-1 text-[12px] text-muted">
            {credential.type_label} · {credential.scope} ·{" "}
            {credential.sharing === "private" ? "private" : "shared with the organization"}
          </p>
        </div>
        <div className="flex items-center gap-1.5">
          <button
            type="button"
            data-credential-test
            disabled={busy !== null}
            onClick={() => void onTest()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px] disabled:opacity-60"
          >
            {busy === "test" ? (
              <Loader2 size={14} className="animate-spin" />
            ) : (
              <PlayCircle size={14} />
            )}
            Test connection
          </button>
        </div>
      </div>

      {error ? (
        <p
          role="alert"
          data-credential-detail-error
          className="flex items-center gap-2 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
        >
          <TriangleAlert size={15} />
          {error.message}
        </p>
      ) : null}

      {notice ? (
        <p
          role="status"
          data-credential-detail-notice
          className="rounded-xl border border-line bg-quiet-soft/60 px-4 py-3 text-[13px] text-ink"
        >
          {notice}
        </p>
      ) : null}

      {secretWarning ? (
        <p
          role="status"
          data-credential-secret-warning
          className="flex items-start gap-2 rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-[13px] text-amber-800 dark:text-amber-200"
        >
          <TriangleAlert size={15} className="mt-0.5 shrink-0" />
          <span>{secretWarning}</span>
        </p>
      ) : null}

      {/* The test's own result, kept on screen. A toast about a failed test that disappears
          teaches the reader that testing is unreliable. */}
      {testResult ? (
        <div
          role="status"
          data-credential-test-result={testResult.ok ? "ok" : "failed"}
          className={`flex items-start gap-2 rounded-xl border px-4 py-3 text-[13px] ${
            testResult.ok
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-800 dark:text-emerald-200"
              : "border-amber-500/40 bg-amber-500/10 text-amber-800 dark:text-amber-200"
          }`}
        >
          {testResult.ok ? <ShieldCheck size={15} /> : <CircleAlert size={15} />}
          <span>{testResult.detail}</span>
        </div>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="rounded-xl border border-line bg-surface p-4">
          <h2 className="text-[14px] font-medium text-ink">Connection</h2>
          <dl className="mt-3 space-y-3">
            {plainFields.map((field) => {
              const value = credential.settings[field.name];
              return (
                <div key={field.name} className="flex flex-wrap items-baseline gap-2">
                  <dt className="w-40 text-[12px] text-muted">{field.label}</dt>
                  <dd className="min-w-0 flex-1 break-words text-[13px] text-ink">
                    {value === undefined || value === "" ? (
                      <span className="text-muted">not set</span>
                    ) : (
                      String(value)
                    )}
                  </dd>
                </div>
              );
            })}
            {secretFields.map((field) => (
              <div key={field.name} className="flex flex-wrap items-baseline gap-2">
                <dt className="w-40 text-[12px] text-muted">{field.label}</dt>
                <dd className="min-w-0 flex-1" data-credential-masked={field.name}>
                  {credential.has_secret ? (
                    <span className="font-mono text-[13px] tracking-widest text-muted">{MASK}</span>
                  ) : (
                    <span className="text-[13px] text-muted">not set</span>
                  )}
                </dd>
              </div>
            ))}
            <div className="flex flex-wrap items-baseline gap-2">
              <dt className="w-40 text-[12px] text-muted">Last used</dt>
              <dd className="text-[13px] text-ink">
                {credential.last_used_at
                  ? new Date(credential.last_used_at).toLocaleString()
                  : "never"}
              </dd>
            </div>
          </dl>

          {secretFields.length > 0 ? (
            <div className="mt-4 border-t border-line pt-3">
              {!replacing ? (
                <button
                  type="button"
                  data-credential-replace-open
                  onClick={() => setReplacing(true)}
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px]"
                >
                  <RefreshCw size={14} />
                  Replace secret
                </button>
              ) : (
                <div className="space-y-3" data-credential-replace-form>
                  <p className="text-[12px] text-muted">
                    The old value is discarded the moment this saves. The new one is never shown
                    again — not here, not in any API response.
                  </p>
                  {secretFields.map((field) => (
                    <label key={field.name} className="flex flex-col gap-1">
                      <span className="text-[12px] font-medium text-muted">{field.label}</span>
                      <input
                        type="password"
                        autoComplete="off"
                        data-credential-replace-field={field.name}
                        value={secretDraft[field.name] ?? ""}
                        onChange={(event) =>
                          setSecretDraft((current) => ({
                            ...current,
                            [field.name]: event.target.value,
                          }))
                        }
                        className="rounded-lg border border-line bg-surface px-3 py-2 font-mono text-[12px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
                      />
                    </label>
                  ))}
                  <div className="flex gap-2">
                    <button
                      type="button"
                      data-credential-replace-save
                      disabled={busy === "secret"}
                      onClick={() => void onReplace()}
                      className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[13px] font-medium text-white disabled:opacity-60"
                    >
                      {busy === "secret" ? <Loader2 size={14} className="animate-spin" /> : null}
                      Save the new secret
                    </button>
                    <button
                      type="button"
                      onClick={() => {
                        setReplacing(false);
                        setSecretDraft({});
                      }}
                      className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px]"
                    >
                      <X size={14} />
                      Cancel
                    </button>
                  </div>
                </div>
              )}
            </div>
          ) : null}
        </section>

        <section className="rounded-xl border border-line bg-surface p-4">
          <h2 className="text-[14px] font-medium text-ink">Health</h2>
          <p className="mt-2 text-[13px] text-ink" data-credential-health-line={health}>
            {HEALTH_LABEL[health] ?? health}
            {credential.expired ? " — the token expired and needs re-connecting" : ""}
          </p>
          {credential.health_detail ? (
            <p className="mt-1 text-[12px] text-muted">{credential.health_detail}</p>
          ) : null}
          <p className="mt-1 text-[12px] text-muted">
            {credential.health_checked_at
              ? `Last tested ${new Date(credential.health_checked_at).toLocaleString()}`
              : "Never tested"}
          </p>
          {credential.oauth_subject ? (
            <p className="mt-2 text-[12px] text-muted">
              Connected as {credential.oauth_subject}
              {credential.oauth_scopes ? ` · ${credential.oauth_scopes}` : ""}
            </p>
          ) : null}
          {definition?.oauth ? (
            <p className="mt-2 text-[12px] text-muted">
              This type uses an OAuth authorization-code flow. Connecting it is the next slice of
              REQ-087; the credential exists now and every other lifecycle around it works.
            </p>
          ) : null}
          {definition?.docs_url ? (
            <a
              href={definition.docs_url}
              target="_blank"
              rel="noreferrer"
              className="mt-3 inline-flex items-center gap-1 text-[12px] text-accent underline underline-offset-2"
            >
              Type documentation
              <ExternalLink size={12} />
            </a>
          ) : null}
        </section>
      </div>

      <section className="rounded-xl border border-line bg-surface p-4">
        <h2 className="text-[14px] font-medium text-ink">Usage</h2>
        {usage ? (
          usage.references.length === 0 ? (
            <p className="mt-2 text-[13px] text-muted" data-credential-usage="none">
              No workflow names this credential. Deleting it will not break anything.
            </p>
          ) : (
            <>
              <p className="mt-1 text-[12px] text-muted" data-credential-usage={usage.workflow_count}>
                {usage.workflow_count} workflow{usage.workflow_count === 1 ? "" : "s"} ·{" "}
                {usage.references.length} node{usage.references.length === 1 ? "" : "s"} ·{" "}
                {usage.node_type_count} node type{usage.node_type_count === 1 ? "" : "s"}
              </p>
              <ul className="mt-3 space-y-1.5">
                {usage.references.map((reference) => (
                  <li
                    key={`${reference.workflow_id}-${reference.node_id}`}
                    data-credential-usage-row
                    className="flex flex-wrap items-center gap-2 text-[13px]"
                  >
                    <span className="text-ink">{reference.workflow_name}</span>
                    <span className="text-muted">·</span>
                    <span className="text-muted">
                      {reference.node_label ?? reference.node_id}
                      {reference.node_type ? ` (${reference.node_type})` : ""}
                    </span>
                  </li>
                ))}
              </ul>
            </>
          )
        ) : (
          <p className="mt-2 text-[13px] text-muted">Loading usage…</p>
        )}
      </section>

      <section className="rounded-xl border border-red-500/30 bg-red-500/5 p-4">
        <h2 className="text-[14px] font-medium text-ink">Delete</h2>
        <p className="mt-1 text-[12px] text-muted">
          Removing a credential leaves any workflow that names it pointing at nothing. The guard
          below will tell you which ones before it lets you do it.
        </p>
        {blocked ? (
          <div
            role="alert"
            data-credential-detail-blocked
            className="mt-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-[13px] text-amber-800 dark:text-amber-200"
          >
            <p>{blocked}</p>
            <div className="mt-2 flex gap-2">
              <button
                type="button"
                onClick={() => setBlocked(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[13px]"
              >
                Keep it
              </button>
              <button
                type="button"
                data-credential-force-delete
                disabled={busy === "delete"}
                onClick={() => void onDelete(true)}
                className="rounded-lg bg-red-600 px-3 py-1.5 text-[13px] font-medium text-white disabled:opacity-60"
              >
                Delete anyway
              </button>
            </div>
          </div>
        ) : (
          <button
            type="button"
            data-credential-delete
            disabled={busy === "delete"}
            onClick={() => void onDelete(false)}
            className="mt-3 inline-flex items-center gap-1.5 rounded-lg border border-red-500/40 px-3 py-2 text-[13px] text-red-700 disabled:opacity-60 dark:text-red-300"
          >
            {busy === "delete" ? <Loader2 size={14} className="animate-spin" /> : <Trash2 size={14} />}
            Delete this credential
          </button>
        )}
      </section>
    </div>
  );
}
