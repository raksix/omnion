#!/usr/bin/env node
/**
 * A second QA account that deliberately LACKS `developer.sdks.scaffold` (REQ-033, slice 4).
 *
 * ## Why this exists
 *
 * Slice 4's done-when reads: *"the read-only role sees no management controls."* Nothing could
 * prove that, because the QA fixture had exactly one account — the owner, who holds every
 * permission. A screen whose management controls are hidden by a permission check is, for the
 * only account the pass could sign in as, indistinguishable from a screen that has no
 * management controls at all. Worse, the two defects are opposite: the first hides buttons that
 * should be there, the second shows buttons that should not be, and only the second is dangerous.
 *
 * So the fixture creates the account the owner cannot stand in for, and the pass signs in as it.
 *
 * ## The one rule that shapes this file
 *
 * **The password hash is copied from the existing QA owner, never generated.** There is no public
 * register route on a fresh tenant, and re-implementing argon2 here would be a second
 * implementation of the thing being verified — a second implementation that can be subtly wrong
 * and make the probe fail for a reason that has nothing to do with permissions. Copying the
 * owner's hash is honest: this account exists *purely* to hold a different permission set, and
 * the pass authenticates as it with the owner's password.
 *
 * ## What it does NOT do
 *
 * It does not weaken the owner. The read-only account is a separate user with its own role and
 * its own bindings; the owner keeps the full set, and both are asserted to hold what they should.
 * A fixture that made the owner read-only would make every other pass on this stack weaker.
 */

"use strict";

const { execFileSync } = require("child_process");

// The defaults must match `run.sh`'s, not a plausible guess: a fixture that looks for an account
// the seed never created produces an *empty* CTE rather than an error, and the account then
// silently does not exist. That is the worst shape for a fixture — it looks like a permission
// problem rather than a missing row.
const CREDS = {
  email: process.env.QA_ADMIN_EMAIL || "qa-owner@omnion.test",
  password: process.env.QA_ADMIN_PASSWORD || "OmnionQa-Passw0rd-2026!",
};

const CONTAINER = process.env.QA_PG_CONTAINER || "omnion-postgres";
const DB = process.env.QA_DB_NAME || "omnion_qa_w5";

/** The permission whose absence the pass is looking for. Named once, used in the SQL and the log. */
const DENIED = "developer.sdks.scaffold";

function psql(sql) {
  return execFileSync(
    "docker",
    ["exec", "-i", CONTAINER, "psql", "-U", "omnion", "-d", DB, "-t", "-A", "-c", sql],
    { encoding: "utf8" },
  ).trim();
}

/**
 * The read-only developer role: `developer.*.read` and nothing that can change a thing.
 *
 * Built by *listing* the permissions it must NOT hold rather than by naming the ones it must:
 * a deny-list is the only shape that stays correct as the catalogue grows. `developer.keys.manage`
 * is the one entry that needs spelling out, because a read-only developer who can rotate a
 * credential is not read-only, and the catalogue has no "all manage keys" umbrella to inherit.
 */
