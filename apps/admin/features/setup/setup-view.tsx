"use client";

/**
 * First-run setup wizard (REQ-050).
 *
 * Six screens in the documented order — owner account → organization → first site → theme →
 * (optional) AI provider → done — driven entirely by the server: every answer comes back as a
 * fresh onboarding status, so a refresh resumes at the first open step and a second tab cannot
 * drift out of sync. The wizard works signed-out for its first step (there is no account yet to
 * sign in with) and adopts the session the API hands back with the owner account.
 */
import { useEffect, useState, type FormEvent, type ReactNode } from "react";

import {
  ArrowRight,
  Check,
  Circle,
  Globe,
  LayoutGrid,
  Palette,
  Sparkles,
  UserRound,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import {
  ApiError,
  completeOnboarding,
  createOnboardingOrganization,
  createOnboardingSite,
  createOwnerAccount,
  fetchOnboarding,
  setOnboardingTheme,
  skipAiProvider,
} from "@/lib/api";
import { useSession } from "@/lib/session";
import type { OnboardingStatus } from "@/lib/types";

/** Password policy from the identity crate (crates/identity/src/password.rs). */
const MIN_PASSWORD_LENGTH = 10;

/** The wizard's screens, in order. */
const STEPS = [
  {
    key: "owner",
    title: "Owner account",
    hint: "The account that runs this installation",
    icon: UserRound,
  },
  {
    key: "organization",
    title: "Organization",
    hint: "Sites, content and members live in a tenant",
    icon: LayoutGrid,
  },
  {
    key: "site",
    title: "First site",
    hint: "The property your visitors read",
    icon: Globe,
  },
  { key: "theme", title: "Theme", hint: "How the site looks", icon: Palette },
  {
    key: "ai",
    title: "AI provider",
    hint: "Optional — connections arrive with the AI Hub",
    icon: Sparkles,
  },
] as const;

const inputClass =
  "w-full rounded-lg border border-line bg-surface px-3 py-2 text-[13px] text-ink outline-none transition placeholder:text-muted/70 focus:border-accent focus:ring-2 focus:ring-accent/15";

const buttonClass =
  "inline-flex items-center gap-2 rounded-lg bg-accent px-3.5 py-2.5 text-[13px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:opacity-60";

/** Index of the first open step; `STEPS.length` when everything is done. */
function firstOpenStep(status: OnboardingStatus): number {
  const index = STEPS.findIndex((step) => !status.steps[step.key]);
  return index === -1 ? STEPS.length : index;
}

/** One labelled form field — every input carries a visible label, never a placeholder alone. */
function Field({
  id,
  label,
  hint,
  children,
}: {
  id: string;
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={id} className="text-[12px] font-medium">
        {label}
      </label>
      {children}
      {hint ? <p className="text-[11.5px] text-muted">{hint}</p> : null}
    </div>
  );
}

/** A message shown above the action row. */
function Notice({ tone, children }: { tone: "error" | "info"; children: ReactNode }) {
  const className =
    tone === "error"
      ? "border-accent/30 bg-accent-soft text-accent-strong"
      : "border-line bg-canvas text-muted";
  return (
    <p role={tone === "error" ? "alert" : undefined} className={`rounded-lg border px-3 py-2 text-[12px] ${className}`}>
      {children}
    </p>
  );
}

function messageOf(cause: unknown, fallback: string): string {
  return cause instanceof ApiError ? cause.message : fallback;
}

/** The wizard. */
export function SetupView() {
  const router = useRouter();
  const { user, adoptUser } = useSession();

  const [status, setStatus] = useState<OnboardingStatus | null>(null);
  const [step, setStep] = useState(0);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [ownerName, setOwnerName] = useState("");
  const [ownerEmail, setOwnerEmail] = useState("");
  const [ownerPassword, setOwnerPassword] = useState("");
  const [organizationName, setOrganizationName] = useState("");
  const [organizationSlug, setOrganizationSlug] = useState("");
  const [siteName, setSiteName] = useState("");
  const [siteKey, setSiteKey] = useState("");
  const [domain, setDomain] = useState("");
  const [theme, setTheme] = useState("");

  useEffect(() => {
    let cancelled = false;
    fetchOnboarding()
      .then((next) => {
        if (cancelled) {
          return;
        }
        setStatus(next);
        setStep(firstOpenStep(next));
        setTheme(next.summary.site_theme ?? next.themes[0]?.key ?? "minimal");
        if (next.completed) {
          router.replace("/");
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(messageOf(cause, "The setup state could not be loaded."));
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [router]);

  /** Run one wizard action and adopt the status the server answers with. */
  const run = async (action: () => Promise<OnboardingStatus>): Promise<OnboardingStatus | null> => {
    setBusy(true);
    setError(null);
    try {
      const next = await action();
      setStatus(next);
      setStep(firstOpenStep(next));
      return next;
    } catch (cause: unknown) {
      setError(messageOf(cause, "The step could not be completed."));
      return null;
    } finally {
      setBusy(false);
    }
  };

  const submitOwner = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await createOwnerAccount({
        displayName: ownerName,
        email: ownerEmail,
        password: ownerPassword,
      });
      adoptUser(result.user);
      setStatus(result.onboarding);
      setStep(firstOpenStep(result.onboarding));
    } catch (cause: unknown) {
      setError(messageOf(cause, "The owner account could not be created."));
    } finally {
      setBusy(false);
    }
  };

  const submitOrganization = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    await run(() => createOnboardingOrganization(organizationName, organizationSlug));
  };

  const submitSite = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    await run(() => createOnboardingSite(siteName, siteKey, domain));
  };

  const chooseTheme = async () => {
    await run(() => setOnboardingTheme(theme));
  };

  const finish = async () => {
    const skipped = await run(() => skipAiProvider());
    if (skipped) {
      await run(() => completeOnboarding());
    }
  };

  if (loading) {
    return (
      <main className="flex min-h-screen items-center justify-center px-4 text-[13px] text-muted">
        Loading the setup state…
      </main>
    );
  }

  const current = STEPS[Math.min(step, STEPS.length - 1)];
  const done = status?.completed ?? false;

  return (
    <main className="flex min-h-screen items-start justify-center bg-canvas px-4 py-10">
      <div className="w-full max-w-3xl">
        <header className="mb-6 flex items-center gap-2.5">
          <span
            aria-hidden
            className="flex size-9 items-center justify-center rounded-xl bg-accent text-sm font-semibold text-white"
          >
            O
          </span>
          <span className="flex flex-col leading-tight">
            <span className="text-[15px] font-semibold">Omnion</span>
            <span className="text-[11px] text-muted">First-run setup</span>
          </span>
          <span className="ml-auto text-[11.5px] text-muted" data-setup-progress={step}>
            Step {Math.min(step + 1, STEPS.length)} of {STEPS.length}
          </span>
        </header>

        <div className="grid gap-6 md:grid-cols-[230px_1fr]">
          <ol className="flex flex-col gap-1" aria-label="Setup steps">
            {STEPS.map((item, index) => {
              const finished = status ? status.steps[item.key] : false;
              const active = index === step && !done;
              const Icon = item.icon;
              return (
                <li
                  key={item.key}
                  data-setup-step={item.key}
                  data-step-state={finished ? "done" : active ? "current" : "pending"}
                  aria-current={active ? "step" : undefined}
                  className={`flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-[12.5px] ${
                    active ? "bg-accent-soft font-medium text-accent-strong" : "text-muted"
                  }`}
                >
                  <span
                    aria-hidden
                    className={`flex size-7 shrink-0 items-center justify-center rounded-lg border ${
                      finished
                        ? "border-positive/30 bg-positive-soft text-positive"
                        : "border-line bg-surface"
                    }`}
                  >
                    {finished ? <Check className="size-3.5" /> : <Icon className="size-3.5" />}
                  </span>
                  <span className="flex min-w-0 flex-col leading-tight">
                    <span className="truncate">{item.title}</span>
                    <span className="truncate text-[11px] text-muted">{item.hint}</span>
                  </span>
                </li>
              );
            })}
          </ol>

          <section className="rounded-2xl border border-line bg-surface p-6">
            {error ? (
              <div className="mb-4">
                <Notice tone="error">{error}</Notice>
              </div>
            ) : null}

            {!status ? (
              <Notice tone="info">The setup state could not be loaded. Refresh the page to retry.</Notice>
            ) : done || step >= STEPS.length ? (
              <div className="flex flex-col gap-4">
                <div className="flex items-center gap-2.5">
                  <span
                    aria-hidden
                    className="flex size-8 items-center justify-center rounded-full bg-positive-soft text-positive"
                  >
                    <Check className="size-4" />
                  </span>
                  <h1 className="text-[17px] font-semibold">Your installation is ready</h1>
                </div>
                <dl className="flex flex-col gap-3 text-[13px]">
                  <div className="flex items-center justify-between gap-4">
                    <dt className="text-muted">Organization</dt>
                    <dd className="truncate font-medium">
                      {status.summary.organization_name ?? "—"}
                    </dd>
                  </div>
                  <div className="flex items-center justify-between gap-4">
                    <dt className="text-muted">Site</dt>
                    <dd className="truncate font-medium">{status.summary.site_name ?? "—"}</dd>
                  </div>
                  <div className="flex items-center justify-between gap-4">
                    <dt className="text-muted">Theme</dt>
                    <dd className="truncate font-medium">{status.summary.site_theme ?? "—"}</dd>
                  </div>
                </dl>
                <p className="text-[12.5px] text-muted">
                  Next: publish your first page and connect a domain. The dashboard keeps a
                  getting-started list for the rest.
                </p>
                <div>
                  <button type="button" className={buttonClass} onClick={() => router.push("/")}>
                    Open the panel
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                </div>
              </div>
            ) : !status.needs_setup && !user ? (
              <div className="flex flex-col gap-4">
                <h1 className="text-[17px] font-semibold">Sign in to continue</h1>
                <p className="text-[12.5px] text-muted">
                  This installation already has accounts and its first run is still open. Sign in
                  with the owner account to finish it.
                </p>
                <Link href="/login" className={buttonClass}>
                  Go to sign in
                  <ArrowRight className="size-3.5" aria-hidden />
                </Link>
              </div>
            ) : current.key === "owner" ? (
              <form onSubmit={submitOwner} className="flex flex-col gap-4" noValidate>
                <div>
                  <h1 className="text-[17px] font-semibold">Create the owner account</h1>
                  <p className="mt-1 text-[12.5px] text-muted">
                    The first account owns this installation: it holds the Owner role, and every
                    later account, role and site hangs off it.
                  </p>
                </div>
                <Field id="setup-owner-name" label="Display name">
                  <input
                    id="setup-owner-name"
                    name="display_name"
                    className={inputClass}
                    value={ownerName}
                    onChange={(event) => setOwnerName(event.target.value)}
                    placeholder="Ada Lovelace"
                    autoComplete="name"
                    required
                  />
                </Field>
                <Field id="setup-owner-email" label="Email">
                  <input
                    id="setup-owner-email"
                    name="email"
                    type="email"
                    className={inputClass}
                    value={ownerEmail}
                    onChange={(event) => setOwnerEmail(event.target.value)}
                    placeholder="owner@example.com"
                    autoComplete="username"
                    required
                  />
                </Field>
                <Field
                  id="setup-owner-password"
                  label="Password"
                  hint={`At least ${MIN_PASSWORD_LENGTH} characters; stored hashed (Argon2id).`}
                >
                  <input
                    id="setup-owner-password"
                    name="password"
                    type="password"
                    className={inputClass}
                    value={ownerPassword}
                    onChange={(event) => setOwnerPassword(event.target.value)}
                    autoComplete="new-password"
                    required
                    minLength={MIN_PASSWORD_LENGTH}
                  />
                </Field>
                <div>
                  <button type="submit" className={buttonClass} disabled={busy}>
                    {busy ? "Creating…" : "Create account"}
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                </div>
              </form>
            ) : current.key === "organization" ? (
              <form onSubmit={submitOrganization} className="flex flex-col gap-4" noValidate>
                <div>
                  <h1 className="text-[17px] font-semibold">Create your organization</h1>
                  <p className="mt-1 text-[12.5px] text-muted">
                    The tenant boundary of the platform: sites, content, members and settings live
                    inside it.
                  </p>
                </div>
                <Field id="setup-org-name" label="Organization name">
                  <input
                    id="setup-org-name"
                    name="organization_name"
                    className={inputClass}
                    value={organizationName}
                    onChange={(event) => setOrganizationName(event.target.value)}
                    placeholder="Acme Corporation"
                    required
                  />
                </Field>
                <Field
                  id="setup-org-slug"
                  label="Slug (optional)"
                  hint="Lowercase letters, digits and dashes. Derived from the name when left empty."
                >
                  <input
                    id="setup-org-slug"
                    name="organization_slug"
                    className={inputClass}
                    value={organizationSlug}
                    onChange={(event) => setOrganizationSlug(event.target.value)}
                    placeholder="acme"
                  />
                </Field>
                <div>
                  <button type="submit" className={buttonClass} disabled={busy}>
                    {busy ? "Creating…" : "Create organization"}
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                </div>
              </form>
            ) : current.key === "site" ? (
              <form onSubmit={submitSite} className="flex flex-col gap-4" noValidate>
                <div>
                  <h1 className="text-[17px] font-semibold">Create your first site</h1>
                  <p className="mt-1 text-[12.5px] text-muted">
                    A site is what your visitors read; a domain points it at an address.
                  </p>
                </div>
                <Field id="setup-site-name" label="Site name">
                  <input
                    id="setup-site-name"
                    name="site_name"
                    className={inputClass}
                    value={siteName}
                    onChange={(event) => setSiteName(event.target.value)}
                    placeholder="Acme"
                    required
                  />
                </Field>
                <Field
                  id="setup-site-key"
                  label="Site key (optional)"
                  hint="Stable handle used by the API. Derived from the name when left empty."
                >
                  <input
                    id="setup-site-key"
                    name="site_key"
                    className={inputClass}
                    value={siteKey}
                    onChange={(event) => setSiteKey(event.target.value)}
                    placeholder="main"
                  />
                </Field>
                <Field
                  id="setup-site-domain"
                  label="Primary domain (optional)"
                  hint="You can bind the host later from Sites."
                >
                  <input
                    id="setup-site-domain"
                    name="domain"
                    className={inputClass}
                    value={domain}
                    onChange={(event) => setDomain(event.target.value)}
                    placeholder="acme.example.com"
                  />
                </Field>
                <div>
                  <button type="submit" className={buttonClass} disabled={busy}>
                    {busy ? "Creating…" : "Create site"}
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                </div>
              </form>
            ) : current.key === "theme" ? (
              <div className="flex flex-col gap-4">
                <div>
                  <h1 className="text-[17px] font-semibold">Choose a theme</h1>
                  <p className="mt-1 text-[12.5px] text-muted">
                    The theme decides how your site looks. You can change it any time from Sites.
                  </p>
                </div>
                <div className="flex flex-col gap-2">
                  {status.themes.map((item) => (
                    <button
                      key={item.key}
                      type="button"
                      data-theme-option={item.key}
                      aria-pressed={theme === item.key}
                      onClick={() => setTheme(item.key)}
                      className={`flex items-start gap-3 rounded-xl border px-3.5 py-3 text-left transition ${
                        theme === item.key
                          ? "border-accent bg-accent-soft"
                          : "border-line hover:bg-canvas"
                      }`}
                    >
                      <span
                        aria-hidden
                        className="mt-0.5 flex size-5 items-center justify-center rounded-full border border-line bg-surface"
                      >
                        {theme === item.key ? <Check className="size-3 text-accent-strong" /> : null}
                      </span>
                      <span className="flex flex-col">
                        <span className="text-[13px] font-medium">{item.name}</span>
                        <span className="text-[12px] text-muted">{item.description}</span>
                      </span>
                    </button>
                  ))}
                </div>
                <div>
                  <button type="button" className={buttonClass} onClick={chooseTheme} disabled={busy}>
                    {busy ? "Saving…" : "Use this theme"}
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                </div>
              </div>
            ) : (
              <div className="flex flex-col gap-4">
                <div>
                  <h1 className="text-[17px] font-semibold">Connect an AI provider</h1>
                  <p className="mt-1 text-[12.5px] text-muted">
                    Optional. Provider connections arrive with the AI Hub phase; skip this step now
                    and connect yours from the panel when it lands.
                  </p>
                </div>
                <Notice tone="info">
                  Nothing is configured here yet — skipping records the decision on your first-run
                  record.
                </Notice>
                <div className="flex flex-wrap items-center gap-3">
                  <button type="button" className={buttonClass} onClick={finish} disabled={busy}>
                    {busy ? "Finishing…" : "Skip and finish"}
                    <ArrowRight className="size-3.5" aria-hidden />
                  </button>
                  <span className="flex items-center gap-1.5 text-[12px] text-muted">
                    <Circle className="size-3" aria-hidden />
                    You can revisit this later
                  </span>
                </div>
              </div>
            )}
          </section>
        </div>

        <p className="mt-4 text-center text-[11.5px] text-muted">
          Omnion — an open-source enterprise application platform.
        </p>
      </div>
    </main>
  );
}
