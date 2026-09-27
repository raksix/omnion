"use client";

/**
 * `/secrets/credentials` — the typed credential list and the create wizard
 * (docs/requests/REQ-125, slice 2).
 *
 * The screen's one rule: **a value never appears here.** A credential row is a name, a kind, the
 * non-secret fields the operator recorded, and a chip that says whether the validator still
 * believes the pair works. The value is sealed; the wizard takes it through the API and never
 * puts it in this component's state after the request resolves.
 *
 * The second rule the screen has to make visible: **a failing validation is not a failed save.**
 * A credential whose provider is unreachable is stored with a red chip carrying the provider's
 * own sentence, and the button to fix it is right there. Refusing the save would lose the
 * operator's work for the sake of a check that is advisory by design.
 *
 * Keyboard: `/` focuses the search box, `n` opens the wizard, `Esc` closes it. Mobile turns the
 * table into cards through the shared `sm:` rules.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  CircleHelp,
  KeyRound,
  Plus,
  RefreshCw,
  Search,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  attachCredentialProfile,
  fetchCredentials,
  validateCredential,
  type Credential,
  type CredentialKindOption,
  type CredentialsResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** Which kind filter is on. `all` is the default, so nothing is hidden on arrival. */
type Filter = "all" | "valid" | "invalid" | "unknown";

/** The fields a kind's wizard asks for, given its label text. */
const FIELD_LABELS: Record<string, { label: string; placeholder: string; hint?: string }> = {
  endpoint: { label: "Endpoint", placeholder: "https://api.provider.com" },
  username: { label: "Username", placeholder: "service account or mailbox" },
  key_prefix: {
    label: "Key prefix",
    placeholder: "sk_live_ or sk-live-",
    hint: "The first few characters of the key — enough for the validator to recognise its shape, never the key itself.",
  },
  host: { label: "Mail host", placeholder: "smtp.provider.com" },
  port: { label: "Port", placeholder: "587" },
  tls: { label: "Transport security", placeholder: "starttls, tls or none" },
  expires_at: {
    label: "Expires at",
    placeholder: "2031-01-01T00:00:00Z",
    hint: "An RFC 3339 date or a unix second count. A date in the past turns the chip red.",
  },
  scopes: { label: "Scopes", placeholder: "read, write" },
  account_id: { label: "Account", placeholder: "acct_1…" },
  fingerprint: { label: "Fingerprint", placeholder: "SHA256:…" },
  fingerprint_algorithm: { label: "Fingerprint algorithm", placeholder: "sha256" },
  public_key: {
    label: "Public key",
    placeholder: "ssh-ed25519 AAAA…",
    hint: "The public half only. The private key is the sealed value and is never typed here.",
  },
  comment: { label: "Comment", placeholder: "release@ci" },
};

