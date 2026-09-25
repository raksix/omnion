import type { NextConfig } from "next";

/**
 * The public renderer talks to the Omnion API (`apps/api`) from the **server** only: it reads
 * published content from the public content surface and turns it into HTML, so no browser
 * origin ever calls the API and no CORS rule is needed. `OMNION_API_URL` defaults to the
 * development stack (`cargo run -p omnion-api` + `infra/compose`).
 */
const nextConfig: NextConfig = {
  reactStrictMode: true,
  /**
   * The workspace packages behind the theme engine ship TypeScript sources; Next compiles them
   * with the app so a theme can be read and debugged where it is written.
   */
  transpilePackages: ["@omnion/types", "@omnion/theme-sdk", "@omnion/theme-minimal"],
};

export default nextConfig;
