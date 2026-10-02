"""Unit tests for the release manifest builder (REQ-128, slice 3).

The shell gate proves the builder survives the happy path and refuses nineteen mutations.
These prove the parts a shell gate cannot reach cheaply: the version comparator's edge cases
(which decide whether an upgrade is allowed) and the discovery functions, which read real
repository files and therefore have a different failure mode — a rename, a moved file, a
changed template syntax. A function that returns an empty set because it could not parse
its input passes "did the manifest get built" and fails nothing.

Run: `python3 -m unittest discover -s release/tests -t . -v`
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "lib"))

import manifest as release  # noqa: E402

REPO = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))


class VersionComparison(unittest.TestCase):
    """The comparator decides whether an operator may install a release."""

    def test_components_compare_numerically_not_lexically(self):
        self.assertGreater(release.compare_versions("0.10.0", "0.9.0"), 0)
        self.assertLess(release.compare_versions("0.9.0", "0.10.0"), 0)
        # The case that a string comparison gets wrong AND that would be accepted today:
        # 0.9.0 -> 0.10.0 is an upgrade, and a lexical compare calls it a downgrade.
        self.assertTrue(release.satisfies_core_minimum("0.10.0", "0.9.0"))

    def test_equal_versions_are_equal(self):
        self.assertEqual(release.compare_versions("1.2.3", "1.2.3"), 0)

    def test_a_prerelease_sorts_below_its_release(self):
        self.assertLess(release.compare_versions("1.0.0-rc.1", "1.0.0"), 0)
        self.assertLess(release.compare_versions("1.0.0-rc.1", "1.0.0-rc.2"), 0)
        self.assertGreater(release.compare_versions("1.0.0", "1.0.0-rc.9"), 0)

    def test_a_prerelease_does_not_satisfy_a_release_minimum(self):
        # Installing an rc over a stable release is how a database migration meets code that
        # was never released, so the refusal has to be in the MINIMUM check, not only in sort order.
        self.assertFalse(release.satisfies_core_minimum("1.0.0-rc.1", "1.0.0"))
        self.assertTrue(release.satisfies_core_minimum("1.0.0", "1.0.0"))

    def test_a_non_version_is_refused_rather_than_defaulted(self):
        for value in ("latest", "", "1.2", "v1.2.3", None, "1.2.3.4"):
            with self.assertRaises(release.ManifestError):
                release.parse_version(value)  # type: ignore[arg-type]

    def test_leading_and_trailing_whitespace_is_tolerated(self):
        self.assertEqual(release.compare_versions(" 1.2.3 ", "1.2.3"), 0)


class CredentialShapes(unittest.TestCase):
    def test_url_userinfo_is_a_credential(self):
        for value in (
            "oci://user:pass@ghcr.io/raksix/omnion/api",
            "https://token:x-oauth-basic@github.com/raksix/omnion",
        ):
            self.assertTrue(release.carries_credential(value), value)

    def test_env_assignment_is_a_credential(self):
        for value in ("POSTGRES_PASSWORD=hunter2", "API_KEY=abc123", "REDIS_TOKEN=x"):
            self.assertTrue(release.carries_credential(value), value)

    def test_ghcr_tokens_are_credentials(self):
        self.assertTrue(release.carries_credential("ghp_" + "a" * 36))

    def test_ordinary_references_are_not_credentials(self):
        for value in (
            "ghcr.io/raksix/omnion/api",
            "oci://ghcr.io/raksix/omnion/api",
            "https://github.com/raksix/omnion/releases/download/v0.1.0/omnion-linux-amd64.tar.gz",
            "docker-compose.prod.yml",
            "omnion-windows-amd64",
        ):
            self.assertFalse(release.carries_credential(value), value)

    def test_a_url_without_userinfo_is_not_a_credential(self):
        # `@` in a path is common in package scopes; only userinfo BEFORE the host counts.
        self.assertFalse(release.carries_credential("https://example.com/@scope/pkg"))


class RepositoryDiscovery(unittest.TestCase):
    """These read real files, so their failure mode is parsing nothing and returning empty."""

    def test_every_migration_on_disk_is_listed(self):
        migrations = release.discover_migrations(REPO)
        self.assertGreater(len(migrations), 50)
        # Sorted NUMERICALLY: a five-digit migration number must not sort before four-digit ones.
        numbers = [int(name.split("_", 1)[0]) for name in migrations]
        self.assertEqual(numbers, sorted(numbers))

    def test_a_real_migration_is_present(self):
        self.assertIn("0188_system_health.sql", release.discover_migrations(REPO))

    def test_migration_names_match_the_naming_convention(self):
        for name in release.discover_migrations(REPO):
            self.assertRegex(name, r"^\d{4}_[a-z0-9_]+\.sql$", name)

    def test_each_dockerfile_declares_the_runtime_stage(self):
        targets = release.dockerfile_targets(REPO)
        self.assertEqual(targets["api"], ["api"])
        self.assertEqual(targets["cli"], ["cli"])
        # The panel and the renderer share one file so they cannot drift on the install layer.
        self.assertEqual(sorted(targets["admin"]), ["admin", "web"])

    def test_the_web_image_is_published(self):
        # The regression this whole cross-check exists for: `web` has no Dockerfile of its own,
        # so a one-Dockerfile-one-image rule ships a manifest with no public-site image.
        names = {image["name"].rsplit("/", 1)[-1] for image in release.discover_images(REPO)}
        self.assertIn("web", names)
        self.assertEqual(names, {"admin", "api", "cli", "web"})

    def test_the_chart_and_compose_deploy_only_published_images(self):
        deployed = release.deployed_image_names(REPO)
        published = {i["name"].rsplit("/", 1)[-1] for i in release.discover_images(REPO)}
        self.assertEqual(deployed - published, set())
        self.assertEqual(release._chart_components(REPO), {"api", "admin", "web"})

    def test_the_chart_repository_lives_under_the_release_registry(self):
        self.assertIsNone(release.repository_mismatch("ghcr.io/raksix/omnion", REPO))

    def test_a_wrong_registry_is_named(self):
        problem = release.repository_mismatch("ghcr.io/somebody-else", REPO)
        self.assertIsNotNone(problem)
        self.assertIn("ghcr.io/raksix/omnion", problem)
        self.assertIn("ghcr.io/somebody-else", problem)

    def test_every_non_service_marker_states_a_reason(self):
        for component in release.non_service_images(REPO):
            reason = release.undeclared_image_reasons(REPO).get(component, "")
            self.assertGreater(len(reason), 30, f"{component} marker has no reason")


class ManifestShape(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.facts = release.synthetic_facts(REPO)
        cls.manifest = release.build_manifest(
            release.workspace_version(REPO),
            "a" * 40,
            root=REPO,
            facts=cls.facts,
            allow_version_drift=True,
        )

    def test_the_manifest_verifies(self):
        self.assertEqual(release.verify_manifest(self.manifest, core_version="0.1.0"), [])

    def test_every_documented_artifact_kind_is_present(self):
        kinds = {a["kind"] for a in self.manifest["artifacts"]}
        self.assertEqual(kinds, set(release.REQUIRED_ARTIFACT_KINDS))

    def test_the_registry_is_stripped_of_a_trailing_slash(self):
        manifest = release.build_manifest(
            release.workspace_version(REPO), "b" * 40, registry="ghcr.io/raksix/omnion/",
            root=REPO, facts=self.facts, allow_version_drift=True,
        )
        self.assertEqual(manifest["registry"], "ghcr.io/raksix/omnion")
        for artifact in manifest["artifacts"]:
            if artifact["kind"] == "image":
                self.assertNotIn("//", artifact["name"])

    def test_destructiveness_reflects_the_tree_rather_than_the_release_date(self):
        # The tri-state is `True` / `False` / `None`, and `None` means "the gate that would
        # make the absence of a marker mean something has not landed". This repository is past
        # that now — `0207_migration_safety.sql` is in the tree — so the flag answers the
        # question it exists to answer: 53 of 58 migrations ship no down script, therefore
        # `True`.
        #
        # It used to assert `None`, which was right when REQ-129 was unbuilt and became a
        # snapshot of the repository's age rather than a test of the rule. A release manifest
        # that said "not destructive" about a tree with 53 unreversible migrations is the
        # false promise this request was written against, so the assertion is now on the
        # VALUE, and the pairing with an absent policy is covered in the upgrade suite where
        # both branches can be built in a temp tree.
        self.assertIs(
            self.manifest["migrations_destructive"],
            True,
            "a tree with reversal-less migrations must publish migrations_destructive: true",
        )
        self.assertTrue(release.destructive_migrations(REPO))

    def test_a_version_disagreement_is_refused(self):
        with self.assertRaises(release.ManifestError) as ctx:
            release.build_manifest(
                release.workspace_version(REPO), "c" * 40,
                root=REPO, facts=self.facts, allow_version_drift=False,
            )
        self.assertIn("version disagreement", str(ctx.exception))

    def test_a_non_commit_source_is_refused(self):
        with self.assertRaises(release.ManifestError):
            release.build_manifest(
                "0.1.0", "not-a-commit",
                root=REPO, facts=self.facts, allow_version_drift=True,
            )

    def test_a_missing_digest_is_refused(self):
        facts = json.loads(json.dumps(self.facts))
        del facts["images"]["ghcr.io/raksix/omnion/api"]
        with self.assertRaises(release.ManifestError) as ctx:
            release.build_manifest(
                "0.1.0", "d" * 40, root=REPO, facts=facts, allow_version_drift=True,
            )
        self.assertIn("no digest supplied", str(ctx.exception))

    def test_a_non_sha256_digest_is_refused(self):
        facts = json.loads(json.dumps(self.facts))
        facts["images"]["ghcr.io/raksix/omnion/api"]["digest"] = "latest"
        with self.assertRaises(release.ManifestError) as ctx:
            release.build_manifest(
                "0.1.0", "e" * 40, root=REPO, facts=facts, allow_version_drift=True,
            )
        self.assertIn("not a sha256 digest", str(ctx.exception))

    def test_a_bare_digest_string_is_accepted(self):
        # A CI job writing `{"images": {"name": "sha256:…"}}` is a reasonable thing to emit.
        facts = json.loads(json.dumps(self.facts))
        for record in facts["images"].values():
            record = record  # noqa: B018
        facts["images"] = {name: rec["digest"] for name, rec in facts["images"].items()}
        manifest = release.build_manifest(
            "0.1.0", "f" * 40, root=REPO, facts=facts, allow_version_drift=True,
        )
        self.assertEqual(
            next(a["digest"] for a in manifest["artifacts"] if a["name"].endswith("/api")),
            facts["images"]["ghcr.io/raksix/omnion/api"],
        )

    def test_synthetic_facts_are_deterministic(self):
        self.assertEqual(release.synthetic_facts(REPO), release.synthetic_facts(REPO))
        self.assertTrue(release.synthetic_facts(REPO)["_synthetic"])


class VerificationRefusals(unittest.TestCase):
    """`verify_manifest` is what an operator's install runs, so it must fail closed."""

    @classmethod
    def setUpClass(cls):
        cls.manifest = release.build_manifest(
            release.workspace_version(REPO), "0" * 40, root=REPO,
            facts=release.synthetic_facts(REPO), allow_version_drift=True,
        )

    def verify(self, mutate):
        import copy

        candidate = copy.deepcopy(self.manifest)
        mutate(candidate)
        return release.verify_manifest(candidate, core_version="0.1.0")

    def test_a_clean_manifest_reports_nothing(self):
        self.assertEqual(release.verify_manifest(self.manifest, core_version="0.1.0"), [])

    def test_a_below_minimum_core_is_refused(self):
        problems = release.verify_manifest(self.manifest, core_version="0.0.1")
        self.assertTrue(any("below this release" in p for p in problems), problems)

    def test_an_empty_migration_list_is_refused(self):
        problems = self.verify(lambda m: m.__setitem__("migrations", []))
        self.assertTrue(any("ships no migrations" in p for p in problems), problems)

    def test_a_downgraded_schema_version_is_refused(self):
        problems = self.verify(lambda m: m.__setitem__("manifest_version", "0"))
        self.assertTrue(any("manifest_version" in p for p in problems), problems)

    def test_a_missing_cli_platform_is_refused(self):
        def drop(m):
            m["artifacts"] = [
                a for a in m["artifacts"]
                if not (a["kind"] == "cli" and a["platforms"] == ["windows-amd64"])
            ]

        problems = self.verify(drop)
        self.assertTrue(any("windows-amd64" in p for p in problems), problems)

    def test_a_credential_in_a_download_url_is_refused(self):
        problems = self.verify(lambda m: m["artifacts"][0].__setitem__(
            "download_url", "oci://ghcr.io/raksix/omnion/api?PASSWORD=hunter2"))
        self.assertTrue(any("credential" in p for p in problems), problems)

    def test_a_tag_as_a_digest_is_refused(self):
        problems = self.verify(lambda m: m["artifacts"][0].__setitem__("digest", "sha256:nope"))
        self.assertTrue(any("digest" in p for p in problems), problems)

    def test_a_registry_swap_on_a_chart_artifact_is_still_caught(self):
        # The chart is `artifacts[0]` because the list is sorted by kind. Renaming IT is a
        # different mutation from renaming an image, and the credential rule covers it.
        problems = self.verify(lambda m: m["artifacts"][0].__setitem__(
            "name", "oci://user:token@ghcr.io/raksix/omnion"))
        self.assertTrue(any("credential" in p for p in problems), problems)

    def test_an_image_outside_the_registry_is_refused(self):
        # Mutate an IMAGE, not `artifacts[0]`: the list is sorted by kind, so index 0 is the
        # chart and renaming it to `docker.io/…` is not the mutation anyone imagined. The
        # check is right and the test was aimed at nothing.
        def retarget(m):
            image = next(a for a in m["artifacts"] if a["kind"] == "image")
            image["name"] = "docker.io/attacker/omnion-api"

        problems = self.verify(retarget)
        self.assertTrue(any("registry" in p for p in problems), problems)

    def test_a_non_object_manifest_is_refused(self):
        self.assertEqual(release.verify_manifest("not a manifest"), ["manifest is not an object"])

    def test_an_empty_artifact_list_is_refused(self):
        problems = release.verify_manifest({**self.manifest, "artifacts": []})
        self.assertIn("manifest lists no artifacts", problems)


