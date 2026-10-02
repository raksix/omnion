"use client";

/**
 * `/forms` — the site's forms, one row per form (REQ-064, slice 2).
 *
 * Four things this screen refuses to do, each because a form is the one content surface a
 * *stranger* uses and the owner usually only looks at it when something is wrong:
 *
 * 1. **A draft form is drawn as a draft, not as a working form.** The badge is not decoration: a
 *    published form accepts submissions from anybody, and an editor who saved a form and walked
 *    away believing it was live has published nothing at all.
 * 2. **The unread count is the count.** A form with nine unread submissions and a card that says
 *    "9" is the difference between opening the inbox and trusting it later.
 * 3. **A held-as-spam count is shown, never hidden.** Submissions that tripped the protections
 *    are stored nowhere, so without the counter an owner whose form "stopped working" has
 *    nothing at all to look at. The screen says what the number is *not* — a suspicion, not a
 *    verdict.
 * 4. **Deleting a form is a confirmation that names what goes with it.** The submissions are the
 *    record of what that form asked people, and they cascade.
 */
import { useCallback, useEffect, useState } from "react";
import { Loader2, Pencil, Plus, RefreshCw, Trash2 } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { ApiError, createForm, deleteForm, fetchForms } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { Form } from "@/lib/types";

/** A key derived from the name, so the common case is one field instead of two. */
function suggestKey(name: string): string {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
}

/**
 * The fields a brand-new form starts with.
 *
 * Not an empty canvas: a form with no fields cannot be published (the store refuses it, with a
 * message that says so), so an empty builder would be a screen whose only possible next action
 * is an error. One name field and one message field is the smallest form a person can actually
 * send, and it makes the *first* save succeed.
 */
const STARTER_FIELDS = [
  { key: "name", label: "Your name", type: "text", required: true },
  { key: "message", label: "Message", type: "textarea", required: true },
];

