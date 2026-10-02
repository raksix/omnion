// Does the document announce the theme of the site it is actually drawing?
//
// REQ-062 acceptance 1: "a site activated on each renders its pages with that theme's layout,
// not a colour-swapped copy." Every bundled sheet declares its tokens ONLY inside
// `html[data-theme="<key>"] { … }` and scopes every rule that reads them the same way, so that
// one attribute decides whether a theme paints anything at all.
//
// This drives the REAL modules — `apps/web/proxy.ts`, `apps/web/lib/api.ts` and
// `apps/web/lib/theme.ts` — against a fake public API that publishes two sites on two different
// themes, and asserts the theme the document ends up with. It needs no browser, no QA slot and
// no database, because the bug it exists for was never in the API: it was the renderer choosing
// two different themes for the two halves of one page.
//
// The check that matters is not "does it warn" but "does the page drawn and the `<html>` around
// it agree" — the defect was a page rendered in Magazine's markup under a Minimal attribute,
// which is complete, valid and wrong.

const fs = require("node:fs");
const path = require("node:path");
const { execFile } = require("node:child_process");

const WEB = path.resolve(__dirname, "../../apps/web");

let failures = 0;
function check(name, cond, detail) {
  if (cond) {
    console.log(`  ok  ${name}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

/**
 * Read a probe's `PREFIX <key> <value>` lines into a map.
 *
 * Splitting a three-column line with `split(" ")` and handing the pieces to `Object.fromEntries`
 * silently keeps only the LAST pair, so three sites produced one entry — `{"PAGE":"ghost"}` —
 * and every check below it compared against `undefined`. That is the "a green check that measured
 * nothing" shape, and it produced FIVE failures that all said `undefined` while the probe's own
 * first two checks were green: a failing value in a report reads as a product defect, and this
 * one was the harness's.
 */
function readThemes(stdout, prefix) {
  const map = {};
  for (const line of stdout.split("\n")) {
    const parts = line.trim().split(/\s+/);
    if (parts.length === 3 && parts[0] === prefix) {
      map[parts[1]] = parts[2];
    }
  }
  return map;
}
/** A public API that publishes `SITES`, one page each, and records what it was asked for. */
function fakeApi(sites) {
  const seen = [];
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://localhost");
    const key = url.searchParams.get("site");
    seen.push(key);
    const site = sites.find((s) => s.key === key);
    res.writeHead(site ? 200 : 404, { "content-type": "application/json" });
    res.end(JSON.stringify(site ? { site: { key: site.key, name: site.key, theme: site.theme } } : {}));
  });
  return { server, seen };
}

const http = require("node:http");

/**
 * Run a snippet as a module of `apps/web`, so the `@/` alias resolves exactly as it does in
 * `page.tsx` and `layout.tsx`. A snippet in `/tmp` cannot: the path alias is the tsconfig's, and
 * a copy of the modules into a scratch directory would be a reimplementation, which is the
 * failure mode a test like this must not have.
 *
 * It is `execFile` and NOT `execFileSync`. The fake API runs IN THIS PROCESS, so a synchronous
 * child would block the event loop that has to answer its request: the child waits for the
 * server, the server waits for the child to stop, and the probe hangs to its own timeout with
 * no output. That is the same shape as a test that passes because nothing ran.
 *
 * Both streams come back. `console.warn` — the thing the fallback check is looking for — goes to
 * **stderr**, so a probe that reads only stdout is asserting against a stream the message can
 * never appear on. That is precisely what happened here: the themes resolved correctly and the
 * one check that wanted the warning failed, which is how a correct renderer gets "fixed" by
 * removing its warning.
 */
function runInWeb(snippet, origin, name) {
  const file = path.join(WEB, `.probe-${name}.ts`);
  fs.writeFileSync(
    file,
    snippet,
  );
  return new Promise((resolve, reject) => {
    execFile("bun", ["run", file], { cwd: WEB, encoding: "utf8", env: { ...process.env, OMNION_API_URL: origin }, timeout: 60000 }, (err, stdout, stderr) => {
      fs.rmSync(file, { force: true });
      if (err) {
        reject(new Error(`${name}: bun failed\n${stderr || err.message}`));
        return;
      }
      resolve({ stdout, stderr: stderr || "" });
    });
  });
}

// Three real sites, each publishing its home page. `ghost` is a site activated on a theme this
// build does not ship: the API answers it normally, and only the RENDERER cannot resolve the
// key — which is the case the warning exists for. A site the API cannot find would be a
// different thing (a 404), and testing that here would test the fake, not the renderer.
const SITES = [
  { key: "main", theme: "magazine" },
  { key: "shop", theme: "tech" },
  { key: "ghost", theme: "no-such-theme" },
];

(async () => {
  const { server } = fakeApi(SITES);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = `http://127.0.0.1:${server.address().port}`;

  // 1. The page's theme, exactly as `page.tsx` reads it out of the payload.
  //
  //    It calls `resolveThemeOrWarn`, the function the fix introduced, and the OLD code does not
  //    export it — so the probe dies with a SyntaxError before reaching a single assertion. That
  //    is a *red* for the wrong reason, and a red for the wrong reason is how a real defect gets
  //    waved through: the report says the probe failed, not that the document drew the wrong
  //    theme. So the snippet resolves through `resolveTheme`, which BOTH versions export, and the
  //    warning is asserted separately from its own module — the contract is "which theme does
  //    this request end up with", not "does this function name exist".
  const pageOut = await runInWeb(
    `
import { resolveTheme } from "@/lib/theme";
import { getPublishedPage } from "@/lib/api";
for (const key of ["main", "shop", "ghost"]) {
  const content = await getPublishedPage("home", "desktop", key);
  const theme = resolveTheme(content.site.theme);
  console.log("PAGE " + key + " " + theme.key);
}
`,
    origin,
    "page",
  );
  const pageThemes = readThemes(pageOut.stdout, "PAGE");

  // 2. The document's theme, exactly as `layout.tsx` reads it — through the header `proxy.ts`
  //    forwards, because a layout cannot read `searchParams`.
  //
  //    The header name is READ OUT OF THE PROXY'S SOURCE rather than imported: `proxy.ts`
  //    imports `next/server`, which this CJS harness cannot resolve, and importing it would
  //    hang the probe on a module graph rather than test anything. Reading the declaration means
  //    the probe still fails when the two files disagree about the header's name — which is the
  //    failure worth catching — without needing a bundler.
  //
  //    Both of these are OPTIONAL and reported as such. Against the old tree `proxy.ts` does not
  //    exist, and "the file is missing" is the defect under test, not an extra failure about a
  //    mechanism: when it is gone, `header` is `null`, the layout therefore cannot be reading
  //    one, and the theme checks below are what actually decide the result.
  const proxyPath = path.join(WEB, "proxy.ts");
  const proxySource = fs.existsSync(proxyPath) ? fs.readFileSync(proxyPath, "utf8") : "";
  const header = (proxySource.match(/SITE_HINT_HEADER\s*=\s*"([^"]+)"/) || [])[1];
  check("the proxy declares the site-hint header", Boolean(header), "no proxy.ts / SITE_HINT_HEADER");
  const layoutSource = fs.readFileSync(path.join(WEB, "app/layout.tsx"), "utf8");
  check(
    "the layout reads the header the proxy writes",
    layoutSource.includes("SITE_HINT_HEADER"),
    "layout.tsx never mentions SITE_HINT_HEADER — it cannot resolve the request's site",
  );

  const layoutOut = await runInWeb(
    `
import * as themeModule from "@/lib/theme";
const getSiteTheme = (await import("@/lib/api")).getSiteTheme;
// The OLD renderer has no per-site theme reader and no warning; the fixed one has both. This
// snippet drives whatever exists, so the probe measures the DOCUMENT'S ANSWER rather than
// asserting that a particular new export is present.
const readSiteTheme = getSiteTheme;
for (const key of ["main", "shop", "ghost"]) {
  const raw = readSiteTheme ? await readSiteTheme(key) : undefined;
  const theme = themeModule.resolveThemeOrWarn
    ? themeModule.resolveThemeOrWarn(raw ?? undefined, "this site")
    : themeModule.resolveTheme(raw);
  console.log("LAYOUT " + key + " " + theme.key);
}
`,
    origin,
    "layout",
  );
  const layoutThemes = readThemes(layoutOut.stdout, "LAYOUT");

  console.log(`site-hint header: ${header}`);
  console.log(`page themes   : ${JSON.stringify(pageThemes)}`);
  console.log(`layout themes : ${JSON.stringify(layoutThemes)}`);
  console.log("");

  check(
    "the page draws magazine for the site activated on magazine",
    pageThemes.main === "magazine",
    `got ${pageThemes.main}`,
  );
  check(
    "the page draws tech for the site activated on tech",
    pageThemes.shop === "tech",
    `got ${pageThemes.shop}`,
  );
  check(
    "the document announces the site's own theme, not the installation default",
    layoutThemes.main === "magazine",
    `<html data-theme> was ${layoutThemes.main} for a magazine site — this is the colour-swapped-copy defect`,
  );
  check(
    "the document and the page agree for every site",
    ["main", "shop"].every((key) => pageThemes[key] === layoutThemes[key]),
    `page ${JSON.stringify(pageThemes)} vs layout ${JSON.stringify(layoutThemes)}`,
  );
  check(
    "an unknown theme falls back to the default and still draws a page",
    layoutThemes.ghost === "minimal",
    `got ${layoutThemes.ghost}`,
  );
  check("the fallback is reported, not silent", layoutOut.stderr.includes("no-such-theme"), "no warning line");

  server.close();
  console.log(failures === 0 ? "\nPASS" : `\nFAIL (${failures})`);
  process.exit(failures === 0 ? 0 : 1);
})();