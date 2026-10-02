import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { MenuEditor } from "@/features/menus/menu-editor";

export const metadata = { title: "Menu" };

/**
 * `/menus/{id}/edit` — one menu's item tree, its claimed slots and the rendered preview.
 *
 * The address carries the menu's *id* rather than its key, exactly like the page editor's address
 * carries a page id: the key is unique inside a site, and the panel can be pointed at a site it
 * is not currently switched to.
 */
export default async function MenuEditPage({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Menu editor"
        description="The item tree, the theme slots it claims, and what the site renders for each audience"
      >
        <MenuEditor menuId={id} />
      </AppShell>
    </RequireAuth>
  );
}
