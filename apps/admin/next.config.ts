import type { NextConfig } from "next";

/**
 * Origin of the Omnion API (`apps/api`).
 *
 * The default matches the development stack in `infra/compose` plus `cargo run -p omnion-api`.
 * A deployment sets `OMNION_API_URL` to its API origin.
 */
const apiOrigin = process.env.OMNION_API_URL ?? "http://127.0.0.1:8080";

const nextConfig: NextConfig = {
  reactStrictMode: true,
  /**
   * The admin panel talks to the API over its own origin: `/api/*` is forwarded to the API, so
   * the HttpOnly session cookie stays first-party, no CORS rule is needed, and a production
   * deployment only has to route `/api/*` at the edge.
   */
  async rewrites() {
    return [
      {
        source: "/api/:path*",
        destination: `${apiOrigin}/api/:path*`,
      },
    ];
  },
};

export default nextConfig;
