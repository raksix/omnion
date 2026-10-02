import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ProjectLimitsScreen } from "@/features/projects/project-limits";

export const metadata = { title: "Project limits" };

export default function AutomationProjectLimitsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Limits & usage"
        description="The caps this project runs under, what it has used, and the export behind both"
      >
        <ProjectLimitsScreen />
      </AppShell>
    </RequireAuth>
  );
}
