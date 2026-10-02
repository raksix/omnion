"use client";

/**
 * The create form (`/workflows/credentials/new`): pick a type, fill in its fields, save.
 *
 * The form is generated from the type's definition in the registry, so a third-party type
 * added to the catalogue appears here with its own fields and no change to this file. Five
 * things it refuses to get wrong:
 *
 * 1. **A secret input is off by default and never comes back.** The reveal toggle is a
 *    password-visibility switch, not a "show me the stored value" — there is no stored value
 *    to show, because the API never returns one. The helper text says so once, in the reader's
 *    words, instead of a warning icon nobody reads.
 * 2. **The type picker is the first step, not a select.** Eleven credential types with six
 *    fields each is a form nobody can scan, so the type is a choice of cards and the fields
 *    appear after it. A `?type=` in the URL skips straight to the form, which is what the
 *    empty state's two quick actions link to.
 * 3. **Saving is allowed without a passing test.** The REQ says so, and for a real reason: the
 *    test hook needs an outbound call nobody has configured yet, and a form that refuses to
 *    save until a check passes is a form that cannot be used at all. The untested state is
 *    shown honestly as "Not verified" afterwards.
 * 4. **A required field the type declares is marked before it is refused.** The API's
 *    `credential_field_required` names the field, and the form puts the message under the
 *    input rather than in a toast — the reader is already looking at the form.
 * 5. **The key is derived and shown, and can be overridden.** A graph names a credential by
 *    key, so the reader needs to see the key their workflow will use. Deriving it from the
 *    name is the default; a reader who wants `stripe-prod` rather than `stripe-prod-account`
 *    types it, and the field validates before the submit rather than after.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import {
  ArrowLeft,
  CircleAlert,
  Eye,
  EyeOff,
  KeyRound,
  Loader2,
  ShieldCheck,
  TriangleAlert,
} from "lucide-react";

import { createCredential, fetchCredentialTypes, type ApiError } from "@/lib/api";
import type { CredentialType, CredentialTypeField } from "@/lib/types";
import { useOrganizationScope } from "@/lib/scope";

/** The icon a type's card shows. */
const TYPE_ICON: Record<string, typeof KeyRound> = {
  api_key: KeyRound,
  oauth2: ShieldCheck,
  basic_auth: KeyRound,
  smtp: CircleAlert,
  cloud_storage: KeyRound,
};

/** A credential key the graph may name: the same rule the API enforces. */
const KEY_PATTERN = /^[a-z0-9][a-z0-9_-]{0,63}$/;

/**
 * Derive a key from a name, in the browser, so the reader sees the key before saving.
 *
 * The API derives it too and would derive the same thing — but showing a placeholder that
 * might not match is worse than showing the value that will be used, so the rule is written
 * out here as well. It is short enough that two copies cannot really drift.
 */
function deriveKey(name: string): string {
  let key = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
  if (key.length > 64) key = key.slice(0, 64).replace(/-+$/, "");
  return key;
}