export function FormsView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [forms, setForms] = useState<Form[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Form | null>(null);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setError(null);
    try {
      setForms(await fetchForms(selectedSite.id));
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const remove = useCallback(
    async (form: Form) => {
      setBusyId(form.id);
      setNotice(null);
      setError(null);
      try {
        await deleteForm(form.id);
        // The confirmation names what disappeared with it: the submissions *are* the record of
        // what that form asked people, and they cascade on delete.
        setNotice(
          form.unread_count > 0
            ? `Deleted ${form.name} and its ${form.unread_count} unread submission${form.unread_count === 1 ? "" : "s"}.`
            : `Deleted ${form.name}.`,
        );
        setPendingDelete(null);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
        setPendingDelete(null);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  if (siteStatus === "error" && siteError) {
    return <ErrorStrip message={siteError} onRetry={() => void load()} />;
  }
  // Three states, not two: the site list is still coming, the site is known but the forms are
  // not, and the account genuinely has no site. The third is an empty state rather than a
  // spinner that never resolves — a list that spins forever is a lie about work in progress.
  if (siteStatus === "loading" || siteStatus === "idle") return <FormsSkeleton />;
  if (siteStatus === "ready" && !selectedSite) return <NoSite />;
  if (forms === null) return <FormsSkeleton />;

  return (
    <div className="space-y-6" data-forms-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          {forms.length > 0
            ? `${forms.length} form${forms.length === 1 ? "" : "s"} in this site. A draft accepts no submissions.`
            : "Forms are per site. A published form accepts submissions from anybody."}
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            data-forms-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-forms-create
            onClick={() => {
              setCreating((value) => !value);
              setError(null);
            }}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            New form
          </button>
        </div>
      </div>

      {notice ? (
        <p data-forms-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-forms-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {creating && selectedSite ? (
        <CreateFormForm
          siteId={selectedSite.id}
          onCancel={() => setCreating(false)}
          onError={setError}
          onCreated={async (name) => {
            setCreating(false);
            setNotice(`Created ${name}. It starts as a draft; publish it when the fields are right.`);
            await load();
          }}
        />
      ) : null}

      {forms.length === 0 ? (
        <div className="rounded-lg border border-line" data-forms-empty>
          <EmptyState
            title="No forms in this site yet"
            hint="A site with no form has no way for a visitor to write back — every message has to go to an address somebody typed in by hand."
            action={
              <button
                type="button"
                onClick={() => setCreating(true)}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Create the first form
              </button>
            }
          />
        </div>
      ) : (
        <ul className="space-y-2" data-forms-list>
          {forms.map((form) => (
            <li
              key={form.id}
              data-form-row={form.key}
              className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line px-4 py-3"
            >
              <div className="min-w-0">
                <p className="text-[13.5px] font-medium">
                  {form.name}
                  {/* The draft badge is load-bearing: a published form accepts submissions from
                      anybody, and an editor who saved and walked away believing it was live has
                      published nothing at all. */}
                  {form.status === "draft" ? (
                    <span
                      data-form-status={form.status}
                      className="ml-2 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                    >
                      draft · accepts nothing yet
                    </span>
                  ) : (
                    <span
                      data-form-status={form.status}
                      className="ml-2 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                    >
                      published
                    </span>
                  )}
                </p>
                <p className="text-[12px] text-muted">
                  <code className="font-mono">/{form.key}</code> · {form.field_count} field
                  {form.field_count === 1 ? "" : "s"} · changed {formatTimestamp(form.updated_at)}
                </p>
                <p className="mt-1 flex flex-wrap gap-1">
                  {form.unread_count > 0 ? (
                    <span
                      data-form-unread={form.unread_count}
                      className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] font-medium"
                    >
                      {form.unread_count} unread
                    </span>
                  ) : null}
                  {form.spam_count > 0 ? (
                    /* The counter is shown and explained, because a refused submission leaves no
                       row at all: without this number an owner whose form "stopped working" has
                       nothing to look at. The wording is a suspicion, never a verdict. */
                    <span
                      data-form-spam={form.spam_count}
                      className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                      title="Held by the honeypot, the fill-time floor or the rate limit. A suspicion, not a verdict."
                    >
                      {form.spam_count} held as spam
                    </span>
                  ) : null}
                </p>
              </div>
              <div className="flex gap-2">
                <Link
                  href={`/forms/${form.id}/edit`}
                  data-form-edit={form.id}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  <Pencil className="h-3.5 w-3.5" aria-hidden />
                  Build
                </Link>
                <Link
                  href={`/forms/${form.id}/submissions`}
                  data-form-inbox={form.id}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  Submissions
                </Link>
                <button
                  type="button"
                  data-form-delete={form.id}
                  onClick={() => setPendingDelete(form)}
                  className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  <Trash2 className="h-3.5 w-3.5" aria-hidden />
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {pendingDelete ? (
        <ConfirmDelete
          form={pendingDelete}
          busy={busyId === pendingDelete.id}
          onCancel={() => setPendingDelete(null)}
          onConfirm={() => void remove(pendingDelete)}
        />
      ) : null}
    </div>
  );
}

function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="space-y-3" data-forms-state="error">
      <p className="text-[13px] text-red-700 dark:text-red-300">{message}</p>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
      >
        <RefreshCw className="h-4 w-4" aria-hidden />
        Retry
      </button>
    </div>
  );
}

/** The account has no site, so there is nothing for a form to belong to. Named, not spun on. */
function NoSite() {
  return (
    <div className="rounded-lg border border-line" data-forms-empty="no-site">
      <EmptyState
        title="This account has no site yet"
        hint="Forms belong to a site, so there is nothing to list until a site exists."
      />
    </div>
  );
}

function FormsSkeleton() {
  return (
    <div className="space-y-3" data-forms-state="loading" aria-busy="true">
      <div className="h-4 w-48 animate-pulse rounded bg-quiet-soft" />
      {Array.from({ length: 3 }, (_, index) => (
        <div key={index} className="h-16 animate-pulse rounded-lg bg-quiet-soft" />
      ))}
    </div>
  );
}

/** Name and key. The key is editable because two forms cannot share one — it is the URL. */
function CreateFormForm({
  siteId,
  onCancel,
  onCreated,
  onError,
}: {
  siteId: string;
  onCancel: () => void;
  onCreated: (name: string) => Promise<void>;
  onError: (message: string | null) => void;
}) {
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [keyTouched, setKeyTouched] = useState(false);
  const [busy, setBusy] = useState(false);

  const submit = useCallback(async () => {
    setBusy(true);
    onError(null);
    try {
      const created = await createForm({
        site_id: siteId,
        key: key.trim() || suggestKey(name),
        name,
        fields: STARTER_FIELDS.map((field) => ({
          key: field.key,
          label: field.label,
          field_type: field.type,
          required: field.required,
          width: "full",
          rules: {},
          options: [],
        })),
      });
      await onCreated(created.name);
    } catch (caught) {
      onError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [siteId, key, name, onCreated, onError]);

  return (
    <div className="space-y-3 rounded-lg border border-line p-4" data-forms-create-form>
      <div>
        <label htmlFor="form-name" className="block text-[12.5px] font-medium">
          Name
        </label>
        <input
          id="form-name"
          data-forms-name
          value={name}
          onChange={(event) => {
            setName(event.target.value);
            if (!keyTouched) setKey(suggestKey(event.target.value));
          }}
          placeholder="Contact"
          className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 text-[13px]"
        />
      </div>
      <div>
        <label htmlFor="form-key" className="block text-[12.5px] font-medium">
          Public address
        </label>
        <input
          id="form-key"
          data-forms-key
          value={key}
          onChange={(event) => {
            setKeyTouched(true);
            setKey(event.target.value);
          }}
          placeholder="contact"
          className="mt-1 w-full rounded-md border border-line px-2.5 py-1.5 font-mono text-[13px]"
        />
        <p className="mt-1 text-[11.5px] text-muted">
          This is the URL a theme embeds. Two forms cannot share it.
        </p>
      </div>
      <p className="text-[11.5px] text-muted">
        Starts with a name and a message field. It is saved as a draft, so nothing is accepted
        until you publish it.
      </p>
      <div className="flex gap-2">
        <button
          type="button"
          data-forms-create-submit
          disabled={busy || name.trim() === ""}
          onClick={() => void submit()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Create form
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

function ConfirmDelete({
  form,
  busy,
  onCancel,
  onConfirm,
}: {
  form: Form;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="confirm-form-delete"
      data-forms-delete-confirm
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
    >
      <div className="w-full max-w-md space-y-3 rounded-lg border border-line bg-canvas p-5">
        <h2 id="confirm-form-delete" className="text-[14px] font-medium">
          Delete {form.name}?
        </h2>
        {/* The confirmation names what goes with it rather than asking "are you sure": the
            submissions are the record of what this form asked people, and they cascade. */}
        <p className="text-[12.5px] text-muted">
          The form and every submission it received are deleted. This cannot be undone.
        </p>
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
          >
            Cancel
          </button>
          <button
            type="button"
            data-forms-delete-confirm
            disabled={busy}
            onClick={onConfirm}
            className="inline-flex items-center gap-1.5 rounded-md border border-red-300 px-3 py-1.5 text-[12.5px] text-red-700 disabled:opacity-50 dark:border-red-800 dark:text-red-300"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
            Delete form and submissions
          </button>
        </div>
      </div>
    </div>
  );
}
