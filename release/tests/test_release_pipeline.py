#!/usr/bin/env python3
"""Unit tests for the release pipeline's decision layer (REQ-128, slice 3).

Stdlib `unittest`, run the same way as `test_release_manifest.py` so there is one command
and no dependency to install on a build box.

What these cover is the part a gate script cannot: the refusal logic's individual branches
and their exact wording, the plan's shape, the two archive digests, and the credential
scanner's decisions on the forms that actually occur in this repository's compose stacks.
The mutation suite in `scripts/qa/release-pipeline.sh` proves the rules can fail; these
prove they fail for the right reason.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

# `release/lib`, two levels up from `release/tests` — the same path the manifest tests use,
# so both suites import the SAME module objects rather than two copies of the source.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "lib"))

import manifest as release_manifest  # noqa: E402
import pipeline as release_pipeline  # noqa: E402

ROOT = release_manifest.repo_root()


class PlanShape(unittest.TestCase):
    """The plan is derived from the repository, so these assert against the repository."""

    def test_plan_is_not_empty(self):
        self.assertTrue(release_pipeline.plan(ROOT))

    def test_one_stage_per_dockerfile_runtime(self):
        stages = release_pipeline.plan(ROOT)
        images = {s["target"] for s in stages if s["stage_kind"] == "image"}
        expected = {c for cs in release_manifest.dockerfile_targets(ROOT).values() for c in cs}
        self.assertEqual(images, expected)

    def test_web_is_in_the_plan(self):
        """The image both compose stacks and the chart deploy, and the one a
        file-name-derived list drops."""
        targets = {s["target"] for s in release_pipeline.plan(ROOT) if s["stage_kind"] == "image"}
        self.assertIn("web", targets)

    def test_one_cli_stage_per_documented_platform(self):
        cli = {s["target"] for s in release_pipeline.plan(ROOT) if s["stage_kind"] == "cli"}
        self.assertEqual(cli, set(release_manifest.CLI_PLATFORMS))

    def test_every_stage_kind_is_known(self):
        for stage in release_pipeline.plan(ROOT):
            self.assertIn(stage["stage_kind"], release_pipeline.STAGES, stage["id"])

    def test_sbom_depends_on_its_image(self):
        stages = release_pipeline.plan(ROOT)
        by_id = {s["id"]: s for s in stages}
        for stage in stages:
            if stage["stage_kind"] != "sbom":
                continue
            parent = "image:" + str(stage["target"])
            self.assertIn(parent, by_id, stage["id"])
            self.assertIn(parent, stage["depends_on"], stage["id"])

    def test_manifest_depends_on_everything(self):
        stages = release_pipeline.plan(ROOT)
        manifest_stage = [s for s in stages if s["stage_kind"] == "manifest"]
        self.assertEqual(len(manifest_stage), 1)
        others = {s["id"] for s in stages if s["stage_kind"] != "manifest"}
        self.assertEqual(others - set(manifest_stage[0]["depends_on"]), set())

    def test_chart_artifact_is_named_for_the_version(self):
        version = release_manifest.workspace_version(ROOT)
        charts = [s for s in release_pipeline.plan(ROOT, version) if s["stage_kind"] == "chart"]
        for stage in charts:
            self.assertTrue(stage["name"].endswith(f"-{version}.tgz"))

    def test_a_plan_for_another_version_renames_the_chart(self):
        stages = release_pipeline.plan(ROOT, "9.9.9")
        charts = [s for s in stages if s["stage_kind"] == "chart"]
        self.assertTrue(charts)
        for stage in charts:
            self.assertTrue(stage["name"].endswith("-9.9.9.tgz"))


class StageAvailability(unittest.TestCase):
    def test_unknown_stage_kind_is_not_available(self):
        ok, reason = release_pipeline.stage_availability({"stage_kind": "telepathy"})
        self.assertFalse(ok)
        self.assertIn("no stage specification", reason)

    def test_every_planned_stage_probes_without_raising(self):
        for stage in release_pipeline.plan(ROOT):
            ok, reason = release_pipeline.stage_availability(stage)
            self.assertIsInstance(ok, bool, stage["id"])
            self.assertIsInstance(reason, str, stage["id"])


class Refusals(unittest.TestCase):
    """`publish_refusals` — the thing that decides whether a tag ships."""

    GREEN = {
        "gates": {
            "tests": {"state": "green", "reason": ""},
            "manifest": {"state": "green", "reason": ""},
            "migration-verification": {"state": "green", "reason": ""},
        },
        "missing_required": [],
    }
    COMPLETE = {"_synthetic": False, "_blocked_stages": []}

    def publish(self, **overrides):
        tag = overrides.pop("tag", "v0.1.0")
        version = overrides.pop("version", "0.1.0")
        return release_pipeline.publish_refusals(tag, version, **overrides)

    def test_a_fully_green_release_is_publishable(self):
        self.assertEqual(
            self.publish(gates_report=self.GREEN, facts=self.COMPLETE, attestation=True), []
        )

    def test_a_mismatched_tag_is_refused(self):
        refusals = self.publish(
            tag="v9.9.9", gates_report=self.GREEN, facts=self.COMPLETE, attestation=True
        )
        self.assertTrue(any("does not name the version" in r for r in refusals))

    def test_a_tag_without_the_v_prefix_still_matches(self):
        self.assertEqual(
            self.publish(
                tag="0.1.0", gates_report=self.GREEN, facts=self.COMPLETE, attestation=True
            ),
            [],
        )

    def test_a_prerelease_may_not_reach_stable(self):
        gates = dict(self.GREEN)
        refusals = self.publish(
            tag="v1.0.0-rc.1", version="1.0.0-rc.1", gates_report=gates,
            facts=self.COMPLETE, attestation=True,
        )
        self.assertTrue(any("pre-release" in r for r in refusals))

    def test_a_prerelease_on_a_prerelease_channel_is_fine(self):
        gates = dict(self.GREEN)
        refusals = self.publish(
            tag="v1.0.0-rc.1", version="1.0.0-rc.1", gates_report=gates,
            facts=self.COMPLETE, attestation=True, channel="beta",
        )
        self.assertFalse(any("pre-release" in r for r in refusals))

    def test_a_red_gate_is_refused_by_name(self):
        gates = {
            "gates": {"manifest": {"state": "red", "reason": "themes/minimal declares 0.1.1"}},
            "missing_required": [],
        }
        refusals = self.publish(gates_report=gates, facts=self.COMPLETE, attestation=True)
        self.assertTrue(any("red" in r and "manifest" in r for r in refusals))
        self.assertTrue(any("0.1.1" in r for r in refusals))

    def test_an_unrunnable_gate_is_not_the_same_as_green(self):
        gates = {"gates": {"manifest": {"state": "unrunnable", "reason": "no command"}},
                 "missing_required": []}
        refusals = self.publish(gates_report=gates, facts=self.COMPLETE, attestation=True)
        self.assertTrue(any("could not be run" in r for r in refusals))

    def test_a_required_gate_missing_from_the_list_is_refused(self):
        gates = {"gates": {"tests": {"state": "green", "reason": ""}},
                 "missing_required": ["migration-verification"]}
        refusals = self.publish(gates_report=gates, facts=self.COMPLETE, attestation=True)
        self.assertTrue(any("migration-verification" in r for r in refusals))

    def test_no_gates_at_all_is_refused(self):
        refusals = self.publish(
            gates_report={"gates": {}, "missing_required": []},
            facts=self.COMPLETE, attestation=True,
        )
        self.assertTrue(any("no gate" in r for r in refusals))

    def test_synthetic_facts_are_refused(self):
        refusals = self.publish(
            gates_report=self.GREEN, facts={"_synthetic": True, "_blocked_stages": []},
            attestation=True,
        )
        self.assertTrue(any("synthetic" in r for r in refusals))

    def test_blocked_stages_are_refused_and_counted(self):
        facts = {"_synthetic": False, "_blocked_stages": ["image:api", "image:web"]}
        refusals = self.publish(gates_report=self.GREEN, facts=facts, attestation=True)
        joined = " ".join(refusals)
        self.assertIn("2 build stage", joined)
        self.assertIn("image:api", joined)
        self.assertIn("image:web", joined)

    def test_a_missing_attestation_is_refused(self):
        refusals = self.publish(
            gates_report=self.GREEN, facts=self.COMPLETE, attestation=False
        )
        self.assertTrue(any("attestation" in r for r in refusals))

    def test_every_reason_is_reported_not_just_the_first(self):
        """Seven independent problems in one call. A function that returned only the first
        would report 1, and the operator would fix one problem per run."""
        gates = {
            "gates": {"tests": {"state": "red", "reason": "boom"}},
            "missing_required": ["manifest"],
        }
        facts = {"_synthetic": True, "_blocked_stages": ["image:api"]}
        refusals = self.publish(
            tag="v9.9.9", version="0.1.0", gates_report=gates, facts=facts, attestation=False
        )
        self.assertGreaterEqual(len(refusals), 6)


class ArchiveDigests(unittest.TestCase):
    """The two chart digests answer different questions, and only one is reproducible."""

    def _pack(self, path, entries):
        with tarfile.open(path, "w:gz") as archive:
            for name, data, mtime in entries:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mtime = mtime
                info.mode = 0o644
                archive.addfile(info, io.BytesIO(data))

    def test_content_digest_ignores_mtimes(self):
        with tempfile.TemporaryDirectory() as work:
            one, two = os.path.join(work, "1.tgz"), os.path.join(work, "2.tgz")
            self._pack(one, [("x/a", b"alpha", 1000)])
            time.sleep(1.1)
            self._pack(two, [("x/a", b"alpha", 9000)])
            self.assertEqual(
                release_pipeline.canonical_archive_digest(one),
                release_pipeline.canonical_archive_digest(two),
            )

    def test_content_digest_notices_a_content_change(self):
        with tempfile.TemporaryDirectory() as work:
            one, two = os.path.join(work, "1.tgz"), os.path.join(work, "2.tgz")
            self._pack(one, [("x/a", b"alpha", 1000)])
            self._pack(two, [("x/a", b"ALPHA", 1000)])
            self.assertNotEqual(
                release_pipeline.canonical_archive_digest(one),
                release_pipeline.canonical_archive_digest(two),
            )

    def test_content_digest_notices_a_rename(self):
        with tempfile.TemporaryDirectory() as work:
            one, two = os.path.join(work, "1.tgz"), os.path.join(work, "2.tgz")
            self._pack(one, [("x/a", b"alpha", 1000)])
            self._pack(two, [("x/b", b"alpha", 1000)])
            self.assertNotEqual(
                release_pipeline.canonical_archive_digest(one),
                release_pipeline.canonical_archive_digest(two),
            )

    def test_content_digest_notices_a_mode_change(self):
        with tempfile.TemporaryDirectory() as work:
            one, two = os.path.join(work, "1.tgz"), os.path.join(work, "2.tgz")
            self._pack(one, [("x/a", b"alpha", 1000)])
            with tarfile.open(two, "w:gz") as archive:
                info = tarfile.TarInfo("x/a")
                info.size = 5
                info.mtime = 1000
                info.mode = 0o755
                archive.addfile(info, io.BytesIO(b"alpha"))
            self.assertNotEqual(
                release_pipeline.canonical_archive_digest(one),
                release_pipeline.canonical_archive_digest(two),
            )

    def test_byte_digest_does_differ_between_packs(self):
        """The property that makes the second digest necessary, asserted so a reader does
        not have to take it on trust: the two are genuinely different facts."""
        with tempfile.TemporaryDirectory() as work:
            one, two = os.path.join(work, "1.tgz"), os.path.join(work, "2.tgz")
            self._pack(one, [("x/a", b"alpha", 1000)])
            time.sleep(1.1)
            self._pack(two, [("x/a", b"alpha", 9000)])
            self.assertNotEqual(release_pipeline._sha256_file(one), release_pipeline._sha256_file(two))


class CredentialScanning(unittest.TestCase):
    """The compose credential rules, on the forms this repository actually uses."""

    def scan(self, text):
        return release_pipeline._literal_credentials(text)

    def test_a_literal_password_is_found(self):
        self.assertTrue(self.scan("      ADMIN_PASSWORD: hunter2"))

    def test_a_bare_value_under_a_credential_key_is_found(self):
        self.assertTrue(self.scan("      SECRET_KEY: abc123"))

    def test_a_plain_reference_is_clean(self):
        self.assertEqual(self.scan("      SECRET_KEY: ${S3_KEY}"), [])

    def test_compose_required_variable_form_is_clean(self):
        """`${VAR:?message}` is how compose says "must be supplied", and the message is the
        operator's own sentence — not a hard-coded value."""
        self.assertEqual(self.scan("      OMNION_CSRF_SECRET: ${OMNION_CSRF_SECRET:?set it}"), [])

    def test_compose_default_value_form_is_clean(self):
        self.assertEqual(self.scan("      PORT: ${PORT:-3000}"), [])

    def test_compose_alternate_value_form_is_clean(self):
        self.assertEqual(self.scan("      FLAG: ${FLAG:+on}"), [])

    def test_an_empty_value_is_clean(self):
        self.assertEqual(self.scan("      API_KEY:"), [])

    def test_two_references_concatenated_are_clean(self):
        self.assertEqual(self.scan("      URL: ${HOST}${PATH}"), [])

    def test_a_literal_beside_a_reference_is_found(self):
        self.assertTrue(self.scan("      SECRET_KEY: ${S3_KEY}-appended"))

    def test_a_dsn_with_a_literal_password_is_found(self):
        self.assertTrue(self.scan("      DATABASE_URL: postgres://admin:leaked@db:5432/x"))

    def test_a_dsn_with_a_referenced_password_is_clean(self):
        self.assertEqual(
            self.scan("      DATABASE_URL: postgres://omni:${OMNION_DB_PASSWORD}@db:5432/x"), []
        )

    def test_a_pinned_username_is_not_a_secret(self):
        """A username in the clear is the shape this repository uses and is not a leak —
        a check that flags it gets switched off by whoever it annoys."""
        self.assertEqual(self.scan("      DATABASE_URL: postgres://omni:${P}@db/x"), [])

    def test_a_plain_endpoint_is_not_a_credential(self):
        self.assertEqual(self.scan("      REDIS_URL: redis://redis:6379"), [])

    def test_the_repository_compose_stacks_are_clean(self):
        for name in ("docker-compose.prod.yml", "docker-compose.enterprise.yml"):
            path = os.path.join(ROOT, "infra", "compose", name)
            self.assertEqual(release_pipeline._literal_credentials(path), [], name)

    def test_a_rendered_document_is_scanned_differently(self):
        """A rendered compose document is flow style, so a line scanner reads the whole
        environment block as one pair and reports every setting as a credential."""
        render = "env: {PORT=3000, condition=service_healthy, restart=unless-stopped}"
        self.assertEqual(
            release_pipeline._literal_credentials("x", rendered=render, env={}), []
        )

    def test_a_planted_leak_in_a_render_is_found(self):
        render = "env: {ADMIN_PASSWORD=hunter2, PORT=3000}"
        found = release_pipeline._literal_credentials("x", rendered=render, env={})
        self.assertTrue(any("ADMIN_PASSWORD" in f for f in found))

    def test_a_path_and_its_text_agree(self):
        """The helper takes either. When the two call sites disagreed, the path form
        reported a file containing a password as clean."""
        with tempfile.TemporaryDirectory() as work:
            path = os.path.join(work, "stack.yml")
            with open(path, "w", encoding="utf-8") as handle:
                handle.write("services:\n  api:\n    environment:\n      TOKEN: hunter2\n")
            self.assertEqual(
                release_pipeline._literal_credentials(path),
                release_pipeline._literal_credentials(open(path).read()),
            )


