import { MyProfileView, MyWorkspaceNav } from "@/features/hr/my-workspace";

/**
 * `/hr/me` — the caller's own employee record.
 *
 * Deliberately OUTSIDE the HR module's own shelf: that shelf lists what an administrator works
 * with, and putting "My profile" in it would mean a person without any `hr.*` key lands on a nav
 * whose every link refuses them. The self-service group is its own nav for the same reason the
 * routes carry no permission.
 */
export default function Page() {
  return (
    <div className="space-y-4">
      <MyWorkspaceNav />
      <MyProfileView />
    </div>
  );
}
