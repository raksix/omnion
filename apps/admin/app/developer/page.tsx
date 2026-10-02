import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperOverviewView } from "@/features/developer/developer-overview-view";

export const metadata = { title: "Developer · Overview" };

/**
 * `/developer` — the section root (REQ-033, slice 4's overview cards).
 *
 * Six developer surfaces shipped across slices 1–4 with no landing page between them, so the
 * section could only be entered by knowing which screen you wanted. This is the answer to "where
 * do I start?", and it is a *count* screen: every figure is read from a list its own destination
 * already lists, so a number on this page can never disagree with the screen it links to.
 *
 * No `Suspense` boundary, deliberately. Nothing here reads `?`, so `useSearchParams` would buy a
 * build-time requirement for nothing — the trap documented on `/developer/sdks`, which needed the
 * boundary because the CLI hands out a `?tab=cli` deep link and this screen has no deep link.
 */
export default function DeveloperOverviewPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Developer"
        description="Reference, credentials, the app registry, the event catalogue, generated starters and the request log"
      >
        <DeveloperOverviewView />
      </AppShell>
    </RequireAuth>
  );
}