"""Unit tests for the environment bundle generator (REQ-128, slice 3).

The shell gate proves a generated bundle renders. These prove the claims a render cannot,
and they are the half that matters, because the request's acceptance criterion is not "the
files are valid YAML" — it is "generated bundles contain secret REFERENCES only — a test
greps every generated file for the fixture value and for common secret-shaped strings and
finds none."

That is a property of the GENERATOR, and it is only real if the test can make the generator
wrong. So most of what follows is mutation: build a fixture that WOULD ship a credential,
assert it is refused, and assert which refusal. A test that only generates the good case
passes on a generator with no secret check at all.

Run: `python3 -m unittest discover -s release/tests -t . -v`
"""

import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "lib"))

import bundle as gen  # noqa: E402
import manifest as release  # noqa: E402
import pipeline  # noqa: E402

REPO = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

#: The value a secret would have if one were ever inlined. Every fixture below uses THIS
#: string, and the property test greps for it — so "no fixture value in any generated file"
#: is one assertion about one constant rather than a per-case opinion about a shape.
FIXTURE_SECRET = "s3cr3t-fixture-value-9f2b1c"


def files_of(bundle):
    """(name, body) pairs, rebuilt from the bundle's own recorded digests."""
    return [(f["name"], f) for f in bundle["files"]]


def bodies(root, bundle):
    return dict((name, body) for name, body in gen._bundle_files(root, bundle["config"]))


class Generation(unittest.TestCase):
    def test_all_three_kinds_generate_and_carry_no_secret(self):
        for kind in gen.BUNDLE_KINDS:
            with self.subTest(kind=kind):
                bundle = gen.generate_bundle(
                    {"kind": kind, "domain": "acme.example.com", "name": "acme"}, REPO
                )
                self.assertEqual(bundle["kind"], kind)
                self.assertTrue(bundle["files"])
                self.assertEqual(bundle["secret_scan"]["state"], "clean")

    def test_the_requested_kinds_are_the_ones_the_schema_lists(self):
        # A kind added without a verification command is a kind nobody ever installs.
        for kind in gen.BUNDLE_KINDS:
            self.assertIn(kind, gen.VERIFY_COMMANDS)
        self.assertEqual(set(gen.BUNDLE_KINDS), set(gen.VERIFY_COMMANDS))

    def test_the_generated_files_do_not_contain_the_fixture_secret(self):
        # The acceptance criterion, as an assertion. The generator never receives this
        # string, so this is the property "nothing was inlined", checked on the OUTPUT.
        for kind in gen.BUNDLE_KINDS:
            with self.subTest(kind=kind):
                bundle = gen.generate_bundle(
                    {"kind": kind, "domain": "acme.example.com", "name": "acme"}, REPO
                )
                for name, body in bodies(REPO, bundle).items():
                    self.assertNotIn(FIXTURE_SECRET, body, f"{kind}/{name} leaked the fixture")

    def test_every_generated_file_records_a_real_digest(self):
        # A file list whose sha256 does not match the bytes is a checksum screen that
        # verifies nothing; the bundle's own checksum is the value the DB row stores.
        bundle = gen.generate_bundle(
            {"kind": "compose-small", "domain": "acme.example.com", "name": "acme"}, REPO
        )
        import hashlib

        for name, body in bodies(REPO, bundle).items():
            described = next(f for f in bundle["files"] if f["name"] == name)
            self.assertEqual(described["sha256"], hashlib.sha256(body.encode()).hexdigest())
            self.assertEqual(described["size_bytes"], len(body.encode()))
        self.assertEqual(len(bundle["checksum"]), 64)

    def test_generation_is_deterministic_for_the_same_request(self):
        # A bundle whose checksum changes per generation cannot be compared with the
        # previous one for the same name+version, which is what the table's "keeps the
        # previous for comparison" depends on.
        request = {"kind": "helm", "domain": "acme.example.com", "name": "acme"}
        first = gen.generate_bundle(request, REPO)
        second = gen.generate_bundle(request, REPO)
        self.assertEqual(first["checksum"], second["checksum"])

    def test_a_different_preset_changes_the_files(self):
        small = gen.generate_bundle(
            {"kind": "helm", "domain": "a.example.com", "name": "a", "size": "small"}, REPO
        )
        large = gen.generate_bundle(
            {"kind": "helm", "domain": "a.example.com", "name": "a", "size": "large"}, REPO
        )
        self.assertNotEqual(small["checksum"], large["checksum"])


