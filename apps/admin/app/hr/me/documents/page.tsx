import { MyDocumentsView, MyWorkspaceNav } from "@/features/hr/my-workspace";

export default function Page() {
  return (
    <div className="space-y-4">
      <MyWorkspaceNav />
      <MyDocumentsView />
    </div>
  );
}