class ShellOperators(unittest.TestCase):
    def test_required_form_reduces_to_the_reference(self):
        self.assertEqual(
            release_pipeline._strip_shell_operators("${Y:?set Y in .env}"), "${Y}"
        )

    def test_default_form_reduces_to_the_reference(self):
        self.assertEqual(release_pipeline._strip_shell_operators("${Y:-fallback}"), "${Y}")

    def test_a_plain_reference_is_unchanged(self):
        self.assertEqual(release_pipeline._strip_shell_operators("${Y}"), "${Y}")

    def test_a_bare_value_is_not_a_reference(self):
        self.assertFalse(release_pipeline._is_pure_reference("hunter2"))

    def test_a_reference_with_a_suffix_is_not_pure(self):
        self.assertFalse(release_pipeline._is_pure_reference("${Y}-appended"))


class FactsDocument(unittest.TestCase):
    def test_only_verified_stages_contribute_an_entry(self):
        results = [
            {"id": "chart:omnion", "kind": "chart", "name": "omnion-0.1.0.tgz",
             "state": "verified", "digest": "sha256:" + "a" * 64, "size_bytes": 10,
             "content_digest": "sha256:" + "b" * 64},
            {"id": "image:api", "kind": "image", "name": "ghcr.io/raksix/omnion/api",
             "state": "blocked", "reason": "needs a builder", "digest": None,
             "size_bytes": None},
        ]
        facts = release_pipeline._facts_document("0.1.0", ROOT, results)
        self.assertIn("omnion-0.1.0.tgz", facts["charts"])
        self.assertNotIn("image:api", facts["images"])
        self.assertEqual(facts["images"], {})
        self.assertEqual(facts["_blocked_stages"], ["image:api"])

    def test_the_document_declares_itself_non_synthetic(self):
        facts = release_pipeline._facts_document("0.1.0", ROOT, [])
        self.assertIs(facts["_synthetic"], False)
        self.assertEqual(facts["_source"], release_pipeline.FACTS_SOURCE)

    def test_the_chart_entry_carries_both_digests(self):
        results = [
            {"id": "chart:omnion", "kind": "chart", "name": "omnion-0.1.0.tgz",
             "state": "verified", "digest": "sha256:" + "a" * 64, "size_bytes": 10,
             "content_digest": "sha256:" + "b" * 64},
        ]
        entry = release_pipeline._facts_document("0.1.0", ROOT, results)["charts"][
            "omnion-0.1.0.tgz"
        ]
        self.assertIn("digest", entry)
        self.assertIn("content_digest", entry)
        self.assertNotEqual(entry["digest"], entry["content_digest"])