class Refusals(unittest.TestCase):
    """The mutations. Each one builds a request the generator must reject."""

    def test_a_secret_in_the_name_is_refused(self):
        # `name` reaches the values filename and the header comment. A name carrying a
        # password would be written into a file the operator downloads and commits.
        with self.assertRaises(gen.BundleError):
            gen.generate_bundle(
                {"kind": "helm", "domain": "a.example.com", "name": FIXTURE_SECRET}, REPO
            )

    def test_a_secret_in_the_domain_is_refused(self):
        # The domain is the one field a caller might paste a password into by mistake, and
        # it lands in a URL, a TLS request and a Kubernetes host rule.
        for domain in (FIXTURE_SECRET, "acme.example.com/../etc", "not a domain", "-bad.com"):
            with self.subTest(domain=domain), self.assertRaises(gen.BundleError):
                gen.generate_bundle({"kind": "helm", "domain": domain, "name": "a"}, REPO)

    def test_a_template_injection_in_the_registry_is_refused(self):
        # A registry is substituted into the stack file. `${…}` there would inject a second
        # interpolation, and a quote would break the document.
        for registry in ("${OMNION_DB_PASSWORD}", 'ghcr.io/"; echo pwned; #', "not a registry"):
            with self.subTest(registry=registry), self.assertRaises(gen.BundleError):
                gen.generate_bundle(
                    {"kind": "helm", "domain": "a.example.com", "name": "a", "registry": registry},
                    REPO,
                )

    def test_an_unknown_kind_or_tls_mode_is_refused_rather_than_defaulted(self):
        with self.assertRaises(gen.BundleError):
            gen.generate_bundle(
                {"kind": "helm", "domain": "a.example.com", "name": "a", "tls_mode": "magic"}, REPO
            )
        with self.assertRaises(gen.BundleError):
            gen.generate_bundle({"kind": "kubernetes", "domain": "a.example.com", "name": "a"}, REPO)

    def test_a_missing_stack_file_is_refused(self):
        # The bundle describes files from THIS checkout. Generating for a stack that is not
        # here produces a download that cannot boot.
        #
        # The root is a copy of the repository with ONE file removed, not an empty directory:
        # `validate_request` reads `Cargo.toml` for the version before it checks the stack, so
        # an empty root fails earlier with `FileNotFoundError` — an unhandled traceback rather
        # than a refusal, which is the defect this test was written to look for in the first
        # place.
        empty = tempfile.mkdtemp()
        try:
            shutil.copy(os.path.join(REPO, "Cargo.toml"), os.path.join(empty, "Cargo.toml"))
            with self.assertRaises(gen.BundleError):
                gen.generate_bundle(
                    {"kind": "compose-small", "domain": "a.example.com", "name": "a"}, empty
                )
        finally:
            shutil.rmtree(empty, ignore_errors=True)

    def test_a_secret_shaped_registry_is_refused(self):
        # The format pattern for a registry is correct and still accepts a password, because
        # `s3cr3t-fixture-value-9f2b1c` is a legal hostname label. Found by the mutation
        # test, and fixed in the generator rather than by loosening the test.
        for registry in (FIXTURE_SECRET, "ghcr.io/" + FIXTURE_SECRET):
            with self.subTest(registry=registry), self.assertRaises(gen.BundleError):
                gen.generate_bundle(
                    {"kind": "compose-small", "domain": "a.example.com", "name": "a",
                     "registry": registry},
                    REPO,
                )

    def test_an_ordinary_name_is_not_mistaken_for_a_credential(self):
        # The other direction: a check that refuses "acme-production" is a check that gets
        # switched off. The exception has to be narrow.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "a.example.com", "name": "acme production"}, REPO
        )
        self.assertEqual(bundle["name"], "acme production")

    def test_the_secret_scan_fires_on_an_injected_literal(self):
        """The scan, exercised on a template that HAS a credential in it.

        Without this the scan could be a no-op and every other test would still pass: no
        generator is ever handed a credential, so nothing else in this file ever triggers
        the refusal path.
        """
        poisoned = [("evil.env", f"OMNION_DB_PASSWORD={FIXTURE_SECRET}\n")]
        findings = gen.secret_findings(REPO, {"kind": "compose-small"}, poisoned)
        self.assertTrue(findings, "a hard-coded password was not reported")
        self.assertTrue(any(FIXTURE_SECRET in f for f in findings))

    def test_the_scan_fires_on_a_literal_inside_a_connection_url(self):
        # The key is `DATABASE_URL`, which names no credential. The password inside it is
        # the credential.
        poisoned = [("evil.env", f"OMNION_REDIS_URL=redis://user:{FIXTURE_SECRET}@cache:6379\n")]
        findings = gen.secret_findings(REPO, {"kind": "compose-small"}, poisoned)
        self.assertTrue(findings)

    def test_the_scan_reports_a_clean_reference_file_as_clean(self):
        clean = [("ok.env", "OMNION_DB_PASSWORD=${OMNION_DB_PASSWORD:?set it in .env}\n")]
        self.assertEqual(gen.secret_findings(REPO, {"kind": "compose-small"}, clean), [])


