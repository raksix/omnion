import Link from "next/link";

/**
 * The site's not-found view.
 *
 * One view for every unpublished address — the API answers the same `404` whether a page does
 * not exist or is not published, and the public surface must not tell them apart.
 */
export const metadata = {
  title: "Nothing published here",
};

export default function NotFound() {
  return (
    <div className="mn-shell">
      <main className="mn-main">
        <article className="mn-article">
          <h1 className="mn-title">Nothing published here</h1>
          <p className="mn-summary">
            This address has no published page yet. As soon as the site&apos;s editors publish a
            page with this address in the Omnion panel, it appears here.
          </p>
          <p className="mn-meta">
            <Link href="/">Back to the home page</Link>
          </p>
        </article>
      </main>
    </div>
  );
}
