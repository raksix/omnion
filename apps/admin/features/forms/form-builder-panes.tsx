"use client";

/**
 * The builder's right-hand halves: the field inspector, the settings drawer and the preview
 * (REQ-064, slice 2).
 *
 * The preview is the interesting one. It is not a mock: it renders the *same* fields the canvas
 * holds and runs the *same* rules the public submit route will run, so a preview that is more
 * forgiving than the live form is impossible by construction — which is the only way a "preview"
 * tab earns its place in a builder.
 */
import { useCallback, useMemo, useState } from "react";
import { Loader2 } from "lucide-react";

import { ApiError, updateForm } from "@/lib/api";
import type { FormDetail } from "@/lib/types";

import type { Draft } from "./form-builder";

/** One numeric rule read out of a field's rules object, for the inspector's inputs. */
function ruleNumber(field: Draft, name: string): string {
  const value = field.rules[name];
  return typeof value === "number" ? String(value) : "";
}

/** The inspector. Every control writes a rule; none of them decides anything. */
export function FieldInspector({
  field,
  problem,
  onChange,
}: {
  field: Draft;
  problem?: string;
  onChange: (change: Partial<Draft>) => void;
}) {
  const setRule = useCallback(
    (name: string, raw: string) => {
      const rules = { ...field.rules };
      const trimmed = raw.trim();
      // An emptied input *removes* the rule rather than storing "" — a rule whose value is an
      // empty string is a rule that validates against nothing, and the store would then have to
      // decide what "" means.
      if (trimmed === "") {
        delete rules[name];
      } else if (/^\d+$/.test(trimmed)) {
        rules[name] = Number.parseInt(trimmed, 10);
      }
      onChange({ rules });
    },
    [field.rules, onChange],
  );

  const hasOptions = ["select", "radio", "checkbox"].includes(field.field_type);

  return (
    <div className="space-y-3" data-form-inspector-for={field.key}>
      <div>
        <label htmlFor={`label-${field.draftId}`} className="block text-[12px] font-medium">
          Label
        </label>
        <input
          id={`label-${field.draftId}`}
          data-form-inspector-label
          value={field.label}
          onChange={(event) => onChange({ label: event.target.value })}
          className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
        />
      </div>
      <div>
        <label htmlFor={`key-${field.draftId}`} className="block text-[12px] font-medium">
          Key
        </label>
        <input
          id={`key-${field.draftId}`}
          data-form-inspector-key
          value={field.key}
          onChange={(event) => onChange({ key: event.target.value })}
          className="mt-1 w-full rounded-md border border-line px-2 py-1 font-mono text-[12.5px]"
        />
        <p className="mt-1 text-[11px] text-muted">
          The name this answer is stored under. Renaming it does not rewrite history.
        </p>
      </div>

      {field.field_type === "consent" ? (
        /* A consent field's label *is* the text the visitor agrees to — which is why it is
           stored verbatim rather than as a boolean. The panel says so where the editor writes
           it, not only in the docs. */
        <p className="rounded-md bg-quiet-soft px-2 py-1.5 text-[11.5px] text-muted">
          The label above is the sentence the visitor accepts, and it is stored with every
          submission. Write it as the sentence a person would agree to.
        </p>
      ) : (
        <>
          <div>
            <label htmlFor={`ph-${field.draftId}`} className="block text-[12px] font-medium">
              Placeholder
            </label>
            <input
              id={`ph-${field.draftId}`}
              data-form-inspector-placeholder
              value={field.placeholder}
              onChange={(event) => onChange({ placeholder: event.target.value })}
              className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
            />
          </div>
          <div>
            <label htmlFor={`help-${field.draftId}`} className="block text-[12px] font-medium">
              Help text
            </label>
            <input
              id={`help-${field.draftId}`}
              data-form-inspector-help
              value={field.help_text}
              onChange={(event) => onChange({ help_text: event.target.value })}
              className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
            />
          </div>
        </>
      )}

      <label className="flex items-center gap-2 text-[12px]">
        <input
          type="checkbox"
          data-form-inspector-required
          checked={field.required}
          onChange={(event) => onChange({ required: event.target.checked })}
        />
        Required
      </label>

      {hasOptions ? (
        <div>
          <label htmlFor={`opts-${field.draftId}`} className="block text-[12px] font-medium">
            Options
          </label>
          <textarea
            id={`opts-${field.draftId}`}
            data-form-inspector-options
            value={field.options.join("\n")}
            onChange={(event) =>
              onChange({ options: event.target.value.split("\n").filter((line) => line.trim() !== "") })
            }
            rows={4}
            className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
          />
          <p className="mt-1 text-[11px] text-muted">One per line.</p>
        </div>
      ) : null}

      <fieldset className="space-y-2 rounded-md border border-line p-2">
        <legend className="px-1 text-[11.5px] font-medium">Validation</legend>
        <div className="grid grid-cols-2 gap-2">
          <div>
            <label htmlFor={`min-${field.draftId}`} className="block text-[11.5px]">
              Min length
            </label>
            <input
              id={`min-${field.draftId}`}
              data-form-inspector-min-length
              value={ruleNumber(field, "min_length")}
              onChange={(event) => setRule("min_length", event.target.value)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
            />
          </div>
          <div>
            <label htmlFor={`max-${field.draftId}`} className="block text-[11.5px]">
              Max length
            </label>
            <input
              id={`max-${field.draftId}`}
              data-form-inspector-max-length
              value={ruleNumber(field, "max_length")}
              onChange={(event) => setRule("max_length", event.target.value)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
            />
          </div>
        </div>
        {["text", "textarea"].includes(field.field_type) ? (
          <div>
            <label htmlFor={`fmt-${field.draftId}`} className="block text-[11.5px]">
              Format
            </label>
            <select
              id={`fmt-${field.draftId}`}
              data-form-inspector-format
              value={typeof field.rules.format === "string" ? field.rules.format : ""}
              onChange={(event) => setRule("format", event.target.value)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
            >
              <option value="">none</option>
              <option value="email">e-mail</option>
              <option value="url">URL</option>
              <option value="tel">phone</option>
            </select>
          </div>
        ) : null}
        {field.field_type === "date" ? (
          <div className="grid grid-cols-2 gap-2">
            <div>
              <label htmlFor={`mind-${field.draftId}`} className="block text-[11.5px]">
                Earliest
              </label>
              <input
                id={`mind-${field.draftId}`}
                type="date"
                value={typeof field.rules.min_date === "string" ? field.rules.min_date : ""}
                onChange={(event) => setRule("min_date", event.target.value)}
                className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
              />
            </div>
            <div>
              <label htmlFor={`maxd-${field.draftId}`} className="block text-[11.5px]">
                Latest
              </label>
              <input
                id={`maxd-${field.draftId}`}
                type="date"
                value={typeof field.rules.max_date === "string" ? field.rules.max_date : ""}
                onChange={(event) => setRule("max_date", event.target.value)}
                className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
              />
            </div>
          </div>
        ) : null}
        {field.field_type === "file" ? (
          <div className="grid grid-cols-2 gap-2">
            <div>
              <label htmlFor={`types-${field.draftId}`} className="block text-[11.5px]">
                Allowed types
              </label>
              <input
                id={`types-${field.draftId}`}
                value={Array.isArray(field.rules.allowed_types) ? (field.rules.allowed_types as string[]).join(",") : ""}
                onChange={(event) => {
                  const rules = { ...field.rules };
                  const types = event.target.value
                    .split(",")
                    .map((part) => part.trim())
                    .filter((part) => part !== "");
                  if (types.length === 0) delete rules.allowed_types;
                  else rules.allowed_types = types;
                  onChange({ rules });
                }}
                placeholder="pdf, png"
                className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
              />
            </div>
            <div>
              <label htmlFor={`size-${field.draftId}`} className="block text-[11.5px]">
                Max bytes
              </label>
              <input
                id={`size-${field.draftId}`}
                value={ruleNumber(field, "max_bytes")}
                onChange={(event) => setRule("max_bytes", event.target.value)}
                className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12px]"
              />
            </div>
          </div>
        ) : null}
      </fieldset>

      {problem ? (
        <p data-form-inspector-problem className="text-[11.5px] text-red-700 dark:text-red-300">
          {problem}
        </p>
      ) : null}
    </div>
  );
}

/**
 * The settings drawer.
 *
 * It sends the message and the redirect URL on every save because the *server* validates the
 * pair: a message action with no message is refused. The drawer disables the input the current
 * action does not use rather than hiding it, so a reader can see that the form has one and the
 * other is inert.
 */
export function FormSettings({
  detail,
  onSaved,
}: {
  detail: FormDetail;
  onSaved: (detail: FormDetail) => void;
}) {
  const [name, setName] = useState(detail.name);
  const [key, setKey] = useState(detail.key);
  const [action, setAction] = useState<string>(detail.submit_action);
  const [message, setMessage] = useState<string>(detail.submit_message ?? "");
  const [redirect, setRedirect] = useState<string>(detail.redirect_url ?? "");
  const [notify, setNotify] = useState<string>(detail.notify_emails.join(", "));
  const [honeypot, setHoneypot] = useState(detail.honeypot);
  const [minFill, setMinFill] = useState(detail.min_fill_seconds);
  const [rateLimit, setRateLimit] = useState(detail.rate_limit_per_hour);
  const [retention, setRetention] = useState(detail.retention_days);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const save = useCallback(async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const saved = await updateForm(detail.id, {
        name,
        key,
        submit_action: action,
        // Both travel every time: the server decides which one the form uses, and sending only
        // the relevant one would make switching behaviour impossible without a second save.
        submit_message: message || null,
        redirect_url: redirect || null,
        notify_emails: notify
          .split(",")
          .map((address) => address.trim())
          .filter((address) => address !== ""),
        honeypot,
        min_fill_seconds: minFill,
        rate_limit_per_hour: rateLimit,
        retention_days: retention,
      });
      onSaved(saved);
      setNotice("Settings saved.");
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [
    action,
    detail.id,
    honeypot,
    key,
    message,
    minFill,
    name,
    notify,
    onSaved,
    rateLimit,
    redirect,
    retention,
  ]);

  return (
    <section className="space-y-3 rounded-lg border border-line p-4" data-form-settings>
      <h2 className="text-[13px] font-medium">Settings</h2>
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label htmlFor="settings-name" className="block text-[12px] font-medium">
            Name
          </label>
          <input
            id="settings-name"
            data-form-settings-name
            value={name}
            onChange={(event) => setName(event.target.value)}
            className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
          />
        </div>
        <div>
          <label htmlFor="settings-key" className="block text-[12px] font-medium">
            Public address
          </label>
          <input
            id="settings-key"
            data-form-settings-key
            value={key}
            onChange={(event) => setKey(event.target.value)}
            className="mt-1 w-full rounded-md border border-line px-2 py-1 font-mono text-[12.5px]"
          />
        </div>
      </div>

      <fieldset className="space-y-2 rounded-md border border-line p-3">
        <legend className="px-1 text-[11.5px] font-medium">After a submission</legend>
        <label className="flex items-center gap-2 text-[12px]">
          <input
            type="radio"
            name="submit-action"
            data-form-settings-action="message"
            checked={action === "message"}
            onChange={() => setAction("message")}
          />
          Show a message
        </label>
        <label className="flex items-center gap-2 text-[12px]">
          <input
            type="radio"
            name="submit-action"
            data-form-settings-action="redirect"
            checked={action === "redirect"}
            onChange={() => setAction("redirect")}
          />
          Redirect to a URL
        </label>
        {/* Both inputs stay visible and one is disabled: the form has exactly one behaviour, and
            a reader who cannot see the other one has to guess which. */}
        <div>
          <label htmlFor="settings-message" className="block text-[11.5px]">
            Message
          </label>
          <input
            id="settings-message"
            data-form-settings-message
            disabled={action !== "message"}
            value={message}
            onChange={(event) => setMessage(event.target.value)}
            className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12.5px] disabled:opacity-50"
          />
        </div>
        <div>
          <label htmlFor="settings-redirect" className="block text-[11.5px]">
            Redirect URL
          </label>
          <input
            id="settings-redirect"
            data-form-settings-redirect
            disabled={action !== "redirect"}
            value={redirect}
            onChange={(event) => setRedirect(event.target.value)}
            placeholder="/thanks"
            className="mt-0.5 w-full rounded-md border border-line px-2 py-1 font-mono text-[12.5px] disabled:opacity-50"
          />
        </div>
      </fieldset>

      <div>
        <label htmlFor="settings-notify" className="block text-[12px] font-medium">
          Notify
        </label>
        <input
          id="settings-notify"
          data-form-settings-notify
          value={notify}
          onChange={(event) => setNotify(event.target.value)}
          placeholder="owner@example.com"
          className="mt-1 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
        />
        <p className="mt-1 text-[11px] text-muted">Comma separated.</p>
      </div>

      <fieldset className="space-y-2 rounded-md border border-line p-3">
        <legend className="px-1 text-[11.5px] font-medium">Spam protection</legend>
        <p className="text-[11px] text-muted">
          A refused submission is counted, never stored. A suspicion is not a verdict: read the
          held answers before you believe the number.
        </p>
        <label className="flex items-center gap-2 text-[12px]">
          <input
            type="checkbox"
            data-form-settings-honeypot
            checked={honeypot}
            onChange={(event) => setHoneypot(event.target.checked)}
          />
          Honeypot (an invisible field a human never fills)
        </label>
        <div className="grid gap-3 sm:grid-cols-3">
          <div>
            <label htmlFor="settings-minfill" className="block text-[11.5px]">
              Min fill seconds
            </label>
            <input
              id="settings-minfill"
              type="number"
              min={0}
              max={3600}
              value={minFill}
              onChange={(event) => setMinFill(Number.parseInt(event.target.value, 10) || 0)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
            />
          </div>
          <div>
            <label htmlFor="settings-rate" className="block text-[11.5px]">
              Per hour
            </label>
            <input
              id="settings-rate"
              type="number"
              min={1}
              max={10000}
              value={rateLimit}
              onChange={(event) => setRateLimit(Number.parseInt(event.target.value, 10) || 1)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
            />
          </div>
          <div>
            <label htmlFor="settings-retention" className="block text-[11.5px]">
              Keep days
            </label>
            <input
              id="settings-retention"
              type="number"
              min={1}
              max={3650}
              value={retention}
              onChange={(event) => setRetention(Number.parseInt(event.target.value, 10) || 1)}
              className="mt-0.5 w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
            />
          </div>
        </div>
      </fieldset>

      {notice ? (
        <p data-form-settings-notice className="text-[12px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-form-settings-error className="text-[12px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}
      <button
        type="button"
        data-form-settings-save
        disabled={busy}
        onClick={() => void save()}
        className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
      >
        {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
        Save settings
      </button>
    </section>
  );
}

/**
 * The live preview.
 *
 * It renders the fields the canvas holds — not a copy — and checks them with the *same* rules
 * the public route runs, so a form the preview accepts and the site refuses cannot exist.
 */
export function FormPreview({
  detail,
  fields,
}: {
  detail: FormDetail;
  fields: Draft[];
}) {
  const [answers, setAnswers] = useState<Record<string, string | boolean>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [submitted, setSubmitted] = useState(false);

  const errorsFrom = useMemo(() => {
    // The store is the authority, so the preview asks it rather than keeping a second copy of
    // every rule. The answers are shaped exactly as the public route shapes them.
    return (candidate: Record<string, string | boolean>) => {
      const result: Record<string, string> = {};
      for (const field of fields) {
        const raw = candidate[field.key];
        const value =
          field.field_type === "consent"
            ? raw === true
            : field.field_type === "checkbox"
              ? raw === true
              : (raw as string) ?? "";
        if (field.field_type === "consent" && field.required && value !== true) {
          result[field.key] = "this field has to be accepted";
          continue;
        }
        const text = typeof value === "string" ? value.trim() : "";
        if (text === "" && !field.required) continue;
        if (text === "" && field.required) {
          result[field.key] = "this field is required";
          continue;
        }
        if (typeof value === "boolean") continue;
        const min = field.rules.min_length;
        const max = field.rules.max_length;
        if (typeof min === "number" && text.length < min) {
          result[field.key] = `please use at least ${min} characters`;
          continue;
        }
        if (typeof max === "number" && text.length > max) {
          result[field.key] = `please use at most ${max} characters`;
          continue;
        }
        if (typeof field.rules.format === "string" && text !== "") {
          const format = field.rules.format;
          const shaped =
            (format === "email" && text.includes("@")) ||
            (format === "url" && (text.startsWith("http://") || text.startsWith("https://"))) ||
            (format === "tel" && /^[+0-9 ().-]+$/.test(text));
          if (format !== "email" && format !== "url" && format !== "tel") continue;
          if (!shaped) {
            result[field.key] = `please enter a valid ${format}`;
            continue;
          }
        }
        if (
          ["select", "radio"].includes(field.field_type) &&
          field.options.length > 0 &&
          !field.options.includes(text)
        ) {
          result[field.key] = `"${text}" is not one of the offered options`;
        }
      }
      return result;
    };
  }, [fields]);

  const submit = useCallback(() => {
    const found = errorsFrom(answers);
    setErrors(found);
    setSubmitted(Object.keys(found).length === 0);
  }, [answers, errorsFrom]);

  return (
    <section className="rounded-lg border border-line p-4" data-form-preview>
      <h2 className="text-[13px] font-medium">Preview</h2>
      <p className="text-[11.5px] text-muted">
        These are the fields the canvas holds, checked by the same rules the public route runs.
      </p>
      {submitted ? (
        <p data-form-preview-success className="mt-3 rounded-md border border-line px-3 py-2 text-[12.5px]">
          Every answer is acceptable. Nothing was sent — this is a preview.
        </p>
      ) : null}
      <div className="mt-3 space-y-3">
        {fields.map((field) => (
          <div key={field.draftId} data-form-preview-field={field.key}>
            <label htmlFor={`preview-${field.draftId}`} className="block text-[12px] font-medium">
              {field.label}
              {field.required ? <span aria-hidden> *</span> : null}
            </label>
            <div className="mt-1">
              {field.field_type === "consent" ? (
                <label className="flex items-start gap-2 text-[12px]">
                  <input
                    type="checkbox"
                    id={`preview-${field.draftId}`}
                    data-form-preview-input={field.key}
                    checked={answers[field.key] === true}
                    onChange={(event) =>
                      setAnswers((current) => ({ ...current, [field.key]: event.target.checked }))
                    }
                  />
                  <span>{field.help_text || "I agree"}</span>
                </label>
              ) : field.field_type === "textarea" ? (
                <textarea
                  id={`preview-${field.draftId}`}
                  data-form-preview-input={field.key}
                  rows={3}
                  value={(answers[field.key] as string) ?? ""}
                  onChange={(event) =>
                    setAnswers((current) => ({ ...current, [field.key]: event.target.value }))
                  }
                  className="w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
                />
              ) : field.field_type === "select" ? (
                <select
                  id={`preview-${field.draftId}`}
                  data-form-preview-input={field.key}
                  value={(answers[field.key] as string) ?? ""}
                  onChange={(event) =>
                    setAnswers((current) => ({ ...current, [field.key]: event.target.value }))
                  }
                  className="w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
                >
                  <option value="">choose…</option>
                  {field.options.map((option) => (
                    <option key={option} value={option}>
                      {option}
                    </option>
                  ))}
                </select>
              ) : field.field_type === "radio" || field.field_type === "checkbox" ? (
                <div className="space-y-1">
                  {field.options.map((option) => (
                    <label key={option} className="flex items-center gap-2 text-[12px]">
                      <input
                        type={field.field_type === "radio" ? "radio" : "checkbox"}
                        name={`preview-${field.key}`}
                        data-form-preview-option={option}
                        checked={(answers[field.key] as string) === option}
                        onChange={() =>
                          setAnswers((current) => ({ ...current, [field.key]: option }))
                        }
                      />
                      {option}
                    </label>
                  ))}
                </div>
              ) : (
                <input
                  id={`preview-${field.draftId}`}
                  type={field.field_type === "date" ? "date" : "text"}
                  data-form-preview-input={field.key}
                  value={(answers[field.key] as string) ?? ""}
                  onChange={(event) =>
                    setAnswers((current) => ({ ...current, [field.key]: event.target.value }))
                  }
                  placeholder={field.placeholder || undefined}
                  className="w-full rounded-md border border-line px-2 py-1 text-[12.5px]"
                />
              )}
            </div>
            {field.help_text && field.field_type !== "consent" ? (
              <p className="mt-0.5 text-[11px] text-muted">{field.help_text}</p>
            ) : null}
            {errors[field.key] ? (
              <p data-form-preview-error={field.key} className="mt-0.5 text-[11.5px] text-red-700 dark:text-red-300">
                {errors[field.key]}
              </p>
            ) : null}
          </div>
        ))}
      </div>
      <button
        type="button"
        data-form-preview-submit
        onClick={submit}
        className="mt-3 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
      >
        Check the answers
      </button>
      <p className="mt-2 text-[11px] text-muted">
        The live form is <code className="font-mono">/{detail.key}</code> and it is currently{" "}
        {detail.status === "published" ? "accepting submissions" : "a draft, accepting nothing"}.
      </p>
    </section>
  );
}