class RequiredGates(unittest.TestCase):
    def test_the_gate_list_cannot_be_silently_shortened(self):
        """A gate dropped from this list is a gate nothing runs, and the request names
        tests and the migration verification as the two that must be green."""
        for name in ("tests", "migration-verification", "manifest"):
            self.assertIn(name, release_pipeline.REQUIRED_GATES)

    def test_a_gate_with_no_command_is_unrunnable_not_green(self):
        report = release_pipeline.run_gates(ROOT, ["migration-verification"])
        self.assertEqual(report["gates"]["migration-verification"]["state"], "unrunnable")

    def test_omitting_a_required_gate_is_reported_as_missing(self):
        report = release_pipeline.run_gates(ROOT, ["manifest"])
        self.assertIn("tests", report["missing_required"])
        self.assertFalse(report["green"])


class CommandSurface(unittest.TestCase):
    """The four subcommands, as the CI and the tag pipeline invoke them."""

    def _run(self, *args):
        return subprocess.run(
            [sys.executable, os.path.join(ROOT, "release", "lib", "pipeline.py"), *args],
            capture_output=True, text=True, cwd=ROOT, timeout=300,
        )

    def test_plan_json_parses(self):
        result = self._run("plan", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout))

    def test_plan_human_output_names_the_stages(self):
        result = self._run("plan")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("stages", result.stdout)

    def test_publish_check_refuses_this_repository(self):
        with tempfile.TemporaryDirectory() as work:
            facts_path = os.path.join(work, "facts.json")
            with open(facts_path, "w", encoding="utf-8") as handle:
                json.dump({"_synthetic": False, "_blocked_stages": ["image:api"]}, handle)
            result = self._run(
                "publish-check", "--tag", "v0.1.0", "--facts", facts_path, "--attestation"
            )
            self.assertNotEqual(result.returncode, 0)
            report = json.loads(result.stdout)
            self.assertIs(report["publishable"], False)
            self.assertTrue(report["refusals"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