class KubernetesObjectNames(unittest.TestCase):
    """The scan must not fire on the chart's own reference vocabulary.

    These are the findings the generator produced on ITSELF when the scan was first wired in:
    `existingSecret: omnion-secrets` and `secrets.keys.s3SecretKey: S3_SECRET_KEY` are in the
    chart's committed `values.yaml`. A check that fires on correct code is switched off by
    whoever it annoys — and then the real leak it was written for ships. So the exception is
    asserted here rather than left as a comment.
    """

    def test_a_kubernetes_object_name_is_not_a_credential(self):
        clean = [
            ("values.yaml", "secrets:\n  existingSecret: omnion-secrets\n  keys:\n    s3SecretKey: S3_SECRET_KEY\n")
        ]
        self.assertEqual(gen.secret_findings(REPO, {"kind": "helm"}, clean), [])

    def test_a_tls_secret_name_is_not_a_credential(self):
        clean = [("values.yaml", "tls:\n  - secretName: omnion-tls\n    hosts:\n      - a.example.com\n")]
        self.assertEqual(gen.secret_findings(REPO, {"kind": "helm"}, clean), [])

    def test_the_exception_is_a_named_set_and_does_not_cover_a_password_key(self):
        self.assertIn("existingSecret", gen._REFERENCE_KEYS)
        self.assertIn("secretName", gen._REFERENCE_KEYS)
        # A key that merely LOOKS like an object-name key is not on the list, so the next
        # person cannot widen it into "anything with secret in the name".
        self.assertNotIn("password", gen._REFERENCE_KEYS)
        self.assertNotIn("token", gen._REFERENCE_KEYS)
        self.assertNotIn("secret", gen._REFERENCE_KEYS)
        leaking = [("values.yaml", "password: hunter2\n")]
        self.assertTrue(gen.secret_findings(REPO, {"kind": "helm"}, leaking))