export function CredentialCreate() {
  const router = useRouter();
  const params = useSearchParams();
  const preselected = params.get("type");
  // A tenant creates in its own organization and names nothing. A platform account has none of
  // its own, so it names the tenant it is creating in — the API refuses `organization_required`
  // otherwise, and the form would report a refusal for a perfectly valid credential.
  const { platformAccount, organizationId, organizations, setOrganizationId, needsOrganization } =
    useOrganizationScope();

  const [types, setTypes] = useState<CredentialType[]>([]);
  const [chosen, setChosen] = useState<string | null>(preselected);
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [keyTouched, setKeyTouched] = useState(false);
  const [scope, setScope] = useState("organization");
  const [sharing, setSharing] = useState("private");
  const [values, setValues] = useState<Record<string, string>>({});
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<ApiError | null>(null);
  const [loadingTypes, setLoadingTypes] = useState(true);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let alive = true;
    fetchCredentialTypes()
      .then((page) => {
        if (alive) setTypes(page.types);
      })
      .catch((cause: ApiError) => {
        if (alive) setError(cause);
      })
      .finally(() => {
        if (alive) setLoadingTypes(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  const definition = useMemo(
    () => types.find((type) => type.key === chosen) ?? null,
    [types, chosen],
  );

  // The key follows the name until the reader types one of their own; after that it is theirs
  // and the form stops overwriting it, because a field that fights the typist is a field
  // they will paste into a text box and forget to check.
  useEffect(() => {
    if (!keyTouched) setKey(deriveKey(name));
  }, [name, keyTouched]);

  const pick = useCallback((typeKey: string) => {
    setChosen(typeKey);
    setValues({});
    setRevealed({});
    setFieldErrors({});
    setError(null);
  }, []);

  const fields: CredentialTypeField[] = definition?.fields ?? [];
  const nonSecret = fields.filter((field) => field.kind !== "secret");
  const secrets = fields.filter((field) => field.kind === "secret");
  const effectiveKey = key.trim() || deriveKey(name);

  const onSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!definition) return;

    const errors: Record<string, string> = {};
    for (const field of nonSecret) {
      if (field.required && !(values[field.name] ?? "").trim()) {
        errors[field.name] = `${field.label} is required`;
      }
    }
    for (const field of secrets) {
      if (field.required && !(values[field.name] ?? "").trim()) {
        errors[field.name] = `${field.label} is required`;
      }
    }
    if (!name.trim()) errors.name = "A credential needs a name";
    if (!KEY_PATTERN.test(effectiveKey)) {
      errors.key =
        "A key is 1–64 characters of a–z, 0–9, '_' or '-', starting with a letter or a digit";
    }
    setFieldErrors(errors);
    if (Object.keys(errors).length > 0) return;

    // Secrets ride in their own array and nowhere else. The API refuses a secret inside
    // `settings` by name, and this is the shape that satisfies it.
    const payload: Parameters<typeof createCredential>[0] = {
      name: name.trim(),
      type: definition.key,
      scope,
      sharing,
      settings: Object.fromEntries(
        nonSecret
          .filter((field) => (values[field.name] ?? "").trim() !== "")
          .map((field) => [field.name, coerce(field, values[field.name] ?? "")]),
      ),
      secrets: secrets
        .filter((field) => (values[field.name] ?? "").trim() !== "")
        .map((field) => ({ field: field.name, value: values[field.name] })),
    };
    if (key.trim()) payload.key = effectiveKey;

    setSaving(true);
    setError(null);
    try {
      const created = await createCredential(payload, organizationId);
      // A secret that could not be attached is not a failed create — the row is real and the
      // reader is about to be sent to it. The warning rides along so the detail screen can say
      // it on that row; silently dropping it is how somebody ends up debugging a node that
      // cannot authenticate for want of a key they are certain they pasted.
      router.push(
        `/workflows/credentials/${created.id}${
          created.secret_write_warning ? "?warning=1" : ""
        }`,
      );
    } catch (cause) {
      const failure = cause as ApiError;
      // The API names the field it refused; put the message under that input rather than in a
      // banner, because the reader is looking at the form and not at the top of the page.
      const field = (failure.details as { field?: string } | null)?.field;
      if (field) setFieldErrors({ [field]: failure.message });
      setError(failure);
    } finally {
      setSaving(false);
    }
  };

  if (loadingTypes) {
    return (
      <p className="text-[13px] text-muted" data-credential-create-loading>
        Loading the credential catalogue…
      </p>
    );
  }

  if (!definition) {
    return (
      <div className="space-y-4">
        <button
          type="button"
          onClick={() => router.push("/workflows/credentials")}
          className="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-ink"
        >
          <ArrowLeft size={14} />
          Credentials
        </button>
        <div>
          <h2 className="text-[15px] font-medium text-ink">What are you connecting?</h2>
          <p className="mt-1 text-[13px] text-muted">
            Each type has its own fields. Pick one and the form below fills in.
          </p>
        </div>
        <ul className="grid gap-2 sm:grid-cols-2" data-credential-type-picker>
          {types.map((type) => {
            const Icon = TYPE_ICON[type.key] ?? KeyRound;
            return (
              <li key={type.key}>
                <button
                  type="button"
                  data-credential-type={type.key}
                  onClick={() => pick(type.key)}
                  className="flex w-full items-start gap-3 rounded-xl border border-line bg-surface p-4 text-left hover:border-accent"
                >
                  <span className="rounded-lg border border-line p-2">
                    <Icon size={16} aria-hidden />
                  </span>
                  <span className="min-w-0">
                    <span className="block text-[14px] font-medium text-ink">{type.label}</span>
                    <span className="mt-0.5 block text-[12px] text-muted">
                      {type.description}
                    </span>
                    {type.used_by.length > 0 ? (
                      <span className="mt-1 block text-[11px] text-muted">
                        Used by {type.used_by.length} node{type.used_by.length === 1 ? "" : "s"}
                      </span>
                    ) : null}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
        {error ? (
          <p
            role="alert"
            className="flex items-center gap-2 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
          >
            <TriangleAlert size={15} />
            {error.message}
          </p>
        ) : null}
      </div>
    );
  }

  return (
    <form onSubmit={onSubmit} className="max-w-2xl space-y-5">
      <button
        type="button"
        onClick={() => setChosen(null)}
        className="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-ink"
      >
        <ArrowLeft size={14} />
        All types
      </button>

      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium text-ink">{definition.label}</h2>
          <p className="mt-1 text-[13px] text-muted">{definition.description}</p>
        </div>
        {platformAccount ? (
          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Organization</span>
            <select
              data-credential-organization
              value={organizationId ?? ""}
              onChange={(event) => setOrganizationId(event.target.value || null)}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              {(organizations ?? []).length === 0 ? (
                <option value="">No organization</option>
              ) : null}
              {(organizations ?? []).map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : null}
      </div>

      {error ? (
        <p
          role="alert"
          data-credential-create-error
          className="flex items-center gap-2 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
        >
          <TriangleAlert size={15} />
          {error.message}
        </p>
      ) : null}

      <div className="space-y-4 rounded-xl border border-line bg-surface p-4">
        <Field
          name="name"
          label="Name"
          required
          value={name}
          error={fieldErrors.name}
          help="What this connection is for, in your own words."
          onChange={(value) => setName(value)}
        />

        <Field
          name="key"
          label="Key"
          required
          value={effectiveKey}
          error={fieldErrors.key}
          help="What a workflow names to use this. Lowercase letters, digits, - and _."
          onChange={(value) => {
            setKeyTouched(true);
            setKey(value);
          }}
          mono
        />

        {nonSecret.map((field) => (
          <Field
            key={field.name}
            name={field.name}
            label={field.label}
            required={field.required}
            type={field.kind === "number" ? "number" : field.kind === "url" ? "url" : "text"}
            value={values[field.name] ?? ""}
            error={fieldErrors[field.name]}
            help={field.help ?? undefined}
            onChange={(value) =>
              setValues((current) => ({ ...current, [field.name]: value }))
            }
          />
        ))}

        {secrets.length > 0 ? (
          <div className="space-y-4 border-t border-line pt-4">
            <p className="text-[12px] text-muted">
              Shown once, then never again. The platform stores it in its encrypted secret
              store and never sends it back — not to this screen, not to any API response.
            </p>
            {secrets.map((field) => (
              <SecretField
                key={field.name}
                field={field}
                value={values[field.name] ?? ""}
                error={fieldErrors[field.name]}
                revealed={revealed[field.name] ?? false}
                onReveal={(show) =>
                  setRevealed((current) => ({ ...current, [field.name]: show }))
                }
                onChange={(value) =>
                  setValues((current) => ({ ...current, [field.name]: value }))
                }
              />
            ))}
          </div>
        ) : null}
      </div>

      <div className="flex flex-wrap gap-3 rounded-xl border border-line bg-quiet-soft/40 p-4">
        <label className="flex flex-1 flex-col gap-1">
          <span className="text-[12px] font-medium text-muted">Scope</span>
          <select
            value={scope}
            onChange={(event) => setScope(event.target.value)}
            className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <option value="organization">Organization</option>
            <option value="project">Project</option>
          </select>
        </label>
        <label className="flex flex-1 flex-col gap-1">
          <span className="text-[12px] font-medium text-muted">Sharing</span>
          <select
            value={sharing}
            onChange={(event) => setSharing(event.target.value)}
            className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
          >
            <option value="private">Private to me</option>
            <option value="organization">Shared with the organization</option>
          </select>
        </label>
      </div>

      <div className="flex items-center gap-3">
        <button
          type="submit"
          data-credential-save
          // A platform account with nothing chosen has no organization to create in, and the
          // API answers `organization_required`. Saying so on the button is better than a
          // refusal after the reader has filled in the form.
          disabled={saving || needsOrganization}
          className="inline-flex items-center gap-2 rounded-lg bg-accent px-4 py-2 text-[13px] font-medium text-white disabled:opacity-60"
        >
          {saving ? <Loader2 size={14} className="animate-spin" /> : null}
          Save credential
        </button>
        <span className="text-[12px] text-muted">
          {needsOrganization
            ? "Choose the organization this credential belongs to."
            : "Saving is fine without a test — it shows as “Not verified” until you run one."}
        </span>
      </div>
    </form>
  );
}

/** Coerce a form string into the JSON type the field declares. */
function coerce(field: CredentialTypeField, raw: string): string | number | boolean {
  if (field.kind === "number") {
    const parsed = Number(raw);
    return Number.isFinite(parsed) ? parsed : raw;
  }
  if (field.kind === "boolean") return raw === "true";
  return raw;
}

/** One labelled input with its help and its error. */
function Field({
  name,
  label,
  value,
  onChange,
  required,
  help,
  error,
  type = "text",
  mono,
}: {
  name: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  required?: boolean;
  help?: string;
  error?: string;
  type?: string;
  mono?: boolean;
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[12px] font-medium text-muted">
        {label}
        {required ? <span aria-hidden> *</span> : null}
      </span>
      <input
        name={name}
        data-credential-field={name}
        type={type}
        value={value}
        required={required}
        aria-invalid={error ? true : undefined}
        onChange={(event) => onChange(event.target.value)}
        className={`rounded-lg border bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent ${
          error ? "border-red-500" : "border-line"
        } ${mono ? "font-mono text-[12px]" : ""}`}
      />
      {help ? <span className="text-[11px] text-muted">{help}</span> : null}
      {error ? (
        <span role="alert" className="text-[11px] text-red-600 dark:text-red-400">
          {error}
        </span>
      ) : null}
    </label>
  );
}

/**
 * One secret input.
 *
 * The eye is a visibility switch on what the reader is typing *now*, and it is off by default
 * — a password field that shows itself is a screenshot waiting to happen. It is not a way to
 * reveal a stored value, because there is no stored value on this screen to reveal.
 */
function SecretField({
  field,
  value,
  onChange,
  revealed,
  onReveal,
  error,
}: {
  field: CredentialTypeField;
  value: string;
  onChange: (value: string) => void;
  revealed: boolean;
  onReveal: (show: boolean) => void;
  error?: string;
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[12px] font-medium text-muted">
        {field.label}
        {field.required ? <span aria-hidden> *</span> : null}
      </span>
      <span className="relative">
        <input
          name={field.name}
          data-credential-secret={field.name}
          type={revealed ? "text" : "password"}
          value={value}
          required={field.required}
          autoComplete="off"
          aria-invalid={error ? true : undefined}
          onChange={(event) => onChange(event.target.value)}
          className={`w-full rounded-lg border bg-surface px-3 py-2 pr-10 font-mono text-[12px] outline-none focus-visible:ring-2 focus-visible:ring-accent ${
            error ? "border-red-500" : "border-line"
          }`}
        />
        <button
          type="button"
          aria-label={revealed ? `Hide ${field.label}` : `Show ${field.label}`}
          onClick={() => onReveal(!revealed)}
          className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-1 text-muted hover:text-ink"
        >
          {revealed ? <EyeOff size={15} /> : <Eye size={15} />}
        </button>
      </span>
      {field.help ? <span className="text-[11px] text-muted">{field.help}</span> : null}
      {error ? (
        <span role="alert" className="text-[11px] text-red-600 dark:text-red-400">
          {error}
        </span>
      ) : null}
    </label>
  );
}