class SchemaAgreement(unittest.TestCase):
    def test_the_schema_documents_every_required_manifest_key(self):
        schema = release.schema()
        manifest = release.build_manifest(
            release.workspace_version(REPO), "1" * 40, root=REPO,
            facts=release.synthetic_facts(REPO), allow_version_drift=True,
        )
        for key in schema["required"]:
            self.assertIn(key, manifest)

    def test_the_schema_refuses_an_unknown_artifact_key(self):
        items = release.schema()["properties"]["artifacts"]["items"]
        self.assertFalse(items["additionalProperties"])

    def test_every_artifact_key_is_declared_by_the_schema(self):
        items = release.schema()["properties"]["artifacts"]["items"]
        declared = set(items["properties"])
        manifest = release.build_manifest(
            release.workspace_version(REPO), "2" * 40, root=REPO,
            facts=release.synthetic_facts(REPO), allow_version_drift=True,
        )
        for artifact in manifest["artifacts"]:
            self.assertEqual(set(artifact) - declared, set(), artifact["name"])


class RepositoryIsSelfContained(unittest.TestCase):
    def test_discovery_needs_no_network_or_registry(self):
        # The point of the module: everything it reads is a file in this repository.
        self.assertTrue(os.path.isdir(os.path.join(REPO, "infra", "docker")))
        self.assertTrue(os.path.isdir(os.path.join(REPO, "database", "migrations")))

    def test_a_missing_facts_file_is_refused_not_treated_as_empty(self):
        with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as handle:
            empty = handle.name
        try:
            with self.assertRaises(release.ManifestError):
                release.load_supplied(os.path.join(empty, "nope.json"))
        finally:
            os.unlink(empty)


if __name__ == "__main__":
    unittest.main(verbosity=2)