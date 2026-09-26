import { SetupView } from "@/features/setup/setup-view";

export const metadata = { title: "First-run setup" };

/**
 * The wizard screen. It is deliberately outside `RequireAuth`: a fresh installation has no
 * session yet, and the wizard's first step is what creates one.
 */
export default function SetupPage() {
  return <SetupView />;
}
