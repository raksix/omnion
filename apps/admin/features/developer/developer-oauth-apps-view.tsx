"use client";

/**
 * `/developer/oauth-apps` — the OAuth applications a developer registers for third parties to
 * sign into this platform with (REQ-033, slice 3).
 *
 * Six things on this screen are decisions rather than fields, and each is written down because
 * the wrong version of it is a bug nobody reports:
 *
 * - **The client secret is shown exactly once, in a dialog that says so.** The API has no
 *   endpoint that can return it again, so the dialog is the only chance. It states the
 *   consequence, offers a copy button with visible confirmation, and refuses to be dismissed
 *   without an acknowledgement — a developer who closes it by accident has lost a credential
 *   they cannot regenerate and has to go and rotate.
 * - **Rotation's overlap window is stated in days, not implied.** A client secret is normally
 *   deployed to more machines than anybody is tracking, so unlike an API key rotation — which
 *   kills the old secret immediately — this one leaves 7 days. The confirmation says so in as
 *   many words, because the person clicking it is the person who has to deploy to the rest.
 * - **Suspend and withdraw are two separate buttons, never one dropdown.** Suspending is
 *   reversible and safe; withdrawing is neither, and its row has to survive for the audit trail.
 *   A panel that offered both behind a single status dropdown would eventually have somebody
 *   withdraw an app they meant to pause, and the only way back would be to re-register and break
 *   every client holding the old client id.
 * - **The redirect URI list is a textarea, one URI per line, and its warning names the
 *   consequence.** Replacing the list invalidates every authorization in flight, so the edit form
 *   says that *before* the save button is reachable rather than after.
 * - **The scopes are chosen from a list, not typed.** A free-text scope box is how an app ends up
 *   holding a permission name that does not exist — which authenticates and then silently does
 *   nothing, or worse, does something.
 * - **`client_credentials` is offered and marked.** It means the app gets a token with no person
 *   in the flow, so every request it makes is attributed to the application rather than to a
 *   user. That is a real product decision, and the checkbox says which one you are making.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  AlertTriangle,
  Check,
  Copy,
  Info,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  RotateCw,
  Trash2,
  Zap,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  createOAuthApp,
  editOAuthApp,
  fetchOAuthApp,
  fetchOAuthApps,
  rotateOAuthAppSecret,
  setOAuthAppSuspended,
  withdrawOAuthApp,
  type CreateOAuthAppInput,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { OAuthAppDetailResponse, OAuthAppSummary, OAuthGrant } from "@/lib/types";

/**
 * The scopes the picker offers, with the plain-language consequence next to each.
 *
 * The same catalogue the API key screen offers, deliberately: a developer registering a
 * third-party app and a developer minting a key for their own integration are choosing the same
 * permission keys, and two different lists would mean two different answers to "what can this
 * hold".
 */
const SCOPE_CHOICES: { key: string; label: string }[] = [
  { key: "developer.keys.read", label: "Read keys, usage and the request log" },
  { key: "developer.keys.manage", label: "Create, rotate and revoke keys" },
  { key: "content.pages.read", label: "Read pages" },
  { key: "content.pages.create", label: "Create pages" },
  { key: "content.pages.update", label: "Edit pages" },
  { key: "content.pages.publish", label: "Publish and unpublish pages" },
  { key: "media.read", label: "Read media" },
  { key: "media.upload", label: "Upload media" },
  { key: "users.read", label: "Read users" },
  { key: "sites.read", label: "Read sites" },
];

/** The flows, each labelled with what it means for the person signing in — or not. */
const GRANT_CHOICES: { value: OAuthGrant; label: string; hint: string }[] = [
  {
    value: "authorization_code",
    label: "Authorization code",
    hint: "A person signs in and approves the app. The token is exchanged for their account.",
  },
  {
    value: "client_credentials",
    label: "Client credentials",
    hint: "No person signs in. Every request is attributed to the app, not to a user.",
  },
];

/** Name bounds, restated from the API so the form refuses before it posts. */
const NAME_MIN = 3;
const NAME_MAX = 60;

/** How many redirect URIs the API accepts on one app. */
const MAX_REDIRECT_URIS = 10;

/** How many scope chips a row shows before the `+N` overflow takes over. */
const SCOPE_CHIP_LIMIT = 2;

/** The form's own state, kept apart from the saved rows. */
type Draft = {
  name: string;
  description: string;
  /** One entry per line; the API requires each to be an absolute https/http URL. */
  redirectUris: string;
  scopes: string[];
  grants: OAuthGrant[];
};

const EMPTY_DRAFT: Draft = {
  name: "",
  description: "",
  redirectUris: "",
  scopes: [],
  grants: ["authorization_code"],
};

/** Which edit, if any, the form is currently performing. */
type Editing = { id: string; name: string; redirectUriCount: number } | null;

/** The one-time client secret, held only until the dialog is dismissed. */
type Minted = {
  secret: string;
  clientId: string;
  name: string;
  reason: "created" | "rotated";
  /** Days the previous secret keeps working. `0` on a creation. */
  overlapDays: number;
};

/** The action a confirmation is about. */
type Pending = { app: OAuthAppSummary; action: "rotate" | "suspend" | "resume" | "withdraw" };

