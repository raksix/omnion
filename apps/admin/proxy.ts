/**
 * Request gate for the admin panel.
 *
 * Next.js 16 renamed the `middleware.ts` convention to `proxy.ts`; the exported function is
 * `proxy`. It checks only that a session cookie is present — whether the session itself is
 * real stays a question for the API — and keeps anonymous visitors on the sign-in screen
 * before any panel route renders. The first-run wizard (`/setup`) is open on purpose: a fresh
 * installation has no cookie to show yet.
 */
import { NextResponse, type NextRequest } from "next/server";

import { SESSION_COOKIE } from "@/lib/session-cookie";

/** Screens an anonymous visitor may reach. */
const OPEN_PATHS = new Set(["/login", "/setup"]);

export function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;

  if (!request.cookies.has(SESSION_COOKIE) && !OPEN_PATHS.has(pathname)) {
    const signIn = request.nextUrl.clone();
    signIn.pathname = "/login";
    signIn.search = "";
    return NextResponse.redirect(signIn);
  }

  return NextResponse.next();
}

export const config = {
  // Everything except the API forwards, the sign-in screen, the setup wizard and the assets.
  matcher: ["/((?!api|login|setup|_next/static|_next/image|favicon.ico|icon.svg).*)"],
};
