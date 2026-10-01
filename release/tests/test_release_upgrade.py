"""Unit tests for the upgrade plan builder (REQ-128, slice 4).

The request's acceptance line for this slice is one sentence with three claims in it:

    *the upgrade helper renders the ordered steps for compose and Kubernetes, splits
    application from database rollback, and refuses to show a complete checklist until a
    destructive migration is acknowledged.*

Two of the three are about REFUSAL, so most of what follows is the same shape as the bundle
tests: build a fixture that would render a dangerous plan, assert it is refused, and assert
WHICH refusal. A test that only builds the good plan passes on a helper with no checks in
it.

The centre of gravity is `unknown`. REQ-129 has not landed, so every plan built against
this repository today has an UNKNOWN destructiveness verdict, and the property that matters
is that `unknown` behaves like the dangerous case and not like the safe one: the database
rollback is not a down script, the point of no return is marked, and the checklist does not
render complete. Three of the tests below exist to make sure `unknown` cannot quietly
degrade into `reversible`.

Run: `python3 -m unittest discover -s release/tests -t .`
"""

import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "lib"))

import upgrade as plan_lib  # noqa: E402
import manifest as release  # noqa: E402

REPO = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))


def manifest_doc(version, migrations, **over):
    doc = {
        "manifest_version": "1",
        "version": version,
        "registry": "ghcr.io",
        "repository": "raksix/omnion",
        "core_min": "0.1.0",
        "migrations": list(migrations),
        "migrations_destructive": None,
    }
    doc.update(over)
    return doc


M1 = "0001_init.sql"
M2 = "0002_content.sql"
M3 = "0003_media.sql"


def _unreversible_in_repo() -> tuple[str, str]:
    """Two migrations from THIS repository that genuinely ship no down script.

    A plan rooted at REPO has to name files that exist on disk: `destructiveness` reads the
    real tree and intersects it with the delta by exact filename, so a delta of invented names
    (`0001_init.sql`, `0002_content.sql`) intersects to nothing and the plan answers
    `reversible` — while the repository holds 53 migrations with no reversal. That is the
    exact false promise REQ-129 exists to prevent, and it arrived here through the TEST
    fixtures rather than through the code under test, which is the harder kind of mistake to
    notice.
    """
    names = release.unreversible_migrations(REPO)
    assert len(names) >= 2, "the repository is expected to hold reversal-less migrations"
    return names[0], names[-1]


UNREV_A, UNREV_B = _unreversible_in_repo()


def _reversible(filename: str) -> str:
    """A migration file that carries a real reversal, in the shape `down.rs` reads.

    Written once because the reversals matter to three separate tests: a fixture of
    `-- ordinary migration\n` has no reversal block at all, so under the landed policy it is
    honestly `destructive`, and the tests that assert `reversible` would be asserting that a
    missing down script is fine. That is the exact false promise REQ-129 was written against,
    so the fixture has to carry the thing the assertion is about.
    """
    return (
        "-- the migration\n"
        "-- Down script (docs/05-VERSIONING.md)\n"
        "--   drop table %s;\n" % filename[: -len(".sql")]
    )


class VersionRange(unittest.TestCase):
    def test_a_plain_upgrade_builds(self):
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1, M2, M3]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            root=REPO,
        )
        self.assertEqual(plan["migrations_applied"], [M2, M3])

    def test_a_downgrade_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError) as caught:
            plan_lib.build_plan(
                from_version="0.5.0",
                to_manifest=manifest_doc("0.4.0", [M1]),
                from_manifest=manifest_doc("0.5.0", [M1, M2]),
                root=REPO,
            )
        self.assertIn("older than the running", str(caught.exception))

    def test_the_same_version_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib.build_plan(
                from_version="0.5.0",
                to_manifest=manifest_doc("0.5.0", [M1]),
                from_manifest=manifest_doc("0.5.0", []),
                root=REPO,
            )

    def test_ten_is_newer_than_nine(self):
        # The reason the comparison is not a string one. 0.10.0 > 0.9.0 numerically and
        # < 0.9.0 as text, so a plan built with `sorted()` would refuse a valid upgrade.
        plan = plan_lib.build_plan(
            from_version="0.9.0",
            to_manifest=manifest_doc("0.10.0", [M1]),
            from_manifest=manifest_doc("0.9.0", []),
            root=REPO,
        )
        self.assertEqual(plan["to_version"], "0.10.0")

    def test_a_non_version_is_refused_not_defaulted(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("latest", [M1]),
                from_manifest=manifest_doc("0.4.0", []),
                root=REPO,
            )

    def test_a_missing_source_manifest_is_refused(self):
        # The delta is a DIFFERENCE. With one list, every shipped migration reads as new and
        # the plan tells the operator to re-apply what their install already has.
        with self.assertRaises(plan_lib.UpgradeError) as caught:
            plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("0.5.0", [M1, M2]),
                root=REPO,
            )
        self.assertIn("from_manifest is required", str(caught.exception))

    def test_an_unknown_topology_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("0.5.0", [M1]),
                from_manifest=manifest_doc("0.4.0", []),
                topology="nomad",
                root=REPO,
            )


