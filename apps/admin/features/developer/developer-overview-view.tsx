"use client";

/**
 * `/developer` — the section root (REQ-033, slice 4's first clause: the overview cards).
 *
 * Six developer surfaces exist and none of them is a landing page: the Explorer, the keys, the
 * OAuth registry, the event catalogue, the tooling and the logs. That is the shape that produces
 * a person opening the section and finding six equally-weighted links and no answer to "where do
 * I start?", so this screen is the answer to exactly that question and nothing else.
 *
 * Five decisions here are load-bearing, and the wrong version of each is a bug rather than a
 * preference:
 *
 * - **The cards are *counts and destinations*, not status.** Every figure comes from a list the
 *   section's own screens already fetch, so the overview cannot invent a number that disagrees
 *   with the screen it links to. A card saying "3 keys" next to a keys list showing four is the
 *   kind of disagreement that makes a person stop trusting the panel.
 * - **A failing read degrades one card, never the page.** Four of the six reads are optional
 *   enough that a `403` on one must not blank the other five — a developer with the Explorer
 *   permission but not `developer.keys.read` is an ordinary person, and the screen that tells
 *   them so is more useful than an error page that refuses to name the missing key.
 * - **The cards render in a fixed order and it is the order of the work, not alphabetical.**
 *   Read → authenticate → let others authenticate → know what arrives → generate → watch. A
 *   developer who needs none of it still gets "Logs" last because that is when they want it.
 * - **A "0" is a fact and an error is a different shape.** Zero keys renders as `0` next to a
 *   Create link; an unreadable key list renders a refusal naming the permission, never a zero.
 *   Collapsing the two is how a permission bug becomes "this tenant has no keys".
 * - **No `Suspense` boundary and no `useSearchParams`.** Nothing on this screen reads the query
 *   string, so opting into `useSearchParams` would buy a `Suspense` requirement for nothing —
 *   the trap the SDK screen documents at `/developer/sdks`.
 *
 * Every hook is `data-dev-overview-*`; the walkthrough pass drives them by name.
 */

import { useCallback, useEffect, useState, type ReactNode } from "react";

import {
  AppWindow,
  BookOpen,
  Compass,
  KeyRound,
  Package,
  Radio,
  ScrollText,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchApiKeys,
  fetchEventCatalogue,
  fetchOAuthApps,
  fetchScaffolds,
} from "@/lib/api";
import type {
  ApiKeysResponse,
  EventCatalogue,
  OAuthAppsResponse,
  ScaffoldList,
} from "@/lib/types";

type Load<T> = { value: T | null; error: string | null; pending: boolean };

const pending = <T,>(): Load<T> => ({ value: null, error: null, pending: true });

function StatCard({
  href,
  icon,
  label,
  value,
  hint,
  error,
  pending: loading,
  testId,
}: {
  href: string;
  icon: ReactNode;
  label: string;
  value: string;
  hint: string;
  error: string | null;
  pending: boolean;
  testId: string;
}) {
  return (
    <article className="rounded-xl border border-line bg-surface p-4" data-testid={testId}>
      <div className="flex items-center gap-2 text-muted">
        <span aria-hidden className="flex size-7 items-center justify-center rounded-lg bg-canvas">
          {icon}
        </span>
        <span className="text-[12px] font-medium tracking-wide uppercase">{label}</span>
      </div>
      {error ? (
        <p className="mt-3 text-[13px] text-danger" data-testid={`${testId}-error`}>
          {error}
        </p>
      ) : (
        <p className="mt-3 text-[26px] leading-none font-semibold" data-testid={`${testId}-value`}>
          {loading ? "—" : value}
        </p>
      )}
      <p className="mt-1.5 text-[12px] text-muted">{hint}</p>
      <Link
        href={href}
        className="mt-3 inline-block text-[13px] font-medium text-accent hover:underline"
        data-testid={`${testId}-link`}
      >
        Open
      </Link>
    </article>
  );
}