class ChartContract(unittest.TestCase):
    def test_every_key_the_helm_bundle_sets_is_declared_by_the_chart_schema(self):
        # `helm install` with an undeclared key fails with a field-level error, so this is
        # the "installs the chart unmodified" criterion, checked without a cluster.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "acme.example.com", "name": "acme"}, REPO
        )
        self.assertEqual(gen._cross_check_schema(REPO, bundle["config"]), [])

    def test_the_secret_key_names_are_the_charts_own(self):
        # The bundle tells the operator which keys to put in the Secret. If those names drift
        # from the chart, every pod starts and then crash-loops on an unset variable — and the
        # generator is the only place that could catch it.
        with open(os.path.join(REPO, gen.CHART_VALUES), encoding="utf-8") as handle:
            chart = handle.read()
        for key in gen.SECRET_REFERENCE["keys"]:
            with self.subTest(key=key):
                self.assertIn(key, chart, f"the chart does not declare secret key {key}")
        self.assertIn(gen.SECRET_REFERENCE["name"], chart)

    def test_the_ingress_hosts_carry_a_service_each(self):
        # `paths[]` without `service` renders an Ingress rule pointing at nothing: present,
        # matched, and never routed. This is the defect the first version shipped.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "acme.example.com", "name": "acme"}, REPO
        )
        body = bodies(REPO, bundle)[f"{bundle['name']}-values.yaml"]
        self.assertIn("service: web", body)
        self.assertIn("service: admin", body)

    def test_the_panel_is_not_served_on_the_public_host(self):
        # The renderer is anonymous and the panel is not. Putting both on one host puts the
        # panel's cookies on a public name.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "acme.example.com", "name": "acme"}, REPO
        )
        body = bodies(REPO, bundle)[f"{bundle['name']}-values.yaml"]
        self.assertIn("host: admin.acme.example.com", body)
        self.assertNotIn("host: www.acme.example.com", body)

    def test_each_component_gets_its_own_resources(self):
        # The bug: one shared requests/limits map assigned to api, admin and web, so every
        # pod asked for all three components' memory. The sizes are per component and the
        # memory limits must differ between api and web in the `small` preset.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "acme.example.com", "name": "acme", "size": "small"}, REPO
        )
        body = bodies(REPO, bundle)[f"{bundle['name']}-values.yaml"]
        api_block = body.split("\napi:")[1].split("\nadmin:")[0]
        web_block = body.split("\nweb:")[1]
        self.assertIn("memory: 1Gi", api_block)
        self.assertIn("memory: 768Mi", web_block)

    def test_compose_sizes_are_translated_to_kubernetes_quantities(self):
        self.assertEqual(gen._to_quantity("1g"), "1Gi")
        self.assertEqual(gen._to_quantity("768m"), "768Mi")
        self.assertEqual(gen._to_quantity("2.0"), "2.0")


class TlsModes(unittest.TestCase):
    def _values(self, mode):
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "acme.example.com", "name": "a", "tls_mode": mode}, REPO
        )
        return bodies(REPO, bundle)["a-values.yaml"]

    def test_managed_cert_names_a_secret(self):
        self.assertIn("secretName: omnion-tls", self._values("managed-cert"))

    def test_external_secret_uses_annotations_and_no_secret_name(self):
        # The chart documents these two as mutually exclusive: a host with both a
        # `secretName` and an issuer annotation is served by whichever writes first.
        body = self._values("external-secret")
        self.assertIn("cert-manager.io/cluster-issuer", body)
        self.assertNotIn("secretName: omnion-tls", body)

    def test_terminate_upstream_sets_no_tls_at_all(self):
        body = self._values("terminate-upstream")
        self.assertNotIn("secretName:", body)
        self.assertNotIn("cert-manager.io", body)


class StackVariables(unittest.TestCase):
    """The undocumented-variable check, which found two real repository defects."""

    def test_a_required_reference_is_distinguished_from_an_optional_one(self):
        found = gen.stack_variables("a: ${OMNION_DB_PASSWORD:?set it}\nb: ${OMNION_API_PORT:-8080}\n")
        self.assertEqual(found["OMNION_DB_PASSWORD"], "required")
        self.assertEqual(found["OMNION_API_PORT"], "optional")

    def test_the_repository_stacks_document_every_required_variable_they_read(self):
        # The defect this check was written for: `docker-compose.enterprise.yml` read three
        # REQUIRED variables that `.env.example` never declared, so the stack refused to
        # start and the operator had no file telling them which values to supply.
        with open(os.path.join(REPO, gen.ENV_EXAMPLE), encoding="utf-8") as handle:
            env_text = handle.read()
        documented = gen.documented_variables(env_text)
        for kind, rel in gen.COMPOSE_STACKS.items():
            with self.subTest(kind=kind):
                with open(os.path.join(REPO, rel), encoding="utf-8") as handle:
                    stack_text = handle.read()
                for name, required in gen.stack_variables(stack_text).items():
                    if required == "required":
                        self.assertIn(
                            name, documented, f"{rel} requires {name}, which .env.example omits"
                        )

    def test_no_compose_stack_asks_for_a_variable_named_var_in_a_comment(self):
        # Compose interpolates `$` expressions inside comments, so an explanatory `${VAR:?…}`
        # in a comment header makes the stack demand a variable named `VAR` that appears
        # nowhere else. It shipped in docker-compose.prod.yml until this check found it.
        for kind, rel in gen.COMPOSE_STACKS.items():
            with self.subTest(kind=kind):
                with open(os.path.join(REPO, rel), encoding="utf-8") as handle:
                    stack_text = handle.read()
                self.assertNotIn("VAR", gen.stack_variables(stack_text))

    def test_the_verification_environment_uses_typed_values(self):
        # A `0VAR-PLACEHOLDER0` in a port field is refused by compose as `invalid hostPort`,
        # which would make the verification measure the placeholder instead of the stack.
        bundle = gen.generate_bundle(
            {"kind": "compose-small", "domain": "a.example.com", "name": "a"}, REPO
        )
        env = gen._verification_environment(bundle, REPO)
        self.assertEqual(env["OMNION_API_PORT"], "8080")
        for name, value in env.items():
            if name.endswith("_PORT"):
                self.assertTrue(value.isdigit(), f"{name}={value} is not a port")