class Destructiveness(unittest.TestCase):
    """The property that must not degrade: unknown is not safe."""

    def test_absent_policy_yields_unknown_not_reversible(self):
        # The property is the PAIRING, and it is tested in a temp tree rather than against
        # this repository. It used to assert `assertFalse(_policy_exists(REPO))` — true when
        # written, false the moment REQ-129 landed, so the test that exists to catch the
        # helper lying to an operator started failing for a reason that had nothing to do
        # with the helper. A gate that asserts a snapshot of the repo is a gate that reports
        # the repo, not the code.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write("-- ordinary migration\n")
            self.assertFalse(release._policy_exists(tmp))
            verdict = plan_lib.destructiveness([M2], manifest_doc("0.5.0", [M1, M2]), tmp)
            self.assertEqual(verdict["verdict"], plan_lib.VERDICT_UNKNOWN)
            self.assertEqual(verdict["database_rollback"], "unknown")
            self.assertIn("REQ-129", verdict["reason"])

    def test_the_landed_policy_makes_a_reversal_less_migration_destructive(self):
        # The other half of the pairing, and the case that was silently unreachable before:
        # with the policy landed and no `-- omnion:no-down` marker, a migration that has no
        # reversal must NOT come back `reversible`. It has no down script, so the database
        # rollback is a restore, and `destructive_migrations` is what establishes it.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write("-- ordinary migration\n")
            with open(os.path.join(directory, "0207_migration_safety.sql"), "w") as handle:
                handle.write("-- policy\n")
            self.assertTrue(release._policy_exists(tmp))
            verdict = plan_lib.destructiveness([M2], manifest_doc("0.5.0", [M1, M2]), tmp)
            self.assertEqual(verdict["verdict"], plan_lib.VERDICT_DESTRUCTIVE)
            self.assertEqual(verdict["database_rollback"], "restore-from-backup")

    def test_a_reversible_migration_is_not_destructive_under_the_landed_policy(self):
        # …and the case that must stay True, or the rule above degenerates into "always
        # destructive" and stops being information. A migration WITH an executable reversal
        # is reversible, and says so.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write(
                        "-- Down script (docs/05-VERSIONING.md)\n"
                        "--   drop table %s;\n" % name.replace(".sql", "")
                    )
            with open(os.path.join(directory, "0207_migration_safety.sql"), "w") as handle:
                handle.write(
                    "-- Down script (docs/05-VERSIONING.md)\n"
                    "--   drop table migration_safety;\n"
                )
            self.assertEqual(release.destructive_migrations(tmp), [])
            verdict = plan_lib.destructiveness([M2], manifest_doc("0.5.0", [M1, M2]), tmp)
            self.assertEqual(verdict["verdict"], plan_lib.VERDICT_REVERSIBLE)

    def test_a_no_down_marker_in_the_range_makes_it_destructive(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2, M3):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write("-- ordinary migration\n")
            with open(os.path.join(directory, M2), "a", encoding="utf-8") as handle:
                handle.write("-- omnion:no-down\n")
            verdict = plan_lib.destructiveness([M2], manifest_doc("0.5.0", [M1, M2, M3]), tmp)
        self.assertEqual(verdict["verdict"], plan_lib.VERDICT_DESTRUCTIVE)
        self.assertEqual(verdict["destructive_migrations"], [M2])
        self.assertEqual(verdict["database_rollback"], "restore-from-backup")

    def test_a_marker_outside_the_range_does_not_make_the_plan_destructive(self):
        # The marker list is a property of the FILES, not of the upgrade. A destructive
        # migration the operator already applied does not make THIS upgrade destructive.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2, M3):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write(
                        "-- omnion:no-down\n" if name == M1 else "-- ordinary migration\n"
                    )
            verdict = plan_lib.destructiveness([M3], manifest_doc("0.5.0", [M1, M2, M3]), tmp)
        self.assertNotEqual(verdict["verdict"], plan_lib.VERDICT_DESTRUCTIVE)

    def test_the_manifest_flag_is_believed_when_it_says_true(self):
        verdict = plan_lib.destructiveness(
            [M2], manifest_doc("0.5.0", [M1, M2], migrations_destructive=True), REPO
        )
        self.assertEqual(verdict["verdict"], plan_lib.VERDICT_DESTRUCTIVE)
        self.assertEqual(verdict["source"], "manifest")

    def test_a_manifest_flag_of_false_does_not_launder_an_unreversible_migration(self):
        # `migrations_destructive: false` from a publisher is a claim about the publisher's
        # tree, and the helper reads the tree itself. So the flag may never turn an
        # unreversible migration into a `reversible` verdict — that is the invariant, and it is
        # what the original test meant.
        #
        # The assertion was `VERDICT_UNKNOWN` against a `destructiveness()` call rooted at
        # REPO, which pinned "the gate has not landed". It has: `0207_migration_safety.sql` is
        # in the tree, so the honest answer for a migration with no down script is
        # `destructive`, and asserting `unknown` here would have been asserting the
        # pre-REQ-129 world again. `false` is the publisher's silence; the gate is what turns
        # silence into an answer, and both of those are consistent with `destructive`.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write("-- ordinary migration\n")
            with open(os.path.join(directory, "0207_migration_safety.sql"), "w") as handle:
                handle.write(_reversible("0207_migration_safety.sql"))
            verdict = plan_lib.destructiveness(
                [M2],
                manifest_doc("0.5.0", [M1, M2], migrations_destructive=False),
                tmp,
            )
        self.assertEqual(verdict["verdict"], plan_lib.VERDICT_DESTRUCTIVE)
        self.assertEqual(
            verdict["source"],
            "missing-down-script",
            "the reason names what was found, not the manifest's flag",
        )

    def test_every_verdict_is_one_of_three(self):
        self.assertEqual(set(plan_lib.VERDICTS), {"reversible", "destructive", "unknown"})