/** Split a textarea into a URI list, dropping blank lines and trimming each. */
function parseUriLines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/** Shorten a scope key for a chip: `content.pages.publish` → `content.pages…`. */
function shortScope(scope: string): string {
  const parts = scope.split(".");
  return parts.length > 2 ? `${parts.slice(0, 2).join(".")}…` : scope;
}

/** Plain-language name for a flow, for the table cell and the dialog. */
function grantLabel(grant: OAuthGrant): string {
  return grant === "authorization_code" ? "Sign-in" : "Machine";
}

/** Copy to the clipboard, reporting success so the button is not a dead control. */
async function copyToClipboard(value: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    return false;
  }
}

export function DeveloperOAuthAppsView() {
  const [apps, setApps] = useState<OAuthAppSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const [formOpen, setFormOpen] = useState(false);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [editing, setEditing] = useState<Editing>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [busy, setBusy] = useState(false);

  const [minted, setMinted] = useState<Minted | null>(null);
  const [mintedCopied, setMintedCopied] = useState(false);
  const [mintedAcknowledged, setMintedAcknowledged] = useState(false);

  const [pending, setPending] = useState<Pending | null>(null);
  const [confirmText, setConfirmText] = useState("");

  const [filterStatus, setFilterStatus] = useState<"all" | OAuthAppSummary["status"]>("all");
  const [filterGrant, setFilterGrant] = useState<"all" | OAuthGrant>("all");
  const [filterName, setFilterName] = useState("");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [detail, setDetail] = useState<OAuthAppDetailResponse | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    let cancelled = false;
    setError(null);
    fetchOAuthApps()
      .then((response) => {
        if (!cancelled) {
          setApps(response.apps);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setApps([]);
          setError(
            cause instanceof ApiError ? cause.message : "The applications could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [reloadToken]);

  /**
   * The expanded row's detail, fetched on open rather than held.
   *
   * The summary deliberately does not carry the redirect URI list or the scopes — the detail is
   * the only screen that shows all of them, and fetching it on demand keeps the list payload small.
   * A second effect rather than a fetch inside the click handler, so a row that is expanded
   * again after a reload re-reads instead of showing a stale copy.
   */
  useEffect(() => {
    if (expanded === null) {
      setDetail(null);
      setDetailError(null);
      return;
    }
    let cancelled = false;
    setDetail(null);
    setDetailError(null);
    fetchOAuthApp(expanded).then(
      (response) => {
        if (!cancelled) {
          setDetail(response);
        }
      },
      (cause: unknown) => {
        if (!cancelled) {
          setDetailError(
            cause instanceof ApiError ? cause.message : "This application could not be loaded.",
          );
        }
      },
    );
    return () => {
      cancelled = true;
    };
  }, [expanded, reloadToken]);

  const visible = useMemo(() => {
    let rows = apps ?? [];
    if (filterStatus !== "all") {
      rows = rows.filter((app) => app.status === filterStatus);
    }
    if (filterGrant !== "all") {
      rows = rows.filter((app) => app.grant_types.includes(filterGrant));
    }
    const needle = filterName.trim().toLowerCase();
    if (needle.length > 0) {
      rows = rows.filter(
        (app) => app.name.toLowerCase().includes(needle) || app.client_id.toLowerCase().includes(needle),
      );
    }
    return rows;
  }, [apps, filterStatus, filterGrant, filterName]);

  const openCreate = () => {
    setDraft(EMPTY_DRAFT);
    setEditing(null);
    setFieldError(null);
    setNotice(null);
    setFormOpen(true);
  };

  const openEdit = async (app: OAuthAppSummary) => {
    setNotice(null);
    setFieldError(null);
    setEditing({ id: app.id, name: app.name, redirectUriCount: app.redirect_uri_count });
    setBusy(true);
    try {
      const response = await fetchOAuthApp(app.id);
      // The form is seeded from the *detail*, not from the summary: the summary has no redirect
      // URI list and no scope list, and a form that opened with those fields blank would look
      // like an app with no redirects — and then a save would blank them for real.
      setDraft({
        name: response.app.name,
        description: response.app.description ?? "",
        redirectUris: response.app.redirect_uris.join("\n"),
        scopes: [...response.app.scopes].sort(),
        grants: [...response.app.grant_types],
      });
      setFormOpen(true);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "This application could not be loaded.");
    } finally {
      setBusy(false);
    }
  };

  const toggleScope = (scope: string) => {
    setDraft((current) => ({
      ...current,
      // Sorted so two developers producing the same app from the same clicks send byte-identical
      // requests, which is what makes a diff in a bug report readable.
      scopes: current.scopes.includes(scope)
        ? current.scopes.filter((entry) => entry !== scope)
        : [...current.scopes, scope].sort(),
    }));
  };

  const toggleGrant = (grant: OAuthGrant) => {
    setDraft((current) => {
      const has = current.grants.includes(grant);
      // Unchecking the last grant would post an empty list, which the API refuses with
      // `no_grant_types` — a vaguer message than "tick a box". Refuse here instead, for the
      // same reason the scope list refuses empty.
      if (has && current.grants.length === 1) {
        return current;
      }
      return {
        ...current,
        grants: has
          ? current.grants.filter((entry) => entry !== grant)
          : ([...current.grants, grant].sort() as OAuthGrant[]),
      };
    });
  };

  const submitForm = async () => {
    const name = draft.name.trim();
    if (name.length < NAME_MIN || name.length > NAME_MAX) {
      setFieldError({
        field: "name",
        message: `A name must be between ${NAME_MIN} and ${NAME_MAX} characters.`,
      });
      return;
    }
    const uris = parseUriLines(draft.redirectUris);
    if (uris.length === 0) {
      setFieldError({
        field: "redirect_uris",
        message: "Register at least one redirect URI. An app with none could never receive a code.",
      });
      return;
    }
    if (uris.length > MAX_REDIRECT_URIS) {
      setFieldError({
        field: "redirect_uris",
        message: `Register at most ${MAX_REDIRECT_URIS} redirect URIs.`,
      });
      return;
    }
    if (draft.scopes.length === 0) {
      setFieldError({
        field: "scopes",
        message: "Choose at least one scope. An app with none could not do anything.",
      });
      return;
    }

    const description = draft.description.trim();
    const base = {
      redirect_uris: uris,
      scopes: [...draft.scopes].sort(),
      grant_types: [...draft.grants].sort(),
    };

    setBusy(true);
    setFieldError(null);
    try {
      if (editing) {
        await editOAuthApp(editing.id, {
          name,
          // `null` clears, an absent field leaves it alone. Sending the string when it is
          // present and `null` when the box is empty is what makes "remove the description" work
          // without a second control.
          description: description.length > 0 ? description : null,
          ...base,
        });
        setFormOpen(false);
        setEditing(null);
        setNotice(`${editing.name} was updated.`);
      } else {
        const input: CreateOAuthAppInput = {
          name,
          ...base,
          ...(description.length > 0 ? { description } : {}),
        };
        const created = await createOAuthApp(input);
        setFormOpen(false);
        // Opened from the *response*, and the response is the only place the secret exists.
        // Nothing is cached and nothing is refetched; closing the dialog is what discards it.
        setMinted({
          secret: created.client_secret,
          clientId: created.client_id,
          name: created.name,
          reason: "created",
          overlapDays: 0,
        });
        setMintedCopied(false);
        setMintedAcknowledged(false);
      }
      reload();
    } catch (cause: unknown) {
      if (cause instanceof ApiError && typeof cause.details?.field === "string") {
        setFieldError({ field: cause.details.field, message: cause.message });
      } else {
        setFieldError({
          field: "form",
          message: cause instanceof ApiError ? cause.message : "The application could not be saved.",
        });
      }
    } finally {
      setBusy(false);
    }
  };

  const runConfirmed = async () => {
    if (!pending) {
      return;
    }
    const { app, action } = pending;
    setBusy(true);
    setError(null);
    try {
      if (action === "rotate") {
        const rotated = await rotateOAuthAppSecret(app.id);
        setPending(null);
        setConfirmText("");
        setMinted({
          secret: rotated.client_secret,
          clientId: rotated.client_id,
          name: rotated.name,
          reason: "rotated",
          overlapDays: rotated.previous_secret_valid_for_days ?? 7,
        });
        setMintedCopied(false);
        setMintedAcknowledged(false);
        setNotice(
          `${app.name} was rotated. Its previous secret keeps working for ${
            rotated.previous_secret_valid_for_days ?? 7
          } more days.`,
        );
      } else if (action === "withdraw") {
        await withdrawOAuthApp(app.id);
        setPending(null);
        setConfirmText("");
        setNotice(
          `${app.name} was withdrawn. Its row and its audit trail stay; no client can sign in with it again.`,
        );
      } else {
        const suspended = action === "suspend";
        await setOAuthAppSuspended(app.id, suspended);
        setPending(null);
        setConfirmText("");
        setNotice(
          suspended
            ? `${app.name} is suspended. Sign-in requests are refused until you resume it.`
            : `${app.name} is active again.`,
        );
      }
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : `The application could not be updated.`);
      setPending(null);
    } finally {
      setBusy(false);
    }
  };

  const dismissMinted = () => {
    // Deliberately the *only* place the secret is dropped. There is nothing to re-fetch it
    // from, which is the whole design.
    setMinted(null);
    setMintedCopied(false);
    setMintedAcknowledged(false);
  };

  /**
   * Whether a row's button for `action` should be enabled.
   *
   * The rule is "never on a withdrawn app", and the three actions then differ: only an *active*
   * app can be suspended and only a *suspended* one can be resumed, which is why those two
   * branches exist rather than a single status check. Rotate and withdraw are available
   * whenever the app is not withdrawn — an app that is already suspended still has a secret that
   * may need replacing.
   */
  const canAct = (app: OAuthAppSummary, action: Pending["action"]): boolean => {
    if (app.status === "deleted") {
      return false;
    }
    if (action === "suspend") {
      return app.status === "active";
    }
    if (action === "resume") {
      return app.status === "suspended";
    }
    return true;
  };

  if (apps === null) {
    return error ? (
      <div className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <p className="text-[12.5px] text-accent-strong">{error}</p>
        <button
          type="button"
          onClick={reload}
          className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Try again
        </button>
      </div>
    ) : (
      <div className="flex items-center gap-2 px-1 py-8 text-[12.5px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden />
        Loading the applications…
      </div>
    );
  }

  const expandedApp = visible.find((app) => app.id === expanded) ?? null;

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <p
          role="alert"
          data-oauth-app-error
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          data-oauth-app-notice
          className="rounded-xl border border-positive/25 bg-positive-soft px-4 py-3 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[12.5px] text-muted">
          {apps.length === 0
            ? "No applications yet."
            : `${visible.length} of ${apps.length} application${apps.length === 1 ? "" : "s"}.`}
        </p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={reload}
            className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-oauth-app-create
            onClick={openCreate}
            className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
          >
            <Plus className="size-3.5" aria-hidden />
            Register an app
          </button>
        </div>
      </div>

      {/* The filters. Present even when there is nothing to filter: an empty table with no
          filters reads as "the screen is broken", while an empty table with filters reads as
          "try widening these". */}
      <section
        aria-label="Filters"
        data-oauth-app-filters
        className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface px-4 py-3"
      >
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Status
          <select
            value={filterStatus}
            onChange={(event) => setFilterStatus(event.target.value as typeof filterStatus)}
            data-oauth-app-filter-status
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="all">Any</option>
            <option value="active">Active</option>
            <option value="suspended">Suspended</option>
            <option value="deleted">Withdrawn</option>
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Flow
          <select
            value={filterGrant}
            onChange={(event) => setFilterGrant(event.target.value as typeof filterGrant)}
            data-oauth-app-filter-grant
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          >
            <option value="all">Any</option>
            <option value="authorization_code">Sign-in (authorization code)</option>
            <option value="client_credentials">Machine (client credentials)</option>
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px] text-muted">
          Name or client id
          <input
            value={filterName}
            onChange={(event) => setFilterName(event.target.value)}
            data-oauth-app-filter-name
            placeholder="support portal, cli_…"
            className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
          />
        </label>
      </section>

      {apps.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="Henüz uygulama yok"
            hint="An application lets a third-party client sign a person into this platform with OAuth, instead of asking them for a password. Registering one mints a client id and a client secret; the secret is shown once."
            action={
              <button
                type="button"
                onClick={openCreate}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white"
              >
                <Plus className="size-3.5" aria-hidden />
                Register the first app
              </button>
            }
          />
        </div>
      ) : visible.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title="No application matches these filters"
            hint="Widen the status or flow, or clear the name filter."
            action={
              <button
                type="button"
                onClick={() => {
                  setFilterStatus("all");
                  setFilterGrant("all");
                  setFilterName("");
                }}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px]"
              >
                Clear the filters
              </button>
            }
          />
        </div>
      ) : (
        <>
          {/* A table from `md` up, cards below it. Both carry the row hooks, because a harness
              that measures a table at 390px measures a hidden element and reports "the screen
              renders nothing". */}
          <div className="hidden overflow-x-auto rounded-xl border border-line bg-surface md:block">
            <table className="w-full text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-3 py-2 font-medium">Name</th>
                  <th className="px-3 py-2 font-medium">Client id</th>
                  <th className="px-3 py-2 font-medium">Redirect URIs</th>
                  <th className="px-3 py-2 font-medium">Flows</th>
                  <th className="px-3 py-2 font-medium">Scopes</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {visible.map((app) => (
                  <tr
                    key={app.id}
                    data-oauth-app-row
                    data-oauth-app-status={app.status}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="px-3 py-2">
                      <button
                        type="button"
                        onClick={() => setExpanded(expanded === app.id ? null : app.id)}
                        data-oauth-app-expand
                        aria-expanded={expanded === app.id}
                        className="text-left font-medium underline-offset-2 hover:underline"
                      >
                        {app.name}
                      </button>
                      {/* The one thing on this row that can be a *to-do* rather than a fact: an
                          operator who rotated and has not finished redeploying reads this. */}
                      {app.previous_secret_expires_at ? (
                        <p className="mt-0.5 text-[11px] text-caution">
                          Previous secret valid until {formatTimestamp(app.previous_secret_expires_at)}
                        </p>
                      ) : null}
                    </td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">{app.client_id}</td>
                    <td className="px-3 py-2 text-muted">
                      {app.redirect_uri_count === 0 ? (
                        "None"
                      ) : (
                        <>
                          <span className="font-medium text-ink">{app.redirect_uri_count}</span>
                          {app.redirect_uri_count === 1 ? " URI" : " URIs"}
                        </>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <span className="flex flex-wrap items-center gap-1">
                        {app.grant_types.map((grant) => (
                          <span
                            key={grant}
                            className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                          >
                            {grant === "client_credentials" ? (
                              <Zap className="size-2.5" aria-hidden />
                            ) : null}
                            {grantLabel(grant)}
                          </span>
                        ))}
                      </span>
                    </td>
                    <td className="px-3 py-2">
                      <button
                        type="button"
                        onClick={() => setExpanded(expanded === app.id ? null : app.id)}
                        data-oauth-app-scopes-open
                        className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                        title="Open this application to see its redirect URIs and scopes"
                      >
                        {`Details ›`}
                      </button>
                    </td>
                    <td className="px-3 py-2">
                      <StatusBadge status={app.status} />
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex items-center justify-end gap-1">
                        <button
                          type="button"
                          data-oauth-app-edit
                          disabled={app.status === "deleted"}
                          onClick={() => void openEdit(app)}
                          title={
                            app.status === "deleted"
                              ? "A withdrawn application cannot be edited."
                              : "Edit this application"
                          }
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <Pencil className="size-3.5" aria-hidden />
                          <span className="sr-only">Edit {app.name}</span>
                        </button>
                        <button
                          type="button"
                          data-oauth-app-rotate
                          disabled={!canAct(app, "rotate")}
                          onClick={() => {
                            setPending({ app, action: "rotate" });
                            setConfirmText("");
                          }}
                          title={
                            canAct(app, "rotate")
                              ? "Issue a new client secret"
                              : `A ${app.status} application cannot be rotated.`
                          }
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <RotateCw className="size-3.5" aria-hidden />
                          <span className="sr-only">Rotate the secret of {app.name}</span>
                        </button>
                        {app.status === "suspended" ? (
                          <button
                            type="button"
                            data-oauth-app-resume
                            disabled={!canAct(app, "resume")}
                            onClick={() => {
                              setPending({ app, action: "resume" });
                              setConfirmText("");
                            }}
                            title="Let this application sign people in again"
                            className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                          >
                            <Check className="size-3.5" aria-hidden />
                            <span className="sr-only">Resume {app.name}</span>
                          </button>
                        ) : (
                          <button
                            type="button"
                            data-oauth-app-suspend
                            disabled={!canAct(app, "suspend")}
                            onClick={() => {
                              setPending({ app, action: "suspend" });
                              setConfirmText("");
                            }}
                            title={
                              canAct(app, "suspend")
                                ? "Refuse new sign-in requests, reversibly"
                                : "Only an active application can be suspended."
                            }
                            className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                          >
                            <AlertTriangle className="size-3.5" aria-hidden />
                            <span className="sr-only">Suspend {app.name}</span>
                          </button>
                        )}
                        <button
                          type="button"
                          data-oauth-app-withdraw
                          disabled={!canAct(app, "withdraw")}
                          onClick={() => {
                            setPending({ app, action: "withdraw" });
                            setConfirmText("");
                          }}
                          title={
                            canAct(app, "withdraw")
                              ? "Withdraw this application for good"
                              : "This application is already withdrawn."
                          }
                          className="inline-flex min-h-9 min-w-9 items-center justify-center rounded-lg border border-line px-2 transition hover:bg-canvas disabled:cursor-not-allowed disabled:opacity-40"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                          <span className="sr-only">Withdraw {app.name}</span>
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="flex flex-col gap-2 md:hidden">
            {visible.map((app) => (
              <article
                key={app.id}
                data-oauth-app-card
                data-oauth-app-status={app.status}
                className="rounded-xl border border-line bg-surface px-4 py-3"
              >
                <div className="flex items-start justify-between gap-2">
                  <button
                    type="button"
                    onClick={() => setExpanded(expanded === app.id ? null : app.id)}
                    data-oauth-app-expand
                    aria-expanded={expanded === app.id}
                    className="text-left text-[13.5px] font-medium underline-offset-2 hover:underline"
                  >
                    {app.name}
                  </button>
                  <StatusBadge status={app.status} />
                </div>
                <p className="mt-1 break-all font-mono text-[11.5px] text-muted">{app.client_id}</p>
                {app.previous_secret_expires_at ? (
                  <p className="mt-1 text-[11px] text-caution">
                    Previous secret valid until {formatTimestamp(app.previous_secret_expires_at)}
                  </p>
                ) : null}
                <p className="mt-1 text-[11.5px] text-muted">
                  {app.redirect_uri_count} redirect URI{app.redirect_uri_count === 1 ? "" : "s"} ·{" "}
                  {app.grant_types.map(grantLabel).join(", ")}
                </p>
                <div className="mt-2 flex flex-wrap items-center gap-1">
                  <button
                    type="button"
                    data-oauth-app-edit
                    disabled={app.status === "deleted"}
                    onClick={() => void openEdit(app)}
                    className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] disabled:opacity-40"
                  >
                    <Pencil className="size-3" aria-hidden />
                    Edit
                  </button>
                  <button
                    type="button"
                    data-oauth-app-rotate
                    disabled={!canAct(app, "rotate")}
                    onClick={() => {
                      setPending({ app, action: "rotate" });
                      setConfirmText("");
                    }}
                    className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] disabled:opacity-40"
                  >
                    <RotateCw className="size-3" aria-hidden />
                    Rotate
                  </button>
                  {app.status === "suspended" ? (
                    <button
                      type="button"
                      data-oauth-app-resume
                      disabled={!canAct(app, "resume")}
                      onClick={() => {
                        setPending({ app, action: "resume" });
                        setConfirmText("");
                      }}
                      className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] disabled:opacity-40"
                    >
                      <Check className="size-3" aria-hidden />
                      Resume
                    </button>
                  ) : (
                    <button
                      type="button"
                      data-oauth-app-suspend
                      disabled={!canAct(app, "suspend")}
                      onClick={() => {
                        setPending({ app, action: "suspend" });
                        setConfirmText("");
                      }}
                      className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] disabled:opacity-40"
                    >
                      <AlertTriangle className="size-3" aria-hidden />
                      Suspend
                    </button>
                  )}
                  <button
                    type="button"
                    data-oauth-app-withdraw
                    disabled={!canAct(app, "withdraw")}
                    onClick={() => {
                      setPending({ app, action: "withdraw" });
                      setConfirmText("");
                    }}
                    className="inline-flex min-h-9 items-center gap-1 rounded-lg border border-line px-2 text-[11.5px] disabled:opacity-40"
                  >
                    <Trash2 className="size-3" aria-hidden />
                    Withdraw
                  </button>
                </div>
              </article>
            ))}
          </div>

          {/* The expanded row's detail. A panel rather than a route: the list row's link target
              would need the app id in the URL, and a detail screen whose only entrance is a row
              somebody has to remember to click is a detail screen nobody opens. */}
          {expandedApp ? (
            <section
              aria-label={`${expandedApp.name} details`}
              data-oauth-app-detail
              className="rounded-xl border border-line bg-surface px-4 py-3"
            >
              <div className="flex flex-wrap items-center justify-between gap-2">
                <h2 className="text-[13px] font-medium">{expandedApp.name}</h2>
                <button
                  type="button"
                  data-oauth-app-detail-close
                  onClick={() => setExpanded(null)}
                  className="min-h-9 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-canvas"
                >
                  Close
                </button>
              </div>
              {detailError ? (
                <p role="alert" data-oauth-app-detail-error className="mt-2 text-[12.5px] text-accent-strong">
                  {detailError}
                </p>
              ) : detail === null ? (
                <p className="mt-2 flex items-center gap-2 text-[12.5px] text-muted">
                  <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  Loading this application…
                </p>
              ) : (
                <div className="mt-3 grid gap-4 md:grid-cols-2">
                  <div className="flex flex-col gap-1.5">
                    <p className="text-[11.5px] uppercase tracking-wide text-muted">Client id</p>
                    <code
                      data-oauth-app-detail-client-id
                      className="block break-all rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[11.5px]"
                    >
                      {detail.app.client_id}
                    </code>
                    <p className="mt-2 text-[11.5px] uppercase tracking-wide text-muted">
                      Description
                    </p>
                    <p data-oauth-app-detail-description className="text-[12.5px] text-ink">
                      {detail.app.description || "None — this is what the consent screen shows."}
                    </p>
                    <p className="mt-2 text-[11.5px] uppercase tracking-wide text-muted">
                      Live authorization codes
                    </p>
                    <p data-oauth-app-detail-live-codes className="text-[12.5px] text-ink">
                      {detail.live_authorization_codes === 0
                        ? "None redeemable right now."
                        : `${detail.live_authorization_codes} still redeemable. A code is single-use and short-lived.`}
                    </p>
                    <p className="mt-2 text-[11.5px] text-muted">
                      Registered {formatTimestamp(detail.app.created_at)} · last edited{" "}
                      {formatTimestamp(detail.app.updated_at)}
                    </p>
                  </div>
                  <div className="flex flex-col gap-1.5">
                    <p className="text-[11.5px] uppercase tracking-wide text-muted">Redirect URIs</p>
                    <ul data-oauth-app-detail-uris className="flex flex-col gap-1">
                      {detail.app.redirect_uris.map((uri) => (
                        <li
                          key={uri}
                          className="break-all rounded-lg border border-line bg-canvas px-2.5 py-1 font-mono text-[11.5px]"
                        >
                          {uri}
                        </li>
                      ))}
                    </ul>
                    <p className="mt-2 text-[11.5px] uppercase tracking-wide text-muted">Scopes</p>
                    <p data-oauth-app-detail-scopes className="flex flex-wrap gap-1">
                      {detail.app.scopes.map((scope) => (
                        <span
                          key={scope}
                          className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                        >
                          {scope}
                        </span>
                      ))}
                    </p>
                    <p className="mt-2 text-[11.5px] uppercase tracking-wide text-muted">Flows</p>
                    <p data-oauth-app-detail-grants className="flex flex-wrap gap-1">
                      {detail.app.grant_types.map((grant) => (
                        <span
                          key={grant}
                          className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                        >
                          {grant === "client_credentials" ? <Zap className="size-2.5" aria-hidden /> : null}
                          {GRANT_CHOICES.find((choice) => choice.value === grant)?.label ?? grant}
                        </span>
                      ))}
                    </p>
                  </div>
                </div>
              )}
            </section>
          ) : null}
        </>
      )}

      {/* The register / edit form. One form for both, because the fields are the same fields and
          an edit dialog that dropped the redirect URI box would be the quietest possible way to
          lose them. */}
      {formOpen ? (
        <section
          aria-label={editing ? "Edit application" : "Register an application"}
          data-oauth-app-form
          className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4"
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="text-[13px] font-medium">
              {editing ? `Edit ${editing.name}` : "Register an application"}
            </h2>
            <button
              type="button"
              data-oauth-app-form-cancel
              onClick={() => {
                setFormOpen(false);
                setEditing(null);
                setFieldError(null);
              }}
              className="min-h-9 rounded-lg border border-line px-2.5 text-[12px] transition hover:bg-canvas"
            >
              Cancel
            </button>
          </div>

          <div className="grid gap-3 md:grid-cols-2">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Name
              <input
                value={draft.name}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                data-oauth-app-name
                placeholder="Support portal"
                className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Description — shown on the consent screen
              <input
                value={draft.description}
                onChange={(event) => setDraft({ ...draft, description: event.target.value })}
                data-oauth-app-description
                placeholder="Lets our support agents read a user's pages"
                className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px] text-ink"
              />
            </label>
          </div>

          <label className="flex flex-col gap-1 text-[12px] text-muted">
            Redirect URIs — one per line, absolute
            <textarea
              value={draft.redirectUris}
              onChange={(event) => setDraft({ ...draft, redirectUris: event.target.value })}
              data-oauth-app-redirect-uris
              rows={3}
              placeholder={"https://app.example.com/oauth/callback\nhttp://localhost:5173/callback"}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 font-mono text-[12px] text-ink"
            />
          </label>
          {editing && editing.redirectUriCount > 0 ? (
            <p className="flex items-start gap-1.5 text-[11.5px] text-caution" data-oauth-app-uris-warning>
              <AlertTriangle className="mt-px size-3 shrink-0" aria-hidden />
              Changing this list invalidates every authorization in flight: a user who is halfway
              through signing in will be sent back to a redirect URI that is no longer registered.
            </p>
          ) : null}
          {fieldError?.field === "redirect_uris" ? (
            <p role="alert" data-oauth-app-redirect-uris-error className="text-[12px] text-accent-strong">
              {fieldError.message}
            </p>
          ) : null}
          {fieldError?.field === "name" ? (
            <p role="alert" data-oauth-app-name-error className="text-[12px] text-accent-strong">
              {fieldError.message}
            </p>
          ) : null}

          <fieldset className="flex flex-col gap-1.5">
            <legend className="text-[12.5px] font-medium">Scopes</legend>
            <p className="text-[11.5px] text-muted">
              The most this app may ever be granted. A user still approves each one at sign-in.
            </p>
            <div className="flex flex-wrap gap-1.5">
              {SCOPE_CHOICES.map((choice) => (
                <button
                  key={choice.key}
                  type="button"
                  onClick={() => toggleScope(choice.key)}
                  data-oauth-app-scope={choice.key}
                  aria-pressed={draft.scopes.includes(choice.key)}
                  title={choice.label}
                  className={`min-h-9 rounded-full border px-2.5 text-[11.5px] transition ${
                    draft.scopes.includes(choice.key)
                      ? "border-accent bg-accent-soft text-accent-strong"
                      : "border-line text-muted hover:bg-canvas"
                  }`}
                >
                  {choice.label}
                </button>
              ))}
            </div>
            {fieldError?.field === "scopes" ? (
              <p role="alert" data-oauth-app-scopes-error className="text-[12px] text-accent-strong">
                {fieldError.message}
              </p>
            ) : null}
          </fieldset>

          <fieldset className="flex flex-col gap-1.5">
            <legend className="text-[12.5px] font-medium">Flows</legend>
            <p className="text-[11.5px] text-muted">
              An unchecked box here means a client that can never complete the flow you did not
              select. Both are safe to enable; the second removes the person from the flow
              entirely.
            </p>
            {GRANT_CHOICES.map((choice) => {
              const on = draft.grants.includes(choice.value);
              const onlyOne = on && draft.grants.length === 1;
              return (
                <label key={choice.value} className="flex items-start gap-2 text-[12.5px]">
                  <input
                    type="checkbox"
                    checked={on}
                    onChange={() => toggleGrant(choice.value)}
                    data-oauth-app-grant={choice.value}
                    disabled={onlyOne}
                    title={onlyOne ? "An application needs at least one flow." : undefined}
                    className="mt-0.5 size-3.5"
                  />
                  <span>
                    <span className="font-medium text-ink">{choice.label}</span>
                    <span className="block text-[11.5px] text-muted">{choice.hint}</span>
                  </span>
                </label>
              );
            })}
            {fieldError?.field === "grant_types" ? (
              <p role="alert" className="text-[12px] text-accent-strong">
                {fieldError.message}
              </p>
            ) : null}
          </fieldset>

          {fieldError?.field === "form" ? (
            <p role="alert" data-oauth-app-form-error className="text-[12px] text-accent-strong">
              {fieldError.message}
            </p>
          ) : null}

          <div className="flex items-center gap-2">
            <button
              type="button"
              data-oauth-app-submit
              disabled={busy}
              onClick={submitForm}
              className="inline-flex min-h-9 items-center gap-1.5 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-50"
            >
              {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
              {editing ? "Save the changes" : "Register and show the secret"}
            </button>
            <button
              type="button"
              onClick={() => {
                setFormOpen(false);
                setEditing(null);
                setFieldError(null);
              }}
              className="min-h-9 rounded-lg border border-line px-3 text-[12.5px]"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      {/* The one-time client secret. Not dismissible by clicking the backdrop — only by the
          button, and only after the acknowledgement, because the alternative is a lost credential
          the developer has to go and rotate. */}
      {minted ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label="Your new client secret"
          data-oauth-app-secret-dialog
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
        >
          <div className="flex w-full max-w-lg flex-col gap-3 rounded-xl border border-line bg-surface px-5 py-4">
            <div className="flex items-center gap-2">
              <AlertTriangle className="size-4 text-caution" aria-hidden />
              <h2 className="text-[13.5px] font-medium">
                {minted.reason === "created" ? "Your app is registered" : "Your secret was rotated"}
              </h2>
            </div>
            <p className="text-[12.5px] text-muted">
              <span className="font-medium text-ink">{minted.name}</span> is live as{" "}
              <span className="font-mono text-[11.5px] text-ink">{minted.clientId}</span>. Copy the
              client secret now — the platform stores only a one-way hash and cannot show it again.
              {minted.reason === "rotated" && minted.overlapDays > 0 ? (
                <>
                  {" "}
                  The previous secret keeps working for {minted.overlapDays} days, so you can deploy
                  the new one everywhere before the old one dies.
                </>
              ) : (
                " If you lose it, rotate the app and use the new secret."
              )}
            </p>
            <code
              data-oauth-app-secret
              className="block overflow-x-auto rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px]"
            >
              {minted.secret}
            </code>
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                data-oauth-app-copy
                onClick={async () => {
                  const copied = await copyToClipboard(minted.secret);
                  setMintedCopied(copied);
                }}
                className="inline-flex min-h-9 items-center gap-1.5 rounded-lg border border-line px-3 text-[12.5px] transition hover:bg-canvas"
              >
                {mintedCopied ? (
                  <Check className="size-3.5 text-positive" aria-hidden />
                ) : (
                  <Copy className="size-3.5" aria-hidden />
                )}
                {mintedCopied ? "Copied" : "Copy the secret"}
              </button>
              <label className="flex items-center gap-2 text-[11.5px] text-muted">
                <input
                  type="checkbox"
                  checked={mintedAcknowledged}
                  onChange={(event) => setMintedAcknowledged(event.target.checked)}
                  data-oauth-app-acknowledged
                  className="size-3.5"
                />
                I have copied it somewhere safe.
              </label>
            </div>
            <div className="flex justify-end">
              <button
                type="button"
                data-oauth-app-secret-done
                disabled={!mintedAcknowledged}
                onClick={dismissMinted}
                className="min-h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-40"
              >
                Done
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {/* Rotate, suspend, resume and withdraw confirmations. Rotation states the overlap;
          withdrawal demands a typed name because it is the one action here that cannot be
          undone. */}
      {pending ? (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={
            pending.action === "rotate"
              ? "Rotate client secret"
              : pending.action === "withdraw"
                ? "Withdraw application"
                : pending.action === "suspend"
                  ? "Suspend application"
                  : "Resume application"
          }
          data-oauth-app-confirm
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4"
        >
          <div className="flex w-full max-w-md flex-col gap-3 rounded-xl border border-line bg-surface px-5 py-4">
            <h2 className="text-[13.5px] font-medium">
              {pending.action === "rotate"
                ? "Rotate this client secret?"
                : pending.action === "withdraw"
                  ? "Withdraw this application?"
                  : pending.action === "suspend"
                    ? "Suspend this application?"
                    : "Resume this application?"}
            </h2>
            <p className="text-[12.5px] text-muted">
              {pending.action === "rotate" ? (
                <>
                  <span className="font-medium text-ink">{pending.app.name}</span> gets a new client
                  secret, shown once. The previous secret keeps working for 7 days, which is how
                  long you have to deploy the new one everywhere. After that every client still
                  using the old secret starts failing.
                </>
              ) : pending.action === "withdraw" ? (
                <>
                  <span className="font-medium text-ink">{pending.app.name}</span> will refuse every
                  sign-in and token request from now on, and the client id cannot be used again —
                  re-registering mints a new one and breaks every client holding the old. The row
                  and its audit trail stay, so you can still see what this integration did. This
                  cannot be undone.
                </>
              ) : pending.action === "suspend" ? (
                <>
                  <span className="font-medium text-ink">{pending.app.name}</span> stops accepting
                  new sign-in requests. Tokens it already holds keep working until they expire.
                  Resume it whenever you like — this is the reversible one.
                </>
              ) : (
                <>
                  <span className="font-medium text-ink">{pending.app.name}</span> will accept sign-in
                  requests again. Nothing else about it changed.
                </>
              )}
            </p>
            {pending.action === "withdraw" ? (
              <label className="flex flex-col gap-1 text-[12px]">
                Type <span className="font-medium text-ink">{pending.app.name}</span> to confirm
                <input
                  value={confirmText}
                  onChange={(event) => setConfirmText(event.target.value)}
                  data-oauth-app-confirm-input
                  className="min-h-9 rounded-lg border border-line bg-canvas px-2 text-[12.5px]"
                />
              </label>
            ) : null}
            <div className="flex flex-wrap items-center justify-end gap-2">
              <button
                type="button"
                onClick={() => {
                  setPending(null);
                  setConfirmText("");
                }}
                className="min-h-9 rounded-lg border border-line px-3 text-[12.5px]"
              >
                Cancel
              </button>
              <button
                type="button"
                data-oauth-app-confirm-submit
                disabled={
                  busy || (pending.action === "withdraw" && confirmText !== pending.app.name)
                }
                onClick={runConfirmed}
                className="min-h-9 rounded-lg bg-accent px-3 text-[12.5px] font-medium text-white disabled:opacity-40"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
                {pending.action === "rotate"
                  ? "Rotate and show the new secret"
                  : pending.action === "withdraw"
                    ? "Withdraw for good"
                    : pending.action === "suspend"
                      ? "Suspend"
                      : "Resume"}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      <p className="flex items-start gap-1.5 text-[11.5px] text-muted">
        <Info className="mt-px size-3 shrink-0" aria-hidden />
        A withdrawn application keeps its row so its audit trail stays readable, but its client id
        and secret stop working and cannot be revived. Suspend instead if you only need it switched
        off for a while.
      </p>
    </div>
  );
}