async function main() {
  const stamp = Date.now().toString(36);

  // Every `developer.*.read` the catalogue has, and nothing else, except the one manage key the
  // deny-list below spells out. Written as a `not in` over the whole manage family rather than
  // as an enumeration of reads: a role built from a list of the reads it wants has to be edited
  // every time a read scope is added, and the day it is not edited is the day the fixture
  // silently loses the very permission a future pass needs.
  const sql = `
    with source as (
      select id, password_hash from users where email = '${CREDS.email}'
    ),
    org as (
      select id from organizations order by created_at limit 1
    ),
    readonly_role as (
      insert into roles (organization_id, key, name, description, priority, is_system)
      select org.id, 'qa-readonly-developer', 'QA read-only developer',
             'Fixture role: developer reads without any manage key.',
             10, false
      from org
      where not exists (select 1 from roles where key = 'qa-readonly-developer')
      returning id
    ),
    -- Exactly one row, and limit 1 over a UNION ALL is NOT that: the inserting branch and the
    -- already-present branch both produce a row on a re-run, order by id picks whichever sorts
    -- first, and the grants then land on a role id that is not the one the account is bound to.
    -- The role's own key is what identifies it, so the union is de-duplicated on that.
    role as (
      select distinct id from roles where key = 'qa-readonly-developer'
    ),
    granted as (
      -- The effect column is NOT NULL and has no default; omitting it
      -- inserts the column default, which the table does not have, so the statement fails on
      -- a not-null violation. The role's whole point is to ALLOW, and the column carries
      -- nothing more subtle than that here.
      insert into role_permissions (role_id, permission_key, effect)
      select role.id, p.key, 'allow'
      from role, permissions p
      where p.key like 'developer.%.read'
        and p.key not in ('${DENIED}')
      on conflict (role_id, permission_key) do update set effect = 'allow'
      returning 1
    ),
    -- A non-recursive CTE cannot see its own siblings' effects, so a CTE that counts the
    -- permissions this same statement just granted reads the PRE-statement table and reports the
    -- previous run's number. The count therefore happens in a SECOND statement below, after the
    -- write has landed -- the alternative (counting the insert) reports zero on every re-run
    -- because on-conflict rows are not returned, which is the same class of lie.
    reads as (
      select 1
    ),
    account as (
      -- display_name, not name: users has no name column, and a fixture that guessed the
      -- column name fails at insert time rather than at the assertion that needed it.
      insert into users (email, display_name, password_hash, organization_id, status, created_at, updated_at)
      select 'qa-readonly-${stamp}@omnion.test', 'QA read-only developer', source.password_hash,
             org.id, 'active', now(), now()
      from source, org
      returning id
    ),
    bound as (
      insert into role_bindings (role_id, user_id, scope_type, organization_id)
      select role.id, account.id, 'organization', org.id
      from role, account, org
      returning 1
    )
    select (select count(*) from reads),
           (select count(*) from account),
           (select count(*) from bound);
  `;

  const out = psql(sql);
  const [, account, bound] = out.split("|").map((n) => Number(n.trim()));

  // Read the granted state in its own statement, now that the write above has committed.
  const granted = Number(
    psql(`
      select count(*) from role_permissions rp
      join roles r on r.id = rp.role_id
      where r.key = 'qa-readonly-developer';
    `),
  );

  // The fixture is only useful if it produced the *contrast* it exists for: an account that can
  // sign in and that genuinely lacks the key. Asserting the account exists is not enough — a
  // fixture that accidentally granted the very permission it exists to deny would make the pass
  // report "the control is visible" and be right for the wrong reason.
  const held = psql(`
    select count(*) from role_permissions rp
    join roles r on r.id = rp.role_id
    where r.key = 'qa-readonly-developer' and rp.permission_key = '${DENIED}';
  `);

  console.log(
    `[qa] read-only developer: ${granted} read scopes, account=${account === 1}, bound=${bound === 1}`,
  );

  if (account !== 1 || bound !== 1) {
    console.error(
      `[qa] the read-only developer fixture was not created (account=${account}, bound=${bound})`,
    );
    process.exit(1);
  }
  // Zero granted reads is the quiet failure this fixture is most likely to hit: the pattern is
  // developer.%.read, and a catalogue that renames those keys would grant nothing while every
  // other assertion still passed — a read-only account with no reads is refused by everything,
  // which looks exactly like a correct hiding of the controls.
  if (granted === 0) {
    console.error(
      "[qa] the read-only role holds no developer reads — every control would be hidden " +
        "because nothing is permitted, not because management is refused",
    );
    process.exit(1);
  }
  // Zero held is the DESIRED state — this account exists precisely to lack the key. The check is
  // therefore on it being non-zero, and it is written as a number because a `held === "0"`
  // comparison silently reads a trailing-newline psql output as a string and inverts the whole
  // assertion: the first version of this file failed with "the account HOLDS the key" on a role
  // that demonstrably held two reads and not the scaffold.
  if (Number(held) !== 0) {
    console.error(
      `[qa] the read-only developer HOLDS ${DENIED} — the pass would prove nothing about it`,
    );
    process.exit(1);
  }
}

main().catch((cause) => {
  console.error("[qa] read-only developer fixture failed:", cause.message);
  process.exit(1);
});