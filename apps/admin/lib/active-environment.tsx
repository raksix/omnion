"use client";

/**
 * Which environment the panel is *working in* (REQ-017, slice 5).
 *
 * The acceptance line is "the environment chip appears in the panel header while staging is
 * active". That word — **active** — is the whole design problem, and it is not a word the request
 * defines. Everything else in this request is about a *named* environment (`GET
 * /environments/{id}`), so a chip could be written in ten minutes by reading the URL. The trouble
 * is that a chip pinned to the URL is a chip that is right on one screen and wrong on the other
 * forty: you open a staging environment's detail, the chip says "staging"; you then click into
 * Pages, and the chip either vanishes or keeps claiming staging while the panel is showing
 * production content. A chip that lies is worse than no chip, because the one thing it is for is
 * telling a person which copy of their content they are about to edit.
 *
 * So the answer is an explicit selection, owned here, not derived from where you are:
 *
 * - **The chip is a control, not a label.** It opens a list of the tenant's environments and sets
 *   the selection. A chip you cannot change is a caption, and a caption in the header is noise.
 * - **Selecting production is the default and the reset.** Entering staging is deliberate; leaving
 *   it is one click, and the choice is what stops an edit in staging from being mistaken for an
 *   edit in production. This is the same reason the request insists on an *undismissable* banner
 *   (see `environment-banner.tsx`): the two halves are one mechanism. The chip is how you arrive,
 *   the banner is how you cannot forget.
 * - **The selection is per tenant.** Switching organization resets it, because environment ids
 *   belong to one tenant and a chip carrying the last tenant's id is a 404 with extra steps.
 * - **It survives a reload** (localStorage) but never a tenant switch, and it degrades to
 *   production whenever the stored id is no longer in the list — an archived or deleted
 *   environment must not leave the panel claiming to be inside it.
 *
 * It fetches `/environments` itself, which the environments screen also fetches. Same trade as
 * `tenant-status.tsx`, for the same reason: the header chrome needs the *answer* on every screen,
 * and hoisting the list screen's copy would couple the chrome to a screen that is not always
 * mounted.
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";

import { fetchEnvironments } from "./api";
import { useSession } from "./session";
import type { Environment } from "./types";

/** The tenant whose selection is stored. A switch to another tenant resets it. */
const TENANT_KEY = "omnion-active-environment-org";
/** The selected environment id, per tenant. */
const ENV_KEY = "omnion-active-environment";

type ActiveEnvironmentValue = {
  /** The tenant's environments, once the list has loaded. Empty while loading and on failure. */
  environments: Environment[];
  /** The selected environment, or `null` before the list loads and on a failed read. */
  environment: Environment | null;
  /**
   * `true` only when the selected environment is staging *and* usable.
   *
   * `false` while loading, for a failed read, and for production. The chip and the banner both
   * read this, so there is exactly one definition of "the panel is inside a staging copy" and it
   * cannot drift between the two surfaces that have to agree.
   */
  isStaging: boolean;
  /** `true` once the list has been read — `false` only while in flight. */
  loaded: boolean;
  /** Select an environment by id. An unknown id falls back to production. */
  select: (id: string) => void;
  /** Re-read the list (a clone finishing or an archive changes what a selection may mean). */
  reload: () => void;
};

const ActiveEnvironmentContext = createContext<ActiveEnvironmentValue | null>(null);

/** The production environment of a list, or `null` when the tenant somehow has none. */
function productionOf(environments: Environment[]): Environment | null {
  return environments.find((entry) => entry.type === "production") ?? null;
}

/**
 * The stored id, but only when it belongs to the tenant that stored it.
 *
 * Returning `null` for a mismatch is what makes a tenant switch safe: the panel cannot come back
 * holding a foreign environment id, because the value that would select one is never read.
 */
function readStored(tenantId: string | null): string | null {
  if (typeof window === "undefined" || tenantId === null) return null;
  try {
    if (window.localStorage.getItem(TENANT_KEY) !== tenantId) return null;
    return window.localStorage.getItem(ENV_KEY);
  } catch {
    // Private mode and a full quota both throw here, and neither is a reason to break the panel —
    // the cost of losing the selection is one click after a reload.
    return null;
  }
}

