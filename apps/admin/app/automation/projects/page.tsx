import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ProjectList } from "@/features/projects/project-list";

export const metadata = { title: "Projects" };

export default function AutomationProjectsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Projects"
        description="Buckets for your automations. Every installation has a protected Default project; the ones you add here hold one team's workflows and credentials, with their own members."
      >
        <ProjectList />
      </AppShell>
    </RequireAuth>
  );
}
