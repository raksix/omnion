"use client";

/** Renders a screen only for a signed-in account; a session that ends leads back to sign-in. */
import { useEffect, type ReactNode } from "react";

import { useRouter } from "next/navigation";

import { useSession } from "@/lib/session";

/** Guard for every panel screen. */
export function RequireAuth({ children }: { children: ReactNode }) {
  const { status } = useSession();
  const router = useRouter();

  useEffect(() => {
    if (status === "signed-out") {
      router.replace("/login");
    }
  }, [status, router]);

  if (status === "signed-in") {
    return <>{children}</>;
  }

  return (
    <div className="flex min-h-screen items-center justify-center px-4 text-[13px] text-muted">
      Taking you to sign in…
    </div>
  );
}
