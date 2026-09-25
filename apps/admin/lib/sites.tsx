"use client";

/**
 * The site the panel is currently working on.
 *
 * A site switcher only makes sense against a loaded list, so this provider fetches the sites
 * of the signed-in account once, keeps the selection in `localStorage`, and falls back to the
 * first site whenever the stored one is gone (a fresh browser, a deleted site).
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";

import { ApiError, fetchSites } from "./api";
import { useSession } from "./session";
import type { Site } from "./types";

const STORAGE_KEY = "omnion.admin.selected-site";

type SitesValue = {
  /** Sites the account may see. */
  sites: Site[];
  /** The site the panel is showing. */
  selectedSite: Site | null;
  /** `loading` while the list is on its way, then `ready` or `error`. */
  status: "idle" | "loading" | "ready" | "error";
  /** Why the list could not be loaded, when it could not. */
  error: string | null;
  /** Switch the panel to another site. */
  selectSite: (siteId: string) => void;
  /** Re-read the list from the API. */
  reload: () => void;
};

const SitesContext = createContext<SitesValue | null>(null);

function readStoredSiteId(): string | null {
  if (typeof window === "undefined") {
    return null;
  }
  try {
    return window.localStorage.getItem(STORAGE_KEY);
  } catch {
    // A blocked storage must not take the panel down; the first site is a fine fallback.
    return null;
  }
}

function storeSiteId(siteId: string): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, siteId);
  } catch {
    // Ignore: the selection simply does not survive a reload.
  }
}

/** Provide the sites and the current selection to the panel. */
export function SitesProvider({ children }: { children: React.ReactNode }) {
  const { status: sessionStatus } = useSession();
  const [sites, setSites] = useState<Site[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [status, setStatus] = useState<SitesValue["status"]>("idle");
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (sessionStatus !== "signed-in") {
      setSites([]);
      setSelectedId(null);
      setStatus("idle");
      return;
    }

    let cancelled = false;
    setStatus("loading");
    setError(null);
    fetchSites()
      .then((list) => {
        if (cancelled) {
          return;
        }
        setSites(list);
        setStatus("ready");
        setSelectedId((current) => {
          const wanted = current ?? readStoredSiteId();
          if (wanted && list.some((site) => site.id === wanted)) {
            return wanted;
          }
          return list.length > 0 ? list[0].id : null;
        });
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setSites([]);
        setSelectedId(null);
        setStatus("error");
        setError(cause instanceof ApiError ? cause.message : "The sites could not be loaded.");
      });

    return () => {
      cancelled = true;
    };
  }, [sessionStatus, reloadToken]);

  const selectSite = useCallback((siteId: string) => {
    setSelectedId(siteId);
    storeSiteId(siteId);
  }, []);

  const selectedSite = useMemo(
    () => sites.find((site) => site.id === selectedId) ?? null,
    [sites, selectedId],
  );

  const value = useMemo(
    () => ({ sites, selectedSite, status, error, selectSite, reload }),
    [sites, selectedSite, status, error, selectSite, reload],
  );

  return <SitesContext.Provider value={value}>{children}</SitesContext.Provider>;
}

/** Read the site list and the current selection; only valid inside [`SitesProvider`]. */
export function useSites(): SitesValue {
  const value = useContext(SitesContext);
  if (!value) {
    throw new Error("useSites must be used inside <SitesProvider>");
  }
  return value;
}