/** The developer section's landing screen. */
export function DeveloperOverviewView() {
  const [keys, setKeys] = useState<Load<ApiKeysResponse>>(pending);
  const [apps, setApps] = useState<Load<OAuthAppsResponse>>(pending);
  const [events, setEvents] = useState<Load<EventCatalogue>>(pending);
  const [scaffolds, setScaffolds] = useState<Load<ScaffoldList>>(pending);

  // One loader, four results. Each read settles independently and a rejection names its own
  // permission, so the card that cannot be read says why while the other three still answer.
  useEffect(() => {
    let cancelled = false;

    const read = <T,>(
      load: () => Promise<T>,
      set: (next: Load<T>) => void,
      fallback: string,
    ) => {
      load()
        .then((value) => {
          if (!cancelled) set({ value, error: null, pending: false });
        })
        .catch((cause: unknown) => {
          if (cancelled) return;
          set({
            value: null,
            error: cause instanceof ApiError ? cause.message : fallback,
            pending: false,
          });
        });
    };

    read(fetchApiKeys, setKeys, "The API keys could not be loaded.");
    read(fetchOAuthApps, setApps, "The applications could not be loaded.");
    read(fetchEventCatalogue, setEvents, "The event catalogue could not be loaded.");
    read(fetchScaffolds, setScaffolds, "The generated starters could not be loaded.");

    return () => {
      cancelled = true;
    };
  }, []);

  // The OAuth registry counts rows the list endpoint returns; a registry this tenant has not
  // created yet is `0`, not an error, and the "withdrawn" rows are counted too because they are
  // still rows a developer will see in the list.
  const appsValue = apps.value === null ? "—" : String(apps.value.apps.length);
  const eventsValue =
    events.value === null ? "—" : `${events.value.live_count} of ${events.value.events.length}`;

  if (keys.error && apps.error && events.error && scaffolds.error) {
    return (
      <EmptyState
        title="The developer section could not be read"
        hint="Every surface on this page needs at least one permission this account does not hold. Ask an administrator for a developer role, or open a surface you can already reach."
      />
    );
  }

  return (
    <div className="space-y-4">
      <div
        className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3"
        data-testid="dev-overview-cards"
      >
        <StatCard
          href="/developer/api-explorer"
          icon={<Compass className="size-4" />}
          label="API Explorer"
          value="Browse"
          hint="Every operation this account may call, with a request form that runs as you"
          error={null}
          pending={false}
          testId="dev-card-explorer"
        />
        <StatCard
          href="/developer/keys"
          icon={<KeyRound className="size-4" />}
          label="API keys"
          value={keys.value === null ? "—" : String(keys.value.keys.length)}
          hint="Credentials for a server of your own, with scopes and an IP allowlist"
          error={keys.error}
          pending={keys.pending}
          testId="dev-card-keys"
        />
        <StatCard
          href="/developer/oauth-apps"
          icon={<AppWindow className="size-4" />}
          label="OAuth apps"
          value={appsValue}
          hint="Let somebody else sign in as one of your users, with authorization code and PKCE"
          error={apps.error}
          pending={apps.pending}
          testId="dev-card-apps"
        />
        <StatCard
          href="/developer/events"
          icon={<Radio className="size-4" />}
          label="Event catalogue"
          value={eventsValue}
          hint="What the platform can emit, what each event carries, and how to subscribe"
          error={events.error}
          pending={events.pending}
          testId="dev-card-events"
        />
        <StatCard
          href="/developer/sdks"
          icon={<Package className="size-4" />}
          label="Starters and CLI"
          value={scaffolds.value === null ? "—" : String(scaffolds.value.scaffolds.length)}
          hint="Generate a plugin, theme or workflow archive, or sign a terminal in"
          error={scaffolds.error}
          pending={scaffolds.pending}
          testId="dev-card-sdks"
        />
        <StatCard
          href="/developer/logs"
          icon={<ScrollText className="size-4" />}
          label="Request logs"
          value="Inspect"
          hint="Every call this tenant made, by key, status, path and time"
          error={null}
          pending={false}
          testId="dev-card-logs"
        />
      </div>

      <section
        className="rounded-xl border border-line bg-surface p-4"
        data-testid="dev-overview-contract"
      >
        <div className="flex items-center gap-2">
          <BookOpen aria-hidden className="size-4 text-muted" />
          <h2 className="text-[13px] font-medium">What the API contract is</h2>
        </div>
        <p className="mt-2 text-[13px] text-muted">
          The Explorer reads its operations from the same router the API serves, so a call it offers
          is a call that exists. When a route changes, the drift check in{" "}
          <code className="text-[12px]">cargo test --workspace</code> fails the build rather than
          letting a developer learn a wrong call from the panel.
        </p>
      </section>
    </div>
  );
}