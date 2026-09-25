/**
 * Request gate for the admin panel.
 *
 * Next.js 16 renamed the `middleware.ts` convention to `proxy.ts`; the exported function is
 * `proxy`. It checks only that a session cookie is present — whether the session itself is
 * real stays a question for the API — and keeps anonymous visitors on the sign-in screen
 * before any panel route renders.
 */
import { NextResponse, type NextRequest } from "next/server";

import { SESSION_COOKIE } from "@/lib/session-cookie";

export function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;

  if (!request.cookies.has(SESSION_COOKIE) && pathname !== "/login") {
    const signIn = request.nextUrl.clone();
    signIn.pathname = "/login";
    signIn.search = "";
    return NextResponse.redirect(signIn);
  }

  return NextResponse.next();
}

export const config = {
  // Everything except the API forwards, the sign-in screen and the static assets.
  matcher: ["/((?!api|login|_next/static|_next/image|favicon.ico|icon.svg).*)"],
};
