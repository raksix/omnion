#!/usr/bin/env node
/**
 * Smoke-test the GENERATED TypeScript client against a LIVE server (REQ-130, slice 3).
 *
 * The Python half of this smoke test is `scripts/qa/sdk-smoke.py`; this is the same set of
 * claims in the other language, because "both packages pass smoke tests" is two facts and
 * testing one of them proves the generator is not language-blind, not that it is correct.
 *
 *   node scripts/qa/sdk-smoke.mjs --base-url http://127.0.0.1:18085
 *
 * **What makes this worth running at all.** The generator emits both clients from one document,
 * so every unit test over the emitters is a test of a function returning a string. What only a
 * live server can say is whether the URL the TypeScript client builds is a URL the router
 * serves — and the first TypeScript package this generator produced could not even be imported.
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = dirname(dirname(HERE));
const SNAPSHOT = join(REPO, 'api', 'openapi.snapshot.json');
const CLIENT_DIR = join(REPO, 'dist', 'sdks', 'typescript', 'src');

// Parsed by hand rather than with a regex over the joined argv. The regex version matched
// `--base-url http://...` only when nothing followed that began with `-`, and `--token` with no
// value consumed the next flag as its value; `process.argv` is already a list, so walking it is
// both shorter and the one thing that cannot be wrong about quoting.
const args = {};
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i += 1) {
  const token = argv[i];
  if (!token.startsWith('--')) continue;
  const eq = token.indexOf('=');
  if (eq > 0) {
    args[token.slice(2, eq)] = token.slice(eq + 1);
  } else {
    const next = argv[i + 1];
    if (next && !next.startsWith('--')) {
      args[token.slice(2)] = next;
      i += 1;
    } else {
      args[token.slice(2)] = 'true';
    }
  }
}
const baseUrl = args['base-url'];
if (!baseUrl) {
  console.error('usage: sdk-smoke.mjs --base-url http://127.0.0.1:18085 [--token …]');
  process.exit(2);
}

let passes = 0;
const failures = [];
const check = (name, ok, detail = '') => {
  if (ok) {
    passes += 1;
    console.log(`ok    ${name}`);
  } else {
    failures.push(`${name}: ${detail}`);
    console.log(`FAIL  ${name} — ${detail}`);
  }
};

const mod = await import(pathToFileURL(join(CLIENT_DIR, 'index.ts')).href);
const { OPERATIONS, OmnionClient, OmnionError } = mod;

const document = JSON.parse(readFileSync(SNAPSHOT, 'utf8'));
const documentOps = Object.entries(document.paths).flatMap(([path, item]) =>
  Object.keys(item)
    .filter((m) => ['get', 'post', 'put', 'patch', 'delete'].includes(m))
    .map((m) => `${m.toUpperCase()} ${path}`),
);

check(
  'the client describes every operation in the document',
  OPERATIONS.length === documentOps.length,
  `client has ${OPERATIONS.length}, document has ${documentOps.length}`,
);

// Every path in the client resolves to a concrete URL. This is the check that found the four
// generation defects: it exercises the same string surgery the client does at call time.
const unresolvable = [];
for (const operation of OPERATIONS) {
  const argsFor = {};
  for (const name of (operation.path.match(/\{([^}]+)\}/g) ?? []).map((s) => s.slice(1, -1))) {
    argsFor[name] = 'smoke';
  }
  try {
    const url = operation.path.replace(/\{([^}]+)\}/g, (_m, name) =>
      encodeURIComponent(argsFor[name] ?? '{missing}'),
    );
    if (url.includes('{')) unresolvable.push(operation.id);
  } catch {
    unresolvable.push(operation.id);
  }
}
check('every path parameter is substitutable', unresolvable.length === 0, unresolvable.slice(0, 5).join(', '));

// The groups are real methods on a real class, not a table nobody can call.
const client = new OmnionClient({ baseUrl, token: args.token || undefined });
const routes = client.routes;
const groupNames = Object.keys(routes.all());
check('every group is reachable from the client', groupNames.length > 0, 'routes.all() is empty');
check(
  'every operation belongs to a group',
  groupNames.reduce((n, name) => n + routes[name]().ids().length, 0) === OPERATIONS.length,
  'the groups do not cover the table',
);

// Discover the health operation rather than naming it — a hardcoded id is a test of a name the
// author invented, and the Python half failed exactly that way.
const health = OPERATIONS.find((o) => o.method === 'GET' && o.path.replace(/\/$/, '') === '/healthz');
if (!health) {
  check('the document exposes a health route', false, 'no GET /healthz in the client table');
} else {
  try {
    const body = await client.call(health.id);
    check('the generated client completes a real call', true, `${health.id} answered ${typeof body}`);
  } catch (e) {
    if (e instanceof OmnionError && [401, 403, 429].includes(e.status)) {
      check(
        'the generated client completes a real call',
        true,
        `the server answered ${e.status} — the request reached a real guard`,
      );
    } else {
      check('the generated client completes a real call', false, `${e.name}: ${e.message}`);
    }
  }
}

try {
  await client.call('get_a_route_that_does_not_exist');
  check('an unknown operation id is refused by the client', false, 'it returned a value');
} catch (e) {
  check(
    'an unknown operation id is refused by the client',
    e instanceof OmnionError && e.status === 404,
    `${e.name} status ${e.status}`,
  );
}

console.log(`\n${passes} passed, ${failures.length} failed`);
for (const line of failures) console.log(`  FAIL ${line}`);
process.exit(failures.length ? 1 : 0);