class RollbackSplit(unittest.TestCase):
    def test_the_plan_splits_application_from_database(self):
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [UNREV_A, UNREV_B]),
            from_manifest=manifest_doc("0.4.0", [UNREV_A]),
            root=REPO,
        )
        self.assertTrue(plan["rollback"]["application"]["available"])
        self.assertIsNotNone(plan["rollback"]["application"]["command"])
        # Application rollback is ALWAYS available — the previous tag exists. Database
        # rollback is not, and the two must not be rendered as one availability flag.
        #
        # The verdict is `destructive`, not the `unknown` this asserted when REQ-129's gate
        # had not landed: 53 of this tree's migrations ship no down script, so under a landed
        # gate the honest answer is "restore from backup". `available` stays False either way,
        # which is the property that matters — the test changed its expected REASON, not its
        # promise to the operator.
        self.assertFalse(plan["rollback"]["database"]["available"])
        self.assertEqual(plan["rollback"]["database"]["verdict"], plan_lib.VERDICT_DESTRUCTIVE)

    def test_a_reversible_range_offers_the_down_script(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write(_reversible(name))
            # A policy migration is what makes the absence of a marker mean something.
            with open(os.path.join(directory, "0207_migration_safety.sql"), "w") as handle:
                handle.write(_reversible("0207_migration_safety.sql"))
            plan = plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("0.5.0", [M1, M2]),
                from_manifest=manifest_doc("0.4.0", [M1]),
                root=tmp,
            )
        self.assertEqual(plan["destructive"]["verdict"], plan_lib.VERDICT_REVERSIBLE)
        self.assertTrue(plan["rollback"]["database"]["available"])
        self.assertEqual(plan["rollback"]["database"]["method"], "down-script")
        # A reversible upgrade has no point of no return. Marking one would train operators
        # to ignore the marker that matters.
        self.assertIsNone(plan["point_of_no_return"])