function writeStored(tenantId: string | null, environmentId: string | null) {
  if (typeof window === "undefined") return;
  try {
    if (tenantId === null) {
      window.localStorage.removeItem(TENANT_KEY);
      window.localStorage.removeItem(ENV_KEY);
      return;
    }
    window.localStorage.setItem(TENANT_KEY, tenantId);
    if (environmentId === null) {
      window.localStorage.removeItem(ENV_KEY);
    } else {
      window.localStorage.setItem(ENV_KEY, environmentId);
    }
  } catch {
    /* A selection that will not persist is still a selection for this session. */
  }
}

/** Provide the panel's active environment. */
export function ActiveEnvironmentProvider({ children }: { children: React.ReactNode }) {
  const { status: sessionStatus } = useSession();
  const [environments, setEnvironments] = useState<Environment[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [tenantId, setTenantId] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  // The tenant comes from the cookie-backed session rather than from the environment list, because
  // the list is what this provider fetches and reading it from itself would be a cycle. `null`
  // while signed out is the correct answer, and it clears the selection below.
  useEffect(() => {
    // Nothing to key a stored selection on without a session, and a stale tenant id from the
    // previous sign-in must not survive into the next one. Same gate as the read below.
    if (sessionStatus !== "signed-in") {
      setTenantId(null);
      return;
    }
    const read = () => {
      const match = document.cookie.match(/(?:^|;\s*)omnion_org=([^;]+)/);
      setTenantId(match ? decodeURIComponent(match[1]) : null);
    };
    read();
    // A tenant switch is a navigation, not a state change this component can observe, so the
    // cookie is re-read whenever the list is re-read. It is a cheap string parse, and guessing
    // wrong would show one tenant's environments inside another's panel.
  }, [sessionStatus, reloadToken]);

  useEffect(() => {
    // No session, no read. This provider is mounted by the app shell, so without the gate it
    // fired on the sign-in screen, on every public render, and for every signed-in account that
    // cannot read the list — and an unauthenticated `/environments` answers 401, which the
    // browser logs as a failed request on *every* page of the panel. The two sibling providers
    // (`tenant-status`, `sites`) gate on the same `sessionStatus`; this one did not, and the
    // asymmetry is invisible from the code until you count the console.
    if (sessionStatus !== "signed-in") {
      setEnvironments([]);
      setLoaded(true);
      return;
    }
    let cancelled = false;
    setLoaded(false);
    fetchEnvironments()
      .then((body) => {
        if (cancelled) return;
        setEnvironments(body.environments);
        setLoaded(true);
      })
      .catch(() => {
        // A header chip that cannot load must not take the panel down, and it must not *hide*
        // navigation either — an absent chip reads as "there is no staging here", which is a
        // claim, and the panel is not entitled to make one because a request failed.
        if (!cancelled) {
          setEnvironments([]);
          setLoaded(true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [sessionStatus, reloadToken]);

  const select = useCallback(
    (id: string) => {
      setSelectedId(id);
      writeStored(tenantId, id);
    },
    [tenantId],
  );

  const value = useMemo<ActiveEnvironmentValue>(() => {
    // The stored id is adopted once, and only when the list contains it. An id that is absent
    // (archived, deleted, or from another tenant) resets to production rather than leaving the
    // chip pointing at nothing.
    const stored = readStored(tenantId);
    const adopted =
      selectedId ??
      (stored !== null && environments.some((entry) => entry.id === stored) ? stored : null);
    const match = adopted !== null ? (environments.find((entry) => entry.id === adopted) ?? null) : null;
    const environment = match ?? productionOf(environments);
    // An archived staging environment is not "inside staging" for editing purposes: its content
    // is readable, which is what the archive criterion asks for, and the panel should not be
    // wearing a staging chip over a read-only copy.
    const isStaging =
      environment !== null &&
      environment.type === "staging" &&
      environment.status !== "archived" &&
      loaded;
    return {
      environments,
      environment,
      isStaging,
      loaded,
      select,
      reload: () => setReloadToken((token) => token + 1),
    };
  }, [environments, selectedId, tenantId, loaded, select]);

  return (
    <ActiveEnvironmentContext.Provider value={value}>{children}</ActiveEnvironmentContext.Provider>
  );
}

/** Read the panel's active environment; only valid inside [`ActiveEnvironmentProvider`]. */
export function useActiveEnvironment(): ActiveEnvironmentValue {
  const value = useContext(ActiveEnvironmentContext);
  if (value === null) {
    throw new Error("useActiveEnvironment must be used inside an ActiveEnvironmentProvider");
  }
  return value;
}
