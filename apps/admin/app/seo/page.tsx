import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SeoView } from "@/features/seo/seo-view";

export const metadata = { title: "SEO" };

/**
 * `/seo` — the site's search setup (REQ-064, slice 3).
 *
 * Carries `seo.read` for the read side and `seo.manage` for every write, so the page does not
 * check a permission itself: the route guard answers 403 and the panel renders its own state.
 * The three panels live on one route rather than the three the REQ sketched because they answer
 * one question — "what does a crawler see, and what is in the way" — and an owner chasing a page
 * that is not indexed needs all of them open at once.
 */
export default function SeoPage() {
  return (
    <RequireAuth>
      <AppShell
        title="SEO"
        description="Redirects, the sitemap, robots.txt and the links that point at nothing"
      >
        <SeoView />
      </AppShell>
    </RequireAuth>
  );
}
