import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiSkills } from "@/features/ai/ai-skills";

export const metadata = { title: "Skills" };

/**
 * The skills registry (REQ-099, slice 3).
 *
 * A skill is a short piece of written guidance the model gets with every run — when to reach
 * for it and what a good answer looks like. It is data, never code: it cannot be executed, and
 * naming a tool in a skill does not grant that tool to the agent. The screen exists so an
 * operator can see, edit and retire that guidance without a database session.
 */
export default function AiSkillsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Skills"
        description="Written guidance every run of an attached agent receives — never code, and never a tool grant"
      >
        <AiSkills />
      </AppShell>
    </RequireAuth>
  );
}
