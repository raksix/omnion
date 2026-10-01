import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiLocalDoctorView } from "@/features/ai/ai-local-doctor";

export const metadata = { title: "Local AI doctor" };

/**
 * `/ai/local/doctor` — can this installation run inference by itself? (REQ-106, slice 4)
 *
 * A separate screen from `/ai/local` rather than another tab there, because the two answer
 * different questions: the endpoints screen answers "what does this installation talk to", and
 * the doctor answers "does any of it actually work with the internet unplugged". An operator
 * pointing production at an air-gapped installation needs the second before the first, and the
 * one that fails is the one with four possible causes.
 */
export default function AiLocalDoctorPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Local AI doctor"
        description="Reachability, models, a real one-token completion, embeddings and the air-gap state — each with its own verdict and its own fix. 'Not established' means unproven, not working"
      >
        <AiLocalDoctorView />
      </AppShell>
    </RequireAuth>
  );
}