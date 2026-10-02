import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { FeaturedMediaTab } from "@/features/pages/featured-media-tab";

export const metadata = { title: "Page image" };

/**
 * `/pages/<id>/media` — the page's featured image, its alt, its legend and its crop
 * (REQ-064, slice 4d).
 *
 * Its own route rather than a tab inside the block editor, for the reason the members settings
 * have: one state, one owner. The block editor is a thousand-line component that owns the
 * working block tree, and a media field bolted into it would either be saved by a second writer
 * (two "unsaved" bars on one screen) or have to go through the editor's own save — which is how
 * an image ends up published by a keystroke the operator meant for a heading.
 */
export default function PageMediaRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Page image"
        description="The one image this page leads with, what it says, and where a crop centres"
      >
        <FeaturedMediaTab />
      </AppShell>
    </RequireAuth>
  );
}
