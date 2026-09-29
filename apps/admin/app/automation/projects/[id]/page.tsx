import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ProjectDetail } from "@/features/projects/project-detail";

export const metadata = { title: "Project" };

export default function AutomationProjectDetailPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Project"
        description="The project's own fields, its members and what each role may do inside it"
      >
        <ProjectDetail />
      </AppShell>
    </RequireAuth>
  );
}