class PointOfNoReturn(unittest.TestCase):
    def _plan(self, topology="compose"):
        # Real names, not `M1`/`M2`. `destructiveness` intersects the tree with the delta by
        # exact filename, so a delta of invented names matches nothing and the plan comes back
        # `reversible` — which is why this helper's earlier revision produced a plan with no
        # marker at all and looked like a bug in `point_of_no_return` rather than a fixture
        # that named files the repository does not contain.
        return plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [UNREV_A, UNREV_B]),
            from_manifest=manifest_doc("0.4.0", [UNREV_A]),
            topology=topology,
            root=REPO,
        )

    def test_a_reversible_plan_carries_no_marker(self):
        # A plan whose database rollback is available must NOT carry a point of no return:
        # marking one there trains operators to ignore the marker that matters. Rooted at a
        # tree where both migrations ship reversals, which is the only way to get there now
        # that this repository honestly reports `destructive`.
        with tempfile.TemporaryDirectory() as tmp:
            directory = os.path.join(tmp, "database", "migrations")
            os.makedirs(directory)
            for name in (M1, M2):
                with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
                    handle.write(_reversible(name))
            with open(os.path.join(directory, "0207_migration_safety.sql"), "w") as handle:
                handle.write(_reversible("0207_migration_safety.sql"))
            for topology in plan_lib.TOPOLOGIES:
                with self.subTest(topology=topology):
                    plan = plan_lib.build_plan(
                        from_version="0.4.0",
                        to_manifest=manifest_doc("0.5.0", [M1, M2]),
                        from_manifest=manifest_doc("0.4.0", [M1]),
                        root=tmp,
                        topology=topology,
                    )
                    self.assertEqual(plan["destructive"]["verdict"], plan_lib.VERDICT_REVERSIBLE)
                    self.assertFalse(any(s["point_of_no_return"] for s in plan["steps"]))

    def test_an_unreversible_plan_marks_exactly_the_migration_step(self):
        # The other half, and the one that matters to an operator: a release whose database
        # rollback is a restore DOES have a point of no return, and it sits on the migration.
        # This was the assertion the old test made against REPO, where it passed for the wrong
        # reason — REPO was `unknown` then, and `unknown` marks too. Now it is `destructive`
        # and the marker is there for the right one.
        for topology in plan_lib.TOPOLOGIES:
            with self.subTest(topology=topology):
                plan = self._plan(topology)
                marked = [i for i, s in enumerate(plan["steps"]) if s["point_of_no_return"]]
                self.assertEqual(len(marked), 1)
                self.assertEqual(plan["steps"][marked[0]]["kind"], "migrate")

    def test_the_migration_precedes_the_deploy(self):
        # The request's rule: migrations run before new code serves traffic.
        for topology in plan_lib.TOPOLOGIES:
            with self.subTest(topology=topology):
                kinds = [s["kind"] for s in self._plan(topology)["steps"]]
                self.assertLess(kinds.index("migrate"), kinds.index("deploy"))
                self.assertLess(kinds.index("backup"), kinds.index("migrate"))

    def test_no_marker_without_migrations(self):
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            root=REPO,
        )
        self.assertEqual(plan["migrations_applied"], [])
        self.assertIsNone(plan["point_of_no_return"])
        self.assertFalse(any(s["point_of_no_return"] for s in plan["steps"]))

    def test_the_resolver_and_the_builder_agree(self):
        plan = self._plan()
        selector = plan_lib.point_of_no_return(
            plan["migrations_applied"], plan["destructive"]
        )
        self.assertEqual(plan_lib.resolve_point_of_no_return(selector, plan["steps"]), plan["point_of_no_return"])


class ChecklistGate(unittest.TestCase):
    def _plan(self, **over):
        return plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [UNREV_A, UNREV_B]),
            from_manifest=manifest_doc("0.4.0", [UNREV_A]),
            root=REPO,
            **over,
        )

    def test_an_unacknowledged_plan_does_not_render_complete(self):
        plan = self._plan()
        self.assertTrue(plan["checklist"]["requires_acknowledgement"])
        self.assertFalse(plan["checklist"]["complete"])
        self.assertTrue(plan["checklist"]["items"])

    def test_acknowledging_completes_it_and_records_the_verdict(self):
        plan = self._plan(acknowledged=True, acknowledged_by="ops@example.com")
        self.assertTrue(plan["checklist"]["complete"])
        self.assertEqual(plan["destructive"]["acknowledged_by"], "ops@example.com")
        # The verdict recorded is the one that was acknowledged. An operator who ticked
        # "destructive" on an unknown plan is a different, more dangerous act than one who
        # ticked it on a known one, and the record has to say which.
        # The recorded verdict is the one the plan carries. It used to be asserted as
        # `VERDICT_UNKNOWN` against a REPO-rooted plan, which pinned the pre-REQ-129 answer;
        # the property is that acknowledgement RECORDS the verdict rather than inventing a
        # softer one, and that holds for whichever verdict the range earned.
        self.assertEqual(
            plan["destructive"]["acknowledged_verdict"],
            plan["destructive"]["verdict"],
            "acknowledging must record the verdict, not replace it",
        )

    def test_the_checklist_lists_the_migration_step(self):
        plan = self._plan()
        self.assertTrue(any(i["kind"] == "migrate" for i in plan["checklist"]["items"]))


