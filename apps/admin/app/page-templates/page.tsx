import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { TemplateGallery } from "@/features/blocks/template-gallery";

export const metadata = { title: "Page templates" };

export default function PageTemplatesRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Page templates"
        description="Start a page from a structure with its sample content already in place"
      >
        <TemplateGallery />
      </AppShell>
    </RequireAuth>
  );
}
