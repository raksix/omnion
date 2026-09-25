/**
 * Server-side session lookup.
 *
 * The panel validates the session where the cookie is readable: on the server, against the
 * API. The browser therefore never has to probe `GET /api/v1/me` and never sees a 401 for a
 * visitor who is simply not signed in yet.
 */
import { cookies } from "next/headers";

import type { User } from "./types";

/** Origin of the API — the same default `next.config.ts` forwards `/api/*` to. */
const apiOrigin = process.env.OMNION_API_URL ?? "http://127.0.0.1:8080";

/** The account the request's cookie belongs to, or `null` when there is none. */
export async function readSessionUser(): Promise<User | null> {
  const jar = await cookies();
  const cookieHeader = jar
    .getAll()
    .map((cookie) => `${cookie.name}=${cookie.value}`)
    .join("; ");

  if (!cookieHeader) {
    return null;
  }

  try {
    const response = await fetch(`${apiOrigin}/api/v1/me`, {
      headers: { accept: "application/json", cookie: cookieHeader },
      cache: "no-store",
    });
    if (!response.ok) {
      return null;
    }
    const body = (await response.json()) as { user: User };
    return body.user;
  } catch {
    // The API down means nobody is signed in as far as the panel is concerned.
    return null;
  }
}