class Steps(unittest.TestCase):
    def test_compose_names_a_stack_file_the_repository_ships(self):
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1, M2]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            root=REPO,
        )
        for step in plan["steps"]:
            for token in (step.get("command") or "").split():
                if "docker-compose" in token and token.endswith(".yml"):
                    self.assertTrue(
                        os.path.exists(os.path.join(REPO, "infra", "compose", token)),
                        f"{token} is named by a step but is not in the repository",
                    )

    def test_both_topologies_render_every_step_kind(self):
        seen = {}
        for topology in plan_lib.TOPOLOGIES:
            plan = plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("0.5.0", [M1, M2]),
                from_manifest=manifest_doc("0.4.0", [M1]),
                topology=topology,
                root=REPO,
            )
            seen[topology] = {s["kind"] for s in plan["steps"]}
            for step in plan["steps"]:
                self.assertTrue(step["text"].strip(), "a step with no text renders as nothing")
        for topology, kinds in seen.items():
            with self.subTest(topology=topology):
                self.assertIn("backup", kinds)
                self.assertIn("migrate", kinds)
                self.assertIn("deploy", kinds)
                self.assertIn("verify", kinds)

    def test_the_enterprise_stack_is_reachable(self):
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1, M2]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            bundle_kind="compose-enterprise",
            root=REPO,
        )
        self.assertIn(
            "docker-compose.enterprise.yml",
            " ".join(s["command"] or "" for s in plan["steps"]),
        )

    def test_an_unknown_bundle_kind_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib.build_plan(
                from_version="0.4.0",
                to_manifest=manifest_doc("0.5.0", [M1, M2]),
                from_manifest=manifest_doc("0.4.0", [M1]),
                bundle_kind="compose-nope",
                root=REPO,
            )

    def test_a_verify_step_carries_a_check_rather_than_an_unrunnable_command(self):
        # The first version built `curl -fsS https://<your domain>/readyz` — a command an
        # operator pastes verbatim, on a production host, where it fails. A verification step
        # that cannot be executed is worse than one that says what to check.
        plan = plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1, M2]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            root=REPO,
        )
        verify = [s for s in plan["steps"] if s["kind"] == "verify"][0]
        self.assertIsNone(verify["command"])
        self.assertEqual(verify["check"]["path"], "/readyz")

    def test_a_step_command_carrying_a_credential_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib._step("deploy", "x", "docker login -u me -p s3cr3t-fixture-9f2b1c")

    def test_an_unknown_step_kind_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib._step("teleport", "x", "true")