class Verification(unittest.TestCase):
    """`verify_bundle` states `unverified` rather than `verified` where it cannot run."""

    def test_an_unknown_kind_is_unverified_not_verified(self):
        verdict = gen.verify_bundle({"kind": "unknown", "config": {}, "files": []}, REPO)
        self.assertEqual(verdict["state"], "unverified")
        self.assertIn("unknown kind", verdict["reason"])

    def test_a_missing_tool_is_unverified_and_names_itself(self):
        # `verify_bundle` imports `shutil` INSIDE the function, so the patch target is the
        # module object itself. The first version of this test also tried `gen.shutil`, which
        # does not exist — and an `AttributeError` in a test that is ABOUT a missing tool is
        # the sort of noise that trains people to skip a red test.
        def no_helm(_name):
            return None

        original = shutil.which
        shutil.which = no_helm
        try:
            bundle = gen.generate_bundle(
                {"kind": "helm", "domain": "a.example.com", "name": "a"}, REPO
            )
            verdict = gen.verify_bundle(bundle, REPO)
            self.assertEqual(verdict["state"], "unverified")
            self.assertIn("helm", verdict["reason"])
        finally:
            shutil.which = original

    @unittest.skipUnless(shutil.which("helm"), "helm is not installed")
    def test_the_generated_helm_values_render_the_chart(self):
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "a.example.com", "name": "a"}, REPO
        )
        verdict = gen.verify_bundle(bundle, REPO)
        self.assertEqual(verdict["state"], "verified", verdict.get("reason"))

    @unittest.skipUnless(shutil.which("docker"), "docker is not installed")
    def test_the_generated_compose_stack_renders(self):
        for kind in ("compose-small", "compose-enterprise"):
            with self.subTest(kind=kind):
                bundle = gen.generate_bundle(
                    {"kind": kind, "domain": "a.example.com", "name": "a"}, REPO
                )
                verdict = gen.verify_bundle(bundle, REPO)
                self.assertEqual(verdict["state"], "verified", verdict.get("reason"))


class ManifestIntegration(unittest.TestCase):
    def test_the_version_is_the_workspace_version_not_a_caller_claim(self):
        # A caller cannot label a bundle with a version the platform does not have. This is
        # the same reason the manifest builder refuses: it is the oldest release bug there
        # is, and it is entirely preventable by reading one file.
        bundle = gen.generate_bundle(
            {"kind": "helm", "domain": "a.example.com", "name": "a", "version": "9.9.9"}, REPO
        )
        self.assertEqual(bundle["version"], release.workspace_version(REPO))

    def test_the_bundle_is_json_serialisable_in_full(self):
        # The API route stores this document; a BundleError with a set or a Path in it would
        # fail at the INSERT rather than at generation, where nobody is looking.
        bundle = gen.generate_bundle(
            {"kind": "compose-enterprise", "domain": "a.example.com", "name": "a"}, REPO
        )
        self.assertEqual(json.loads(json.dumps(bundle)), bundle)


class PipelineScanIsTheSameRule(unittest.TestCase):
    def test_the_generator_imports_the_pipelines_credential_rule(self):
        # Two copies of a credential rule drift, and the copy nobody runs is the one that is
        # wrong. The generator must call the pipeline's rule, not re-implement it.
        self.assertIs(gen.release_pipeline._literal_credentials, pipeline._literal_credentials)


if __name__ == "__main__":
    unittest.main()