/** `/secrets/credentials`. */
export function CredentialsView() {
  const [state, setState] = useState<CredentialsResponse | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [busyId, setBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<{ id: string; message: string } | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [wizard, setWizard] = useState<{ secretId: string; secretName: string } | null>(null);
  const search = useRef<HTMLInputElement | null>(null);
  const heading = useRef<HTMLHeadingElement | null>(null);

  const load = useCallback(async () => {
    try {
      const next = await fetchCredentials();
      setState(next);
      setStatus("ready");
      setLoadError(null);
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The credentials could not be read." },
      );
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.isContentEditable;
      if (event.key === "Escape") {
        if (wizard) {
          setWizard(null);
        } else if (openId) {
          setOpenId(null);
        }
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      if (event.key === "/") {
        event.preventDefault();
        search.current?.focus();
      }
      if (event.key === "n" && !wizard) {
        event.preventDefault();
        // The wizard needs a secret to attach to; with none typed the screen says so rather
        // than opening a form with nothing to save it on.
        if (state && state.total > 0) {
          const first = state.credentials[0];
          setWizard({ secretId: first.id, secretName: first.name });
          window.setTimeout(() => heading.current?.focus(), 0);
        } else {
          setNotice("Create a secret first — a credential is a typed view of one.");
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openId, state, wizard]);

  const validate = async (credential: Credential) => {
    setBusyId(credential.id);
    setActionError(null);
    setNotice(null);
    try {
      const result = await validateCredential(credential.id);
      setNotice(
        result.valid
          ? `${credential.name} validated: ${result.validation_message}`
          : `${credential.name} did not validate: ${result.validation_message}`,
      );
      await load();
    } catch (cause) {
      setActionError({
        id: credential.id,
        message: cause instanceof ApiError ? cause.message : "The validation could not be run.",
      });
    } finally {
      setBusyId(null);
    }
  };

  const credentials = state?.credentials ?? [];
  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return credentials.filter((credential) => {
      if (filter !== "all" && credential.validation_state !== filter) {
        return false;
      }
      if (!needle) {
        return true;
      }
      return (
        credential.name.toLowerCase().includes(needle) ||
        credential.kind.toLowerCase().includes(needle) ||
        credential.kind_description.toLowerCase().includes(needle) ||
        credential.slots.some((slot) => slot.toLowerCase().includes(needle))
      );
    });
  }, [credentials, filter, query]);

  if (status === "loading") {
    return <LoadingTable columns={5} />;
  }

  if (status === "error" && loadError) {
    return (
      <div className="flex flex-col gap-3">
        <p
          role="alert"
          data-credentials-error
          className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
        >
          {loadError.message}
        </p>
        <p className="text-[11.5px] text-muted">
          Code <code className="font-mono">{loadError.code}</code>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="flex h-8 w-fit items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-panel"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {/* Counters, then the filter that drives them. */}
      <div className="flex flex-wrap items-center gap-2">
        <Stat label="Total" value={state?.total ?? 0} />
        <Stat label="Valid" value={state?.valid ?? 0} tone="positive" />
        <Stat label="Failing" value={state?.invalid ?? 0} tone={state?.invalid ? "caution" : undefined} />
        <Stat label="Unchecked" value={state?.unknown ?? 0} />
        <div className="ml-auto flex flex-wrap items-center gap-2">
          <label className="relative flex items-center">
            <Search
              className="pointer-events-none absolute left-2.5 size-3.5 text-muted"
              aria-hidden
            />
            <input
              ref={search}
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search credentials  /"
              aria-label="Search credentials"
              data-credentials-search
              className="h-8 w-56 rounded-lg border border-line bg-surface pl-8 pr-2 text-[12.5px] outline-none transition focus:border-accent"
            />
          </label>
          <div className="flex items-center gap-1" role="group" aria-label="Filter by validation">
            {(["all", "valid", "invalid", "unknown"] as const).map((option) => (
              <button
                key={option}
                type="button"
                onClick={() => setFilter(option)}
                aria-pressed={filter === option}
                data-credentials-filter={option}
                className={`h-8 rounded-lg border px-2.5 text-[12px] capitalize transition ${
                  filter === option
                    ? "border-accent bg-accent-soft text-accent-strong"
                    : "border-line text-muted hover:bg-panel"
                }`}
              >
                {option}
              </button>
            ))}
          </div>
        </div>
      </div>

      {notice ? (
        <p
          role="status"
          data-credentials-notice
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      <section className="rounded-xl border border-line bg-surface">
        {credentials.length === 0 ? (
          <EmptyState
            title="No typed credentials yet"
            hint="A credential is a stored secret pinned to one of five kinds — an API key, an OAuth token, an SMTP account, a payment key or an SSH key. The non-secret fields it records are what the validator and every consumer need."
            action={
              <span className="text-[11.5px] text-muted">
                Sealed values are managed in the secrets store; type one here once it exists.
              </span>
            }
          />
        ) : visible.length === 0 ? (
          <EmptyState
            title="Nothing matches that filter"
            hint="A different validation state, or a shorter search term, will bring rows back."
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[720px] text-left text-[12.5px]">
              <thead className="border-b border-line text-[11.5px] text-muted">
                <tr>
                  <th className="px-4 py-2 font-medium">Name</th>
                  <th className="px-4 py-2 font-medium">Kind</th>
                  <th className="px-4 py-2 font-medium">Validation</th>
                  <th className="px-4 py-2 font-medium">Last checked</th>
                  <th className="px-4 py-2 font-medium">Assigned to</th>
                  <th className="px-4 py-2 font-medium" />
                </tr>
              </thead>
              <tbody>
                {visible.map((credential) => (
                  <CredentialRow
                    key={credential.id}
                    credential={credential}
                    expanded={openId === credential.id}
                    busy={busyId === credential.id}
                    error={actionError?.id === credential.id ? actionError.message : null}
                    onToggle={() =>
                      setOpenId((current) => (current === credential.id ? null : credential.id))
                    }
                    onValidate={() => void validate(credential)}
                    onEdit={() =>
                      setWizard({ secretId: credential.id, secretName: credential.name })
                    }
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {credentials.length > 0 ? (
        <p className="text-[11.5px] text-muted">
          <KeyRound className="mr-1 inline size-3.5" aria-hidden />
          Press <kbd className="font-mono">/</kbd> to search, <kbd className="font-mono">n</kbd> to
          type a credential, <kbd className="font-mono">Esc</kbd> to close.
        </p>
      ) : null}

      {wizard ? (
        <CredentialWizard
          secretId={wizard.secretId}
          secretName={wizard.secretName}
          kinds={state?.kinds ?? []}
          existing={credentials.find((credential) => credential.id === wizard.secretId) ?? null}
          onClose={() => setWizard(null)}
          onSaved={async (message) => {
            setWizard(null);
            setNotice(message);
            await load();
          }}
        />
      ) : null}
    </div>
  );
}

/** A counter in the header strip. */
function Stat({
  label,
  value,
  tone,
}: {
  label: string;
  value: number;
  tone?: "positive" | "caution";
}) {
  return (
    <span
      className={`flex items-baseline gap-1.5 rounded-full px-2.5 py-1 text-[12px] ${
        tone === "positive"
          ? "bg-positive-soft text-positive"
          : tone === "caution"
            ? "bg-danger-soft text-caution"
            : "bg-quiet-soft text-muted"
      }`}
    >
      <span className="font-medium tabular-nums">{value}</span>
      {label}
    </span>
  );
}

/** The chip: a colour *and* a word, so it is readable without colour. */
function ValidationChip({ credential }: { credential: Credential }) {
  const state = credential.validation_state;
  const tone =
    state === "valid"
      ? "bg-positive-soft text-positive"
      : state === "invalid"
        ? "bg-danger-soft text-caution"
        : "bg-quiet-soft text-muted";
  const Icon =
    state === "valid" ? CheckCircle2 : state === "invalid" ? AlertTriangle : CircleHelp;
  const word = state === "stale" ? "stale" : state;
  return (
    <span
      data-credential-chip={state}
      className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium ${tone}`}
    >
      <Icon className="size-3" aria-hidden />
      {word}
    </span>
  );
}

/** One credential row, expanding into its non-secret field list. */
function CredentialRow({
  credential,
  expanded,
  busy,
  error,
  onToggle,
  onValidate,
  onEdit,
}: {
  credential: Credential;
  expanded: boolean;
  busy: boolean;
  error: string | null;
  onToggle: () => void;
  onValidate: () => void;
  onEdit: () => void;
}) {
  return (
    <>
      <tr data-credential-row className="border-b border-line last:border-0">
        <td className="px-4 py-2">
          <button
            type="button"
            onClick={onToggle}
            aria-expanded={expanded}
            data-credential-expand={credential.id}
            className="text-left font-medium hover:underline"
          >
            {credential.name}
          </button>
          {credential.read_only ? (
            <p
              data-credential-readonly
              className="mt-0.5 text-[11px] text-muted"
            >
              Managed outside the platform — {credential.provider_locator ?? credential.provider}
            </p>
          ) : null}
        </td>
        <td className="px-4 py-2">
          {credential.read_only ? (
            // A bridge has no kind — it has a *source*. Printing "external" as if it were one of
            // the five kinds would be a lie the operator has to decode.
            <span className="font-mono text-[11.5px] text-muted">
              {credential.provider} · read-only
            </span>
          ) : (
            <span className="font-mono text-[11.5px]">{credential.kind}</span>
          )}
          {credential.offline_checkable ? (
            <span className="ml-1.5 text-[10.5px] text-muted">offline check</span>
          ) : null}
        </td>
        <td className="px-4 py-2">
          <ValidationChip credential={credential} />
        </td>
        <td className="px-4 py-2 text-muted">
          {credential.validation_checked_at
            ? formatTimestamp(credential.validation_checked_at)
            : "never"}
        </td>
        <td className="px-4 py-2 text-[11.5px] text-muted">
          {credential.slots.length > 0 ? credential.slots.join(", ") : "—"}
        </td>
        <td className="px-4 py-2">
          <div className="flex items-center justify-end gap-1.5">
            <button
              type="button"
              onClick={onValidate}
              disabled={busy || credential.read_only}
              title={
                credential.read_only
                  ? "A credential managed outside the platform cannot be validated from here"
                  : "Run this kind's validator now"
              }
              data-credential-validate={credential.id}
              className="flex h-7 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-panel disabled:opacity-60"
            >
              <RefreshCw className={`size-3 ${busy ? "animate-spin" : ""}`} aria-hidden />
              Validate
            </button>
            <button
              type="button"
              onClick={onEdit}
              data-credential-edit={credential.id}
              className="h-7 rounded-lg border border-line px-2 text-[11.5px] transition hover:bg-panel"
            >
              Edit fields
            </button>
          </div>
        </td>
      </tr>
      {expanded ? (
        <tr data-credential-detail-row className="border-b border-line bg-quiet-soft/40 last:border-0">
          <td colSpan={6} className="px-4 py-3">
            <p className="text-[12px] text-muted">{credential.kind_description}</p>
            {credential.field_pairs.length > 0 ? (
              <dl className="mt-2 grid grid-cols-1 gap-x-6 gap-y-1.5 sm:grid-cols-2">
                {credential.field_pairs.map(([name, value]) => (
                  <div key={name} className="flex gap-2 text-[12.5px]">
                    <dt className="w-40 shrink-0 text-muted">{name}</dt>
                    <dd className="min-w-0 break-all font-mono text-[11.5px]">{value}</dd>
                  </div>
                ))}
              </dl>
            ) : (
              <p className="mt-2 text-[12px] text-muted">
                No non-secret fields were recorded for this credential.
              </p>
            )}
            <p className="mt-2 text-[11.5px] text-muted">
              Version {credential.version} · the value itself is sealed and never rendered here.
            </p>
            {credential.validation_message ? (
              <p
                data-credential-message
                className={`mt-2 text-[12px] ${
                  credential.validation_state === "invalid" ? "text-caution" : "text-muted"
                }`}
              >
                {credential.validation_message}
              </p>
            ) : null}
            {error ? (
              <p
                role="alert"
                data-credential-error
                className="mt-2 rounded-lg border border-danger/40 bg-danger-soft px-3 py-1.5 text-[12px] text-caution"
              >
                {error}
              </p>
            ) : null}
          </td>
        </tr>
      ) : null}
    </>
  );
}

/**
 * The create/edit wizard: pick kind → fill the non-secret fields → save.
 *
 * There is no value box. That is deliberate and it is the screen's strongest guarantee: the
 * component has nowhere to put a secret, so a future edit cannot leak one into the DOM.
 */
function CredentialWizard({
  secretId,
  secretName,
  kinds,
  existing,
  onClose,
  onSaved,
}: {
  secretId: string;
  secretName: string;
  kinds: CredentialKindOption[];
  existing: Credential | null;
  onClose: () => void;
  onSaved: (message: string) => void | Promise<void>;
}) {
  const [kind, setKind] = useState(existing?.kind ?? kinds[0]?.kind ?? "api_key");
  const [fields, setFields] = useState<Record<string, string>>(() => {
    const initial: Record<string, string> = {};
    for (const [name, value] of existing?.field_pairs ?? []) {
      const match = Object.keys(FIELD_LABELS).find(
        (key) => FIELD_LABELS[key].label.toLowerCase() === name.toLowerCase(),
      );
      if (match) {
        initial[match] = value;
      }
    }
    return initial;
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const heading = useRef<HTMLHeadingElement | null>(null);

  const option = kinds.find((candidate) => candidate.kind === kind);

  useEffect(() => {
    heading.current?.focus();
  }, []);

  const submit = async () => {
    setBusy(true);
    setError(null);
    const payload: Record<string, string> = {};
    for (const [name, value] of Object.entries(fields)) {
      const trimmed = value.trim();
      if (trimmed) {
        payload[name] = trimmed;
      }
    }
    try {
      const saved = await attachCredentialProfile(secretId, kind, payload);
      await onSaved(
        saved.validation_state === "valid"
          ? `${saved.name} is typed and validated.`
          : `${saved.name} is saved, but its validator did not pass: ${saved.validation_message} — the credential is stored either way.`,
      );
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The credential could not be saved.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 pt-[8vh]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="credential-wizard-title"
      data-credential-wizard
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl">
        <h3
          id="credential-wizard-title"
          ref={heading}
          tabIndex={-1}
          className="text-[15px] font-medium outline-none"
        >
          {existing ? "Edit the non-secret fields" : "Type this secret as a credential"}
        </h3>
        <p className="mt-1 text-[12.5px] text-muted">
          <span className="font-mono">{secretName}</span> — the value itself is already sealed and
          is never shown, typed or returned by this screen.
        </p>

        <fieldset className="mt-3">
          <legend className="text-[12px] font-medium">Kind</legend>
          <div className="mt-1.5 flex flex-col gap-1">
            {kinds.map((candidate) => (
              <label key={candidate.kind} className="flex items-start gap-2 text-[12.5px]">
                <input
                  type="radio"
                  name="credential-kind"
                  value={candidate.kind}
                  checked={kind === candidate.kind}
                  onChange={() => setKind(candidate.kind)}
                  data-credential-kind={candidate.kind}
                  className="mt-0.5 size-4 border-line"
                />
                <span>
                  <span className="font-mono text-[11.5px]">{candidate.kind}</span>{" "}
                  <span className="text-muted">{candidate.description}</span>
                  {candidate.offline ? (
                    <span className="ml-1.5 text-[10.5px] text-muted">offline check</span>
                  ) : null}
                </span>
              </label>
            ))}
          </div>
        </fieldset>

        <div className="mt-3 flex flex-col gap-2.5">
          {(option?.fields ?? []).map((name) => {
            const meta = FIELD_LABELS[name] ?? { label: name, placeholder: "" };
            return (
              <label key={name} className="flex flex-col gap-1 text-[12.5px]">
                <span className="font-medium">{meta.label}</span>
                <input
                  type="text"
                  value={fields[name] ?? ""}
                  onChange={(event) =>
                    setFields((current) => ({ ...current, [name]: event.target.value }))
                  }
                  placeholder={meta.placeholder}
                  data-credential-field={name}
                  aria-describedby={meta.hint ? `credential-hint-${name}` : undefined}
                  className="h-8 rounded-lg border border-line bg-panel px-2.5 text-[12.5px] outline-none transition focus:border-accent"
                />
                {meta.hint ? (
                  <span id={`credential-hint-${name}`} className="text-[11px] text-muted">
                    {meta.hint}
                  </span>
                ) : null}
              </label>
            );
          })}
        </div>

        {error ? (
          <p
            role="alert"
            data-credential-wizard-error
            className="mt-3 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution"
          >
            {error}
          </p>
        ) : null}

        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={() => void submit()}
            disabled={busy}
            data-credential-save
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
          >
            <Plus className="size-3.5" aria-hidden />
            {busy ? "Saving…" : "Save the credential"}
          </button>
        </div>
      </div>
    </div>
  );
}
