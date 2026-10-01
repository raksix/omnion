/**
 * Request gate for the public renderer.
 *
 * Next.js 16 renamed the `middleware.ts` convention to `proxy.ts`; the exported function is
 * `proxy`. This one does not gate anything — there is nothing to refuse on a public site — it
 * carries the request's site hint forward to the document.
 *
 * ## Why this file exists at all
 *
 * All ten bundled stylesheets ship in one bundle and every element rule in them is scoped to
 * `html[data-theme="<key>"]`, so the theme a visitor sees is decided entirely by the `data-theme`
 * attribute on `<html>`. That attribute is written by `app/layout.tsx`, and a **layout cannot
 * read `searchParams`** — Next types them apart on purpose (`"searchParams" is not a valid
 * layout prop`), and the page component below it may not exist: the not-found view is a layout
 * render with no page under it.
 *
 * So the `?site=` the visitor put on the URL arrives here, and this is the only place on the
 * request that can see it and write it where the document can read it. It is forwarded as a
 * request HEADER, not as a rewrite or a cookie:
 *
 * - a **header** is scoped to the one request and is readable from `headers()` anywhere below,
 *   which is exactly what both the layout and the page need;
 * - a **cookie** would persist — a visitor who clicked a `?site=` link once would keep drawing
 *   that site on every later address, which is a real bug with no visible cause;
 * - a **rewrite** would change the URL the renderer resolves the page from, and the page already
 *   reads `?site=` itself, so two readers would have to agree on the same value again.
 *
 * The value is passed through exactly as it arrived. It is NOT re-validated here: the API's
 * `classify_hint` and the renderer's `normalizeSiteHint` are the two ends of one contract, and a
 * third copy of the rules in a file whose job is to forward a string is a third place for them
 * to drift. A hint this file mangled into a valid-but-different key would be a *worse* outcome
 * than one the API refuses.
 */
import { NextResponse, type NextRequest } from "next/server";

/** Request header the document reads the addressed site back from. */
export const SITE_HINT_HEADER = "x-omnion-site-hint";

export function proxy(request: NextRequest) {
  const site = request.nextUrl.searchParams.get("site");
  const headers = new Headers(request.headers);
  if (site) {
    headers.set(SITE_HINT_HEADER, site);
  }
  return NextResponse.next({ request: { headers } });
}

export const config = {
  // Every document render. `_next/*` is excluded because those requests render no document,
  // and the assets themselves must not pay for a header rewrite they never read.
  matcher: ["/((?!_next/static|_next/image|favicon.ico|icon.svg).*)"],
};