class Verification(unittest.TestCase):
    def _plan(self, topology="compose", **over):
        return plan_lib.build_plan(
            from_version="0.4.0",
            to_manifest=manifest_doc("0.5.0", [M1, M2]),
            from_manifest=manifest_doc("0.4.0", [M1]),
            topology=topology,
            root=REPO,
            **over,
        )

    def test_a_real_plan_verifies_on_both_topologies(self):
        for topology in plan_lib.TOPOLOGIES:
            with self.subTest(topology=topology):
                verdict = plan_lib.verify_plan(self._plan(topology), REPO)
                self.assertEqual(verdict["state"], "verified", verdict["problems"])
                self.assertEqual(verdict["problems"], [])

    def test_a_stack_file_that_does_not_exist_is_caught(self):
        plan = self._plan()
        plan["steps"][0]["command"] = plan["steps"][0]["command"].replace(
            "docker-compose.prod.yml", "docker-compose.yml"
        )
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")
        self.assertIn("docker-compose.yml", " ".join(verdict["problems"]))

    def test_a_deploy_step_rolling_to_the_wrong_tag_is_caught(self):
        plan = self._plan()
        for step in plan["steps"]:
            if step["kind"] == "deploy":
                step["image"] = "ghcr.io/raksix/omnion/api:9.9.9"
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")

    def test_a_point_of_no_return_on_a_deploy_step_is_caught(self):
        # The marker on the wrong step is worse than no marker: it tells an operator the
        # point of no return is the deploy, so they roll the app back and assume the schema
        # followed it.
        plan = self._plan()
        for step in plan["steps"]:
            step["point_of_no_return"] = step["kind"] == "deploy"
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")
        self.assertIn("point of no return", " ".join(verdict["problems"]))

    def test_two_markers_are_caught(self):
        plan = self._plan()
        plan["steps"][0]["point_of_no_return"] = True
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")

    def test_a_complete_unacknowledged_checklist_is_caught(self):
        plan = self._plan()
        plan["checklist"]["complete"] = True
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")
        self.assertIn("unacknowledged", " ".join(verdict["problems"]))

    def test_an_acknowledgement_contradicting_the_verdict_is_caught(self):
        plan = self._plan(acknowledged=True, acknowledged_by="ops")
        plan["destructive"]["acknowledged_verdict"] = plan_lib.VERDICT_DESTRUCTIVE
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")

    def test_an_unrecognised_verdict_is_caught(self):
        # Anything that is not one of the three must be refused, not defaulted. A helper
        # that treats an unknown string as `reversible` is the failure this exists to stop.
        plan = self._plan()
        plan["destructive"]["verdict"] = "probably fine"
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")
        self.assertIn("must not be read as 'safe'", " ".join(verdict["problems"]))

    def test_a_migration_the_release_does_not_ship_is_caught(self):
        plan = self._plan()
        plan["migrations_applied"] = [M2, "0099_never_shipped.sql"]
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")
        self.assertIn("does not ship", " ".join(verdict["problems"]))

    def test_a_destructive_plan_that_promises_a_down_script_is_caught(self):
        plan = self._plan()
        plan["destructive"]["verdict"] = plan_lib.VERDICT_DESTRUCTIVE
        plan["destructive"]["database_rollback"] = "down-script"
        verdict = plan_lib.verify_plan(plan, REPO)
        self.assertEqual(verdict["state"], "failed")

    def test_a_plan_that_names_a_rollback_method_it_has_not_earned_is_caught(self):
        # The invariant is per-verdict, and the two halves are separate assertions for a
        # reason: the plan this class builds is `destructive` now (its migrations ship no
        # down script), so a single assertion written against `unknown` no longer exercises
        # the branch it was written for — it passed or failed for a reason that had moved.
        # A gate that only tests one branch of a rule tests that branch.
        for verdict, borrowed in (
            (plan_lib.VERDICT_DESTRUCTIVE, "down-script"),
            (plan_lib.VERDICT_UNKNOWN, "down-script"),
            (plan_lib.VERDICT_REVERSIBLE, "restore-from-backup"),
        ):
            with self.subTest(verdict=verdict):
                plan = self._plan()
                plan["destructive"]["verdict"] = verdict
                plan["destructive"]["database_rollback"] = borrowed
                result = plan_lib.verify_plan(plan, REPO)
                self.assertEqual(
                    result["state"],
                    "failed",
                    f"a {verdict} plan may not claim {borrowed}",
                )
                self.assertTrue(result["problems"], "a refusal must say what it refused")


class Cli(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)
        self.to = os.path.join(self.dir, "to.json")
        self.frm = os.path.join(self.dir, "from.json")
        json.dump(manifest_doc("0.5.0", [M1, M2]), open(self.to, "w"))
        json.dump(manifest_doc("0.4.0", [M1]), open(self.frm, "w"))

    def test_a_plan_built_from_files_verifies(self):
        plan = plan_lib.plan_from_files("0.4.0", self.to, self.frm, "compose", "compose-small", REPO)
        self.assertEqual(plan["verification"]["state"], "verified")

    def test_a_missing_manifest_file_is_refused(self):
        with self.assertRaises(plan_lib.UpgradeError) as caught:
            plan_lib.plan_from_files("0.4.0", os.path.join(self.dir, "nope.json"), self.frm,
                                     "compose", "compose-small", REPO)
        self.assertIn("not found", str(caught.exception))

    def test_a_malformed_manifest_is_refused(self):
        bad = os.path.join(self.dir, "bad.json")
        with open(bad, "w", encoding="utf-8") as handle:
            handle.write("{not json")
        with self.assertRaises(plan_lib.UpgradeError):
            plan_lib.plan_from_files("0.4.0", bad, self.frm, "compose", "compose-small", REPO)

    def test_main_exits_zero_on_a_verified_plan(self):
        code = plan_lib.main([
            "--from-version", "0.4.0", "--to-manifest", self.to, "--from-manifest", self.frm,
        ])
        self.assertEqual(code, 0)

    def test_main_exits_nonzero_on_a_downgrade(self):
        code = plan_lib.main([
            "--from-version", "0.9.0", "--to-manifest", self.to, "--from-manifest", self.frm,
        ])
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
