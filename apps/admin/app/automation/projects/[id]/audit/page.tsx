import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ProjectAuditScreen } from "@/features/projects/project-audit";

export const metadata = { title: "Project audit" };

export default function AutomationProjectAuditPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Project audit"
        description="Everything recorded inside this project, filtered by the project rather than by the tenant"
      >
        <ProjectAuditScreen />
      </AppShell>
    </RequireAuth>
  );
}
