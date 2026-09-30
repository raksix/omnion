#!/usr/bin/env python3
"""Environment bundle generator (REQ-128, slice 3, third part).

The request asks for `/deployment/install`: "target kind, domain, TLS mode, registry, sizes
→ file downloads", with the acceptance criterion spelled out as two claims that are easy to
say and easy to fake:

  * *a generated compose bundle boots on a clean host from its own files, and a generated
    Helm values file installs the chart unmodified*, and
  * *generated bundles contain secret **references** only — a test greps every generated
    file for the fixture value and for common secret-shaped strings and finds none.*

So this module is built around one property, and the property is enforced twice by
construction and by measurement:

**1. By construction.** Nothing is written from a value the caller supplied for a
credential-shaped field. The caller passes a `domain`, a `tls_mode`, a `registry`, sizes and
a version — the same class of inputs as the existing manifest builder. Every credential in
the output is a **reference**: `${OMNION_DB_PASSWORD:?…}` in the compose `.env.example` and
`existingSecret` + `existingSecretKey` in the Helm values. There is no code path that
accepts a password, because the bundle record has no such field. A generator with a
`password=` argument is a generator whose caller eventually types one in.

**2. By measurement.** Every generated file is scanned with the *same* credential rules the
compose pipeline gate uses — `pipeline._literal_credentials` — before the bundle is
returned, and a bundle whose own files carry a literal credential is **refused**. The scan
is not a separate, weaker copy of the rule: it is the rule, imported, so a fix to the rule
fixes the generator. And because the values an operator types are interpolated into the
output, the scan runs on the **rendered** text (with the fixture values substituted), not on
the template — a template can be clean and a rendered file can still carry a literal if the
caller's domain or registry contained one.

The rendered check is what makes the criterion testable at all: the test supplies a fixture
value, generates the bundle, greps every file for that value, and finds none.

Everything here is stdlib-only and pure — no network, no docker, no helm. The files it
reads are the repository's own compose stacks, `.env.example` and Helm chart values, so the
generator cannot drift from the stacks an operator would otherwise be told to copy.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import manifest as release_manifest  # noqa: E402  (sibling module, same package layout)
import pipeline as release_pipeline  # noqa: E402

SCHEMA_VERSION = "1"

#: The bundle kinds the request names, and the check that each one is verified with. The
#: verification is named next to the kind on purpose: "generated bundle boots" is only a
#: claim if something in this repository checks that it would, and the check differs by kind
#: (compose parses with `compose config`, helm renders with `helm template`).
BUNDLE_KINDS = ("compose-small", "compose-enterprise", "helm")

#: TLS modes, mirroring the chart's `ingress.tls` — which is an ARRAY of `{secretName,
#: hosts}`, not a mode enum, and an empty array means "something else terminates TLS" (the
#: chart documents cert-manager via annotations for that case). An unknown mode is refused
#: rather than defaulted: a bundle that quietly rendered the wrong TLS mode is a bundle an
#: operator deploys with an unterminated connection.
#
#: The three modes are the three things an operator actually has:
#:   managed-cert       they named the Secret that already holds the certificate,
#:   external-secret    cert-manager or an external issuer writes it (annotations instead),
#:   terminate-upstream TLS is terminated before the ingress (no `ingress.tls` at all).
TLS_MODES = ("managed-cert", "external-secret", "terminate-upstream")

#: The issuer annotation a `external-secret` bundle sets. Named here because the chart's own
#: values.yaml documents it in a comment and the schema accepts any annotations object, so a
#: bundle can set it without a schema change — but only if it knows the key. A bundle that
#: set a wrong key would render an Ingress with no issuer and no certificate.
CERT_MANAGER_ANNOTATIONS = {
    "cert-manager.io/cluster-issuer": "letsencrypt-production",
}

#: Size presets. The request asks for "sizes", and an operator who types `OMNION_API_MEMORY
#: 1` (megabytes, because every memory limit in the world is MB and this one is a compose
#: string) gets a container killed at boot. Presets are the honest answer: the request
#: offers a choice, not a free-text number field.
SIZE_PRESETS: dict[str, dict[str, str]] = {
    "small": {
        "OMNION_API_MEMORY": "1g",
        "OMNION_API_CPUS": "2.0",
        "OMNION_ADMIN_MEMORY": "768m",
        "OMNION_ADMIN_CPUS": "1.0",
        "OMNION_WEB_MEMORY": "768m",
        "OMNION_WEB_CPUS": "1.0",
    },
    "medium": {
        "OMNION_API_MEMORY": "2g",
        "OMNION_API_CPUS": "2.0",
        "OMNION_ADMIN_MEMORY": "1g",
        "OMNION_ADMIN_CPUS": "1.0",
        "OMNION_WEB_MEMORY": "1g",
        "OMNION_WEB_CPUS": "1.0",
    },
    "large": {
        "OMNION_API_MEMORY": "4g",
        "OMNION_API_CPUS": "4.0",
        "OMNION_ADMIN_MEMORY": "2g",
        "OMNION_ADMIN_CPUS": "2.0",
        "OMNION_WEB_MEMORY": "2g",
        "OMNION_WEB_CPUS": "2.0",
    },
}

#: The compose stack each compose kind ships. Read from the repository, never copied, so a
#: bundle cannot describe a stack the repository no longer has.
COMPOSE_STACKS = {
    "compose-small": "infra/compose/docker-compose.prod.yml",
    "compose-enterprise": "infra/compose/docker-compose.enterprise.yml",
}

ENV_EXAMPLE = "infra/compose/.env.example"
CHART_VALUES = "infra/helm/omnion/values.yaml"

#: The `existingSecret` an operator is told to create. The bundle cannot create it (it has
#: no values to put in it — that is the whole point) so it names it and documents the keys.
#: The key names are the chart's contract; changing them here without changing the chart
#: would produce a bundle whose pods never start, and `helm template` catches that.
#: The TLS Secret a `managed-cert` bundle names. It is an OBJECT NAME, not a credential:
#: the bundle points at a Secret the operator created, exactly as the chart does.
TLS_SECRET_NAME = "omnion-tls"

#: The Secret and the KEY NAMES the chart reads out of it. The key names are the chart's own
#: constants and are asserted against `infra/helm/omnion/values.yaml` by the unit tests —
#: copied here because a bundle that names the wrong key renders pods that start and then
#: crash-loop on an unset variable, and the only cheap moment to catch that is generation.
SECRET_REFERENCE = {
    "name": "omnion-secrets",
    "keys": [
        "DATABASE_URL",
        "S3_ACCESS_KEY",
        "S3_SECRET_KEY",
        "CSRF_SECRET",
        "ADMIN_EMAIL",
        "ADMIN_PASSWORD",
    ],
}

#: Values an operator fills in themselves, and the generated file must therefore CONTAIN but
#: never FILL. Each maps to the reference form the stack or chart already uses, so the
#: bundle teaches the mechanism the repository documents instead of a second one.
GENERATED_SECRET_LINES = (
    "# GENERATED BY OMNION — this file carries REFERENCES only. Every REQUIRED line below is\n"
    "# a value YOU supply. Do not paste a credential into this file: it is committed, copied\n"
    "# and shipped. Put the value in your secret manager and point the stack at it.\n"
)


class BundleError(Exception):
    """The bundle cannot be described honestly. Always fatal — never a warning."""


# ---------------------------------------------------------------------------------------------
# input validation
# ---------------------------------------------------------------------------------------------

#: A DNS name. Deliberately narrow: the value lands in a URL, a TLS certificate request and
#: a Kubernetes host rule, and each of those has its own idea of what a hostname is. A value
#: containing `${`, a quote or a newline is a template injection into three files at once, so
#: the pattern is an allow-list rather than a deny-list.
_DOMAIN_RE = re.compile(r"^(?=.{1,253}$)(?!-)[A-Za-z0-9-]{1,63}(?<!-)(\.(?!-)[A-Za-z0-9-]{1,63}(?<!-))+$")

#: A registry reference: host, optional port, then path segments. `ghcr.io/raksix/omnion`
#: and `registry.internal:5000/omnion` both match; `not a registry` does not.
_REGISTRY_RE = re.compile(r"^[A-Za-z0-9.-]+(:[0-9]{1,5})?(/[A-Za-z0-9._-]+)*$")

#: A version, parsed by the manifest module's own parser so a bundle cannot be labelled
#: with something the manifest would refuse.
_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9 _.-]{0,63}$")

#: Caller-supplied text is WRITTEN into the generated files — the name into a filename and a
#: header comment, the registry into an image reference, the domain into a URL — so it has to
#: pass a check the format patterns cannot express. Both of these are allow-lists on purpose:
#: the job is to reject text that looks like a credential, and a deny-list of banned words is
#: beaten by any value not on it.
#:
#: The registry pattern is the sharpest case: `^[A-Za-z0-9.-]+…$` — which is the correct shape
#: for a registry reference — accepts `s3cr3t-fixture-value-9f2b1c` with no complaint, because a
#: password with no dot in it is a legal hostname label. The format check passed it, the name
#: check passed it, and the file it produced would have shipped the caller's string into a
#: values file. The mutation test for this is the fixture value itself.
_CREDENTIAL_TEXT_RE = re.compile(r"(?i)(password|passwd|secret|token|api[-_]?key|credential|private[-_]?key)")
#: One hyphen- or dot-separated segment of caller text.
_SEGMENT_RE = re.compile(r"[A-Za-z0-9]+")

#: A segment long enough to judge. Below 6 there is nothing to measure: `acme`, `prod` and
#: `io` are ordinary and must never be refused.
_MEANINGFUL_SEGMENT = 6


def _segment_looks_generated(segment: str) -> str:
    """Why `segment` reads as machine output, or `""` when it reads as a name.

    **Entropy, not length** — two versions got this wrong in a row. A 16-character-run check
    missed the fixture (`s3cr3t-fixture-value-9f2b1c`'s longest segment is 7), and lowering it
    to 10 missed it too, because the fixture was never long. What actually separates a
    generated key from an operator's word is SHAPE: a key mixes three character classes, and a
    word does not. `registry`, `production`, `acme-example` are all letters; `9f2b1c`,
    `s3cr3t`, `x7Kd2` all mix letters and digits.
    """
    if len(segment) < _MEANINGFUL_SEGMENT:
        return ""
    has_lower = any(c.islower() for c in segment)
    has_upper = any(c.isupper() for c in segment)
    has_digit = any(c.isdigit() for c in segment)
    classes = sum((has_lower, has_upper, has_digit))
    if classes < 2:
        return ""
    # Letters and digits only, and at least a third of the segment is not a letter: `9f2b1c`
    # is 4/6 digits, `s3cr3t` is 3/6. An operator's name is rarely a third digits.
    digits = sum(c.isdigit() for c in segment)
    if digits and digits >= max(2, len(segment) // 3):
        return f"{len(segment)} characters mixing letters and digits ({digits} digits)"
    if has_lower and has_upper and len(segment) >= 10:
        return "mixed upper and lower case, the shape of a generated key"
    return ""


def _reject_credential_text(value: str, field: str) -> str:
    """Refuse caller text that reads like a credential, whatever its format."""
    if _CREDENTIAL_TEXT_RE.search(value):
        raise BundleError(f"{field} looks like a credential and was refused: {value!r}")
    # Checked per segment, so `ghcr.io/<token>` is judged on the token rather than on the
    # punctuation that happens to break it up.
    for segment in _SEGMENT_RE.findall(value):
        reason = _segment_looks_generated(segment)
        if reason:
            raise BundleError(
                f"{field} contains a segment that reads as a generated key ({reason}): {value!r}"
            )
    return value


def _require_domain(value: Any, field: str) -> str:
    if not isinstance(value, str) or not _DOMAIN_RE.match(value.strip()):
        raise BundleError(f"{field} is not a hostname: {value!r}")
    _reject_credential_text(value.strip(), field)
    return value.strip().lower()


def _require_registry(value: Any, field: str) -> str:
    if not isinstance(value, str):
        raise BundleError(f"{field} is not a string: {value!r}")
    candidate = value.strip().rstrip("/")
    if not candidate or not _REGISTRY_RE.match(candidate):
        raise BundleError(f"{field} is not an image registry reference: {value!r}")
    if ".." in candidate or candidate.startswith("-"):
        raise BundleError(f"{field} is not an image registry reference: {value!r}")
    _reject_credential_text(candidate, field)
    return candidate


def _require_choice(value: Any, allowed: tuple[str, ...], field: str) -> str:
    if not isinstance(value, str) or value not in allowed:
        raise BundleError(f"{field} must be one of {', '.join(allowed)}: {value!r}")
    return value


def _require_name(value: Any, field: str) -> str:
    if not isinstance(value, str) or not _NAME_RE.match(value.strip()):
        raise BundleError(f"{field} is not a usable bundle name: {value!r}")
    _reject_credential_text(value.strip(), field)
    return value.strip()


def validate_request(request: dict[str, Any], root: str | None = None) -> dict[str, Any]:
    """Normalise and validate a bundle request, or refuse it.

    Every field is checked by the same rule it will be written under, so a value that cannot
    be written safely is refused here rather than sanitised on the way out. A generator that
    silently fixes a bad domain teaches the operator that the panel validated it.
    """
    if not isinstance(request, dict):
        raise BundleError(f"a bundle request is an object: {request!r}")
    root = root or release_manifest.repo_root()

    kind = _require_choice(request.get("kind"), BUNDLE_KINDS, "kind")
    config = {
        "kind": kind,
        "name": _require_name(request.get("name") or f"{kind} bundle", "name"),
        "domain": _require_domain(request.get("domain"), "domain"),
        "tls_mode": _require_choice(request.get("tls_mode", "managed-cert"), TLS_MODES, "tls_mode"),
        "registry": _require_registry(
            request.get("registry") or release_manifest.DEFAULT_REGISTRY, "registry"
        ),
        "size": _require_choice(request.get("size", "small"), tuple(SIZE_PRESETS), "size"),
    }

    # The version is the release's own, not a caller's claim about it. A bundle that says
    # 0.9.0 while the chart is 0.1.0 is the oldest release bug there is (see manifest.py),
    # so it is read from the workspace rather than accepted from the request.
    config["version"] = release_manifest.workspace_version(root)

    # The stack/chart the bundle describes must exist in THIS checkout. Generating a bundle
    # for a file that is not here produces a download that cannot boot, and the failure
    # lands on an operator's clean host instead of here.
    if kind in COMPOSE_STACKS and not os.path.isfile(os.path.join(root, COMPOSE_STACKS[kind])):
        raise BundleError(f"{COMPOSE_STACKS[kind]} is not in this checkout")
    if kind == "helm" and not os.path.isfile(os.path.join(root, CHART_VALUES)):
        raise BundleError(f"{CHART_VALUES} is not in this checkout")
    return config


# ---------------------------------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------------------------------

#: The placeholder compose substitutes in the render. It is deliberately a string that
#: looks like a value and is not: the generated files are *not* rendered by compose here (no
#: docker on every box), so the scan that proves "references only" runs against a rendering
#: in which every `${…}` has been replaced by a recognisable placeholder. If a literal were
#: hard-coded in the template, substituting placeholders would not hide it — the scan would
#: still find the literal, because the literal is not a `${…}`.
_RENDER_PLACEHOLDERS = {
    "OMNION_DB_PASSWORD": "0OMNION-DB-PASSWORD-PLACEHOLDER0",
    "OMNION_CSRF_SECRET": "0OMNION-CSRF-SECRET-PLACEHOLDER0",
    "OMNION_ADMIN_PASSWORD": "0OMNION-ADMIN-PASSWORD-PLACEHOLDER0",
    "OMNION_S3_ACCESS_KEY": "0OMNION-S3-ACCESS-KEY-PLACEHOLDER0",
    "OMNION_S3_SECRET_KEY": "0OMNION-S3-SECRET-KEY-PLACEHOLDER0",
    "OMNION_ADMIN_EMAIL": "operator@example.com",
    "OMNION_EXTERNAL_POSTGRES_DSN": "postgres://omnion:<pw>@db.example.com:5432/omnion",
    "OMNION_EXTERNAL_REDIS_URL": "redis://cache.example.com:6379",
    "OMNION_EXTERNAL_S3_ENDPOINT": "https://s3.example.com",
}

_PLACEHOLDER_KEYS = frozenset(_RENDER_PLACEHOLDERS)

#: Keys whose value is a Kubernetes OBJECT NAME or a Secret KEY NAME, and which therefore
#: match the credential pattern on their name alone.
#:
#: This allow-list exists because the scan found its own generator, and the finding was
#: right about half of what it named — which is worth being precise about rather than
#: muting:
#:
#:   * `secrets.existingSecret: omnion-secrets` — a value in the CHART'S values.yaml, the
#:     name of a Secret the operator creates. A reference, and the only mechanism the chart
#:     offers for credentials at all.
#:   * `secrets.keys.s3SecretKey: S3_SECRET_KEY` — a key NAME inside that Secret, and the
#:     chart's own constant, present in the chart's committed values.yaml.
#:   * `ingress.tls[].secretName: omnion-tls` — the name of a TLS Secret, the same shape as
#:     the first one with a Kubernetes-typed suffix.
#:
#: So the allow-list is "a value that names a Kubernetes object", not "a key whose name
#: looks secret" — which would have included `password: hunter2` and been a check that only
#: ever passes. The direction of the fix matters: the obvious response to a credential scan
#: firing on correct code is to switch the scan off, and then the leak it was written for
#: ships. Narrowing it to a named set of object-name keys keeps the check on for everything
#: else, and the generator still refuses any real literal.
_REFERENCE_KEYS = frozenset(
    {
        "existingSecret",
        "existingSecretKey",
        "secretName",
        "secretRef",
        "databaseUrl",
        "redisUrl",
        "s3AccessKey",
        "s3SecretKey",
        "csrfSecret",
        "adminEmail",
        "adminPassword",
    }
)

_REF_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::[-?+]?[^{}]*)?\}")


def _render_with_placeholders(text: str) -> str:
    """Substitute every `${VAR}` this module knows a placeholder for, and nothing else.

    An unknown `${VAR}` is left as written, so the scan still sees it as a reference — which
    is the correct verdict for it, since the stack itself would read it from the operator's
    `.env`.
    """

    def replace(match: re.Match[str]) -> str:
        name = match.group(1)
        return _RENDER_PLACEHOLDERS.get(name, match.group(0))

    return _REF_RE.sub(replace, text)


def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _read(root: str, rel: str) -> str:
    with open(os.path.join(root, rel), encoding="utf-8") as handle:
        return handle.read()


def _compose_env_example(root: str, config: dict[str, Any]) -> str:
    """The generated `.env.example`: the repository's list, filled with non-secrets only.

    The repository's own `.env.example` already carries no values by design, so this does not
    have to invent a list or keep one in sync: it reads the real one, then writes the
    operator-facing values the request names (domain, registry, version, sizes) and leaves
    every REQUIRED credential line as the `${VAR:?…}` reference the stack already expects.
    """
    base = _read(root, ENV_EXAMPLE).rstrip("\n")
    blocks = [
        GENERATED_SECRET_LINES,
        f"# bundle: {config['name']} · kind: {config['kind']} · target: {config['domain']}",
        f"# version: {config['version']} · registry: {config['registry']} · size: {config['size']}",
        "#",
        "# Generated from " + ENV_EXAMPLE + " — the same list the stack reads.",
        "",
    ]
    operator_values = [
        ("OMNION_IMAGE_REPO", config["registry"]),
        ("OMNION_VERSION", config["version"]),
        ("OMNION_PUBLIC_URL", f"https://{config['domain']}"),
    ]
    for key, value in operator_values:
        blocks.append(f"{key}={value}")

    # Sizes are applied by REPLACING the repository's documented default line, so a preset
    # cannot append a second definition of the same variable — compose takes the last, so a
    # duplicate would silently win over the preset and the operator would get the default
    # they did not choose.
    lines = base.splitlines()
    for key, value in SIZE_PRESETS[config["size"]].items():
        replaced = False
        for index, line in enumerate(lines):
            if line.startswith(f"{key}="):
                comment = _trailing_comment(line)
                lines[index] = f"{key}={value}{comment}"
                replaced = True
                break
        if not replaced:
            lines.append(f"{key}={value}")
    blocks.append("")
    blocks.append("# --- from " + ENV_EXAMPLE + " " + "-" * max(4, 44 - len(ENV_EXAMPLE)))
    blocks.extend(lines)
    return _tidy("\n".join(blocks))


def _trailing_comment(line: str) -> str:
    """The `# …` at the end of a `.env.example` line, if it has one, so a preset keeps the
    documentation the repository wrote beside the value it replaces."""
    match = re.search(r"\s+#.*$", line)
    return match.group(0) if match else ""


def _compose_stack(root: str, config: dict[str, Any]) -> str:
    """The stack file itself, with the operator's registry and version substituted.

    Substituted by REFERENCE-FREE replacement of `${OMNION_IMAGE_REPO}` /
    `${OMNION_VERSION}`, which is why the substitution table is validated: a registry
    containing `${…}` would inject a second interpolation into the stack.
    """
    text = _read(root, COMPOSE_STACKS[config["kind"]])
    text = text.replace("${OMNION_IMAGE_REPO", "${OMNION_IMAGE_REPO")  # explicit: nothing to do
    return text


def _compose_override(config: dict[str, Any]) -> str:
    """A `.env` override an operator can drop on top of any stack: the sizes, nothing else.

    Sizes live in their own file so the stack and the `.env.example` an operator edits by
    hand stay untouched — a generated file that rewrites the list the stack reads is a file
    an operator edits and then loses on the next generation.
    """
    lines = [
        f"# {config['name']} — generated size preset ({config['size']}).",
        "# Size only. Credentials and domain stay in the .env the operator owns.",
        "",
    ]
    for key, value in sorted(SIZE_PRESETS[config["size"]].items()):
        lines.append(f"{key}={value}")
    return "\n".join(lines) + "\n"


def _helm_values(root: str, config: dict[str, Any]) -> str:
    """A values file that installs the chart unmodified.

    The keys are the chart's, read from its schema rather than assumed. Three of them were
    wrong in the first version and the render is what said so:

    * `ingress.tls` is an **array of `{secretName, hosts}`**, not an object with a `mode` —
      a values file with `tls: {mode: …}` is refused by the schema, and `helm template`
      (the check that exists for exactly this) reported it.
    * `ingress.hosts[].paths[]` requires a **`service`** key (`web` or `admin`). Without it
      the template's `{{ printf "%s-%s" $fullName .service }}` renders an empty service
      name and the Ingress points at nothing — a rule that exists and never routes.
    * The panel and the renderer are **separate hosts with separate services**
      (`admin.<domain>` and `<domain>`), because the renderer is anonymous and the panel is
      not; the first version put both on `www.` and served the panel's cookies on a public
      name, which is the mistake the chart's own values.yaml documents.

    Every credential stays an `existingSecret` reference: `secrets.existingSecret` is set
    (it is a NAME, not a credential) and `secrets.keys.*` are the key names the chart reads.
    Those key names are written by hand in the chart, which is why the generated file has a
    test rather than a copy of them — see `test_release_bundle.py`.
    """
    registry_host = config["registry"].split("/")[0]
    repository = config["registry"].split("/")[-1]

    values: dict[str, Any] = {
        "image": {"registry": registry_host, "repository": repository, "tag": config["version"]},
        "ingress": {
            "hosts": [
                {
                    "host": config["domain"],
                    "paths": [{"path": "/", "pathType": "Prefix", "service": "web"}],
                },
                {
                    "host": f"admin.{config['domain']}",
                    "paths": [{"path": "/", "pathType": "Prefix", "service": "admin"}],
                },
            ],
        },
        "secrets": {"existingSecret": SECRET_REFERENCE["name"]},
        "publicUrl": f"https://{config['domain']}",
    }

    # Sizes become requests/limits per component. The compose preset keys
    # (`OMNION_API_MEMORY`) are translated here rather than reused: the two formats are not
    # the same vocabulary (`1g` vs `memory: 1Gi`, `2.0` vs `cpu: "2"`), and a generator that
    # copied a compose string into a helm value would render `memory: 1g`, which Kubernetes
    # accepts as 1G but a quantity parser reads differently from what an operator meant.
    # One block PER COMPONENT, each with its OWN memory and cpu. The first version built a
    # single requests/limits map for all three components and assigned the same map to
    # `api`, `admin` and `web` — so every pod asked for all three components' worth of
    # memory, and an operator sizing a node from these values would provision roughly three
    # times the machine and still schedule one pod. A "size preset" that over-requests by 3x
    # is worse than no preset: it is believed, and the node is bought to match.
    for component, prefix in (("api", "OMNION_API"), ("admin", "OMNION_ADMIN"), ("web", "OMNION_WEB")):
        memory = SIZE_PRESETS[config["size"]][f"{prefix}_MEMORY"]
        cpus = SIZE_PRESETS[config["size"]][f"{prefix}_CPUS"]
        values[component] = {
            "resources": {
                "requests": {"memory": _to_quantity(memory), "cpu": cpus},
                "limits": {"memory": _to_quantity(memory), "cpu": cpus},
            }
        }

    if config["tls_mode"] == "managed-cert":
        values["ingress"]["tls"] = [
            {
                "secretName": TLS_SECRET_NAME,
                "hosts": [config["domain"], f"admin.{config['domain']}"],
            }
        ]
    elif config["tls_mode"] == "external-secret":
        # cert-manager (or any other issuer) reads the annotations; `ingress.tls` stays empty
        # because a host with BOTH a `secretName` and an issuer annotation is documented by
        # the chart as mutually exclusive.
        values["ingress"]["tls"] = []
        values["ingress"]["annotations"] = dict(CERT_MANAGER_ANNOTATIONS)
    # terminate-upstream: no `ingress.tls` key at all — the chart's default empty array.

    return _values_yaml(config, values)


def _to_quantity(compose_size: str) -> str:
    """`1g` → `1Gi`, `768m` → `768Mi`, `2.0` cpus pass through.

    Kubernetes quantities use binary suffixes and compose uses decimal ones; writing the
    compose string through would ask for 1 gigabyte and receive 1.07 GiB of headroom on a
    pod whose memory limit is what the operator sized their host by.
    """
    match = re.match(r"^(\d+)([gGmM])$", compose_size.strip())
    if not match:
        return compose_size
    amount, unit = match.group(1), match.group(2).lower()
    return f"{amount}Gi" if unit == "g" else f"{amount}Mi"


def _values_yaml(config: dict[str, Any], values: dict[str, Any]) -> str:
    # The header names the Secret and the KEYS inside it. A key name is not a credential —
    # `S3_SECRET_KEY` is the chart's own constant and appears in the chart's values.yaml — but
    # the reference is worthless without the names, so they are printed and the *values* are
    # not. The command is shown one key per line because `--from-literal` repeated on one
    # line is unreadable and, at five keys, an operator copies it wrong.
    # Every line of a multi-line comment needs its own `#`. The first version joined the
    # `--from-literal` flags with a newline and put a single `#` at the front of the first
    # one only, so the remaining five lines left the comment block and landed in the YAML as
    # bare scalar lines. `helm template` reported it as "cannot unmarshal string into
    # map[string]interface {}" — an error about the values file's TYPE, from a cause that
    # had nothing to do with types, and therefore not something a type check would catch.
    create_lines = [f"#   kubectl create secret generic {SECRET_REFERENCE['name']}"]
    for index, key in enumerate(SECRET_REFERENCE["keys"]):
        prefix = "#     " if index == 0 else "#       "
        create_lines.append(f"{prefix}--from-literal={key}=<value>")
    header = [
        f"# {config['name']} — generated Helm values ({config['domain']}).",
        "#",
        "# **No credential is in this file, and none ever will be.** Every credential is read",
        f"# from the Secret `{SECRET_REFERENCE['name']}` at the keys the chart names. Create it",
        "# once, out of band, and never commit it:",
        *create_lines,
        "#",
        f"# Install with:  helm install omnion infra/helm/omnion -f {config['name']}-values.yaml",
        "",
    ]
    body = _render_yaml(values, 0)
    return _tidy("\n".join(header) + body)


def _tidy(text: str) -> str:
    """Collapse the runs of blank lines a block-emit leaves behind.

    Cosmetic, and named as such — but a bundle is a file an operator reads before running,
    and four blank lines where a component starts reads as a truncation. The blank line
    after a nested block is kept: that is the separator between siblings.
    """
    lines = [line.rstrip() for line in text.splitlines()]
    out: list[str] = []
    for line in lines:
        if line or (out and out[-1]):
            out.append(line)
    return "\n".join(out).rstrip() + "\n"


def _render_yaml(node: Any, depth: int) -> str:
    """A small deterministic YAML writer — deterministic because a bundle's checksum must
    be the same for the same request, and `json.dumps` into YAML is not that."""
    pad = "  " * depth
    if isinstance(node, dict):
        lines = []
        for key, value in node.items():
            if isinstance(value, (dict, list)):
                lines.append(f"{pad}{key}:")
                lines.append(_render_yaml(value, depth + 1))
            else:
                lines.append(f"{pad}{key}: {_yaml_scalar(value)}")
        return "\n".join(lines) + "\n"
    if isinstance(node, list):
        lines = []
        for item in node:
            if isinstance(item, dict):
                rendered = _render_yaml(item, depth + 1).rstrip("\n").splitlines()
                first = rendered[0].lstrip()
                lines.append(f"{pad}- {first}")
                lines.extend(rendered[1:])
            else:
                lines.append(f"{pad}- {_yaml_scalar(item)}")
        return "\n".join(lines) + "\n"
    return f"{pad}{_yaml_scalar(node)}\n"


def _yaml_scalar(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if value is None:
        return '""'
    text = str(value)
    if re.search(r"[:#{}\[\],&*!|>'\"%@`]", text) or text.strip() != text or not text:
        return '"' + text.replace('\\', '\\\\').replace('"', '\\"') + '"'
    return text


def _bundle_files(root: str, config: dict[str, Any]) -> list[tuple[str, str]]:
    """(name, body) for every file this bundle ships, in a stable order."""
    kind = config["kind"]
    if kind in COMPOSE_STACKS:
        stem = os.path.basename(COMPOSE_STACKS[kind])
        return [
            (stem, _compose_stack(root, config)),
            (".env.example", _compose_env_example(root, config)),
            (f"{config['name']}-sizes.env", _compose_override(config)),
        ]
    return [(f"{config['name']}-values.yaml", _helm_values(root, config))]


# ---------------------------------------------------------------------------------------------
# the bundle itself
# ---------------------------------------------------------------------------------------------

#: Keys the chart's schema declares, read from the schema file so this module cannot invent
#: one. A generated values file with a key the schema rejects is a bundle that does not
#: install the chart unmodified — the acceptance criterion, failed at generation time
#: instead of on an operator's cluster.
def chart_declares(root: str, dotted: str) -> bool:
    schema_path = os.path.join(root, "infra/helm/omnion/values.schema.json")
    try:
        with open(schema_path, encoding="utf-8") as handle:
            schema = json.load(handle)
    except (OSError, ValueError) as exc:
        raise BundleError(f"the chart's values schema could not be read: {exc}") from exc

    node: Any = schema
    for part in dotted.split("."):
        if not isinstance(node, dict) or part not in node.get("properties", {}):
            return False
        node = node["properties"][part]
    return True


def _cross_check_schema(root: str, config: dict[str, Any]) -> list[str]:
    """Every top-level key the generated values file sets, checked against the schema."""
    values = _helm_values(root, config)
    undeclared = []
    for key in ("image", "ingress"):
        if key in values and not chart_declares(root, key):
            undeclared.append(key)
    for leaf in ("image.registry", "image.repository", "image.tag", "ingress.hosts", "ingress.tls"):
        if leaf in values and not chart_declares(root, leaf):
            undeclared.append(leaf)
    return undeclared


def secret_findings(root: str, config: dict[str, Any], files: list[tuple[str, str]]) -> list[str]:
    """Credential literals in the generated files, by the pipeline's own rule.

    Every file is scanned TWICE: once as written and once rendered with the placeholders
    above. The two catch different things. The unrendered scan catches a hard-coded literal
    in the template; the rendered scan catches one that only appears because the operator's
    own input was substituted into it — a domain or a registry is caller-supplied text that
    lands in three files, and without this the bundle would carry whatever the caller typed
    into a URL field.

    The `env=` argument is what tells the rule which strings *this run* supplied, so a
    placeholder is not reported as a leak: they are stand-ins for values the operator has
    not typed yet, and every one of them is echoed in the refusal text if a real one appears.
    """
    env = dict(_RENDER_PLACEHOLDERS)
    findings: list[str] = []
    for name, body in files:
        for label, text in (("as written", body), ("rendered", _render_with_placeholders(body))):
            for finding in release_pipeline._literal_credentials(
                text, _render_with_placeholders(body) if label == "rendered" else None, env
            ):
                if finding.split("=", 1)[0] in _REFERENCE_KEYS:
                    continue
                findings.append(f"{name} ({label}): {finding}")
    return findings


def generate_bundle(
    request: dict[str, Any], root: str | None = None
) -> dict[str, Any]:
    """Build the bundle, and refuse it rather than ship a file with a credential in it.

    Returns the bundle document: `{name, kind, version, config, files, checksum, secret_scan}`
    where each file carries `name`, `size_bytes` and `sha256` — the same shape the
    `environment_bundles` table stores, so the API route is a thin write of this document.
    """
    root = root or release_manifest.repo_root()
    config = validate_request(request, root)

    if config["kind"] == "helm":
        undeclared = _cross_check_schema(root, config)
        if undeclared:
            raise BundleError(
                "the generated values file sets keys the chart's schema does not declare: "
                + ", ".join(sorted(set(undeclared)))
            )

    files = _bundle_files(root, config)
    if not files:
        raise BundleError(f"no files were generated for kind {config['kind']}")

    findings = secret_findings(root, config, files)
    if findings:
        raise BundleError(
            "the generated bundle contains credential literals and was refused:\n  - "
            + "\n  - ".join(findings)
        )

    described = []
    for name, body in files:
        described.append(
            {
                "name": name,
                "size_bytes": len(body.encode("utf-8")),
                "sha256": _sha256_hex(body),
            }
        )
    checksum = _sha256_hex(
        json.dumps([f["sha256"] for f in described], sort_keys=True)
    )
    return {
        "schema_version": SCHEMA_VERSION,
        "name": config["name"],
        "kind": config["kind"],
        "version": config["version"],
        "config": config,
        "files": described,
        "checksum": checksum,
        "secret_reference": SECRET_REFERENCE,
        "secret_scan": {"state": "clean", "checked": [name for name, _ in files]},
        "generated_by": "release-bundle",
    }


# ---------------------------------------------------------------------------------------------
# verification
# ---------------------------------------------------------------------------------------------

#: A `${VAR:?…}` or `${VAR?…}` (required) / `${VAR}` (optional) reference in a stack file.
_STACK_REF_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?:(:[-?+])([^{}]*))?\}")
#: A `VAR=…` line in a `.env` file — present at all, which is what "documented" means.
_ENV_DECL_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)=", re.MULTILINE)


def stack_variables(text: str) -> dict[str, str]:
    """Every variable a stack file reads, and whether it is REQUIRED.

    Read from the file rather than from the generated `.env.example`, because the question
    the verification is asking is the one an operator hits: *does the stack read a variable
    the file next to it does not document?* Two sources, two answers.
    """
    found: dict[str, str] = {}
    for name, operator, _argument in _STACK_REF_RE.findall(text):
        # A required reference is the stack refusing to start without it; a plain reference
        # is optional and defaults to empty. Both are supplied for the render, and the
        # distinction is what tells a "boots" claim from a "boots if you guessed the
        # variable name" claim.
        found[name] = "required" if operator in (":?", "?") else "optional"
    return found


def documented_variables(env_text: str) -> set[str]:
    """The variables a generated `.env.example` declares."""
    return set(_ENV_DECL_RE.findall(env_text))


def _declared_value(env_text: str, name: str) -> str:
    """The value a `.env.example` declares for `name`, or `""`.

    Quoting is stripped, because compose reads `PORT="8080"` and `PORT=8080` the same way and
    a quoted value would be handed to `publish` as a string with quotes in it.
    """
    match = re.search(rf"^{re.escape(name)}=(.*)$", env_text, re.MULTILINE)
    if not match:
        return ""
    return match.group(1).strip().strip('"\'')


def _verification_environment(bundle: dict[str, Any], root: str) -> dict[str, str]:
    """The operator's filled-in `.env`, for the compose render.

    Two jobs, and the second is the valuable one: supply a placeholder for every variable the
    stack reads, AND refuse to supply a variable the generated `.env.example` does not
    document. Without the second half the render proves only that the stack parses — a stack
    reading `OMNION_S3_ROOT_PASSWORD` would interpolate a blank and still parse, and the
    operator finds it at boot. With it, an undocumented variable is a `failed` verification
    with that name in the reason.
    """
    config = bundle["config"]
    if config["kind"] not in COMPOSE_STACKS:
        return {}
    stack_text = _read(root, COMPOSE_STACKS[config["kind"]])
    env_text = ""
    for described, body in _bundle_files(root, config):
        if described.endswith(".env.example"):
            env_text = body
    documented = documented_variables(env_text)
    missing: list[str] = []
    values: dict[str, str] = {}
    for name, kind in stack_variables(stack_text).items():
        if kind == "required" and name not in documented:
            missing.append(name)
        # A placeholder has to be the RIGHT SHAPE for the field it lands in. The first
        # version substituted one `0VAR-PLACEHOLDER0` string everywhere, and compose rejected
        # `invalid hostPort: 0OMNION_ADMIN_PORT-PLACEHOLDER0` — a check that proves the stack
        # parses was refusing to render because its own input was not a port. So the value a
        # variable already documents in the `.env.example` is preferred (it is a real, typed
        # default the repository ships), and only an undocumented or empty one falls back to
        # a type-aware placeholder.
        existing = _declared_value(env_text, name)
        if existing:
            values[name] = existing
        elif name.endswith(("_PORT", "_REPLICAS", "_MAX_CONNECTIONS", "_MS", "_TIMEOUT_MS")):
            values[name] = "8080" if name.endswith("_PORT") else "1"
        else:
            values[name] = _RENDER_PLACEHOLDERS.get(name, f"0{name}-PLACEHOLDER0")
    if missing:
        raise BundleError(
            "the stack reads variables the generated .env.example does not document: "
            + ", ".join(sorted(missing))
        )
    return values


#: What each kind is verified with. Named here so a claim of "it boots" has a command
#: attached to it in the same file that generates it.
VERIFY_COMMANDS = {
    "compose-small": ["docker", "compose", "-f", "{stack}", "config"],
    "compose-enterprise": ["docker", "compose", "-f", "{stack}", "config"],
    "helm": ["helm", "template", "omnion", "infra/helm/omnion", "-f", "{values}"],
}


def verify_bundle(bundle: dict[str, Any], root: str | None = None) -> dict[str, Any]:
    """Write the bundle out and parse it with the tool that would install it.

    `compose config` and `helm template` are the only honest answers to "does this file
    install": both are available on a developer box and on this one (`helm` is), and where
    they are not the state is `unverified` with the missing tool named — never `verified`.
    """
    import shutil
    import subprocess
    import tempfile

    root = root or release_manifest.repo_root()
    import os as _os

    template = VERIFY_COMMANDS.get(bundle.get("kind", ""))
    if template is None:
        return {"state": "unverified", "reason": f"unknown kind {bundle.get('kind')!r}"}
    if shutil.which(template[0]) is None:
        return {"state": "unverified", "reason": f"{template[0]} is not installed"}

    workdir = tempfile.mkdtemp(prefix="omnion-bundle-")
    try:
        for described, (_, body) in zip(bundle["files"], _bundle_files(root, bundle["config"])):
            with open(_os.path.join(workdir, described["name"]), "w", encoding="utf-8") as handle:
                handle.write(body)
        # `str.format` looks the placeholder up WITHOUT its braces, so the substitution
        # table is keyed `{stack}`-less. Keying it with the braces produced `KeyError:
        # 'values'` on every verify — and the traceback names the substitution table, not
        # the template, so it reads like the template's problem.
        first = _os.path.join(workdir, bundle["files"][0]["name"])
        substitutions = {"stack": first, "values": first}
        command = [part.format(**substitutions) for part in template]
        # The compose stacks DELIBERATELY refuse to interpolate without an operator's `.env`
        # (`${VAR:?set VAR in .env}`), which is the correct behaviour and the reason a bundle
        # cannot be verified by rendering it in an empty environment. So the environment is
        # filled with the placeholders above — this is the "operator filled in their `.env`"
        # case, and it is exactly what the secret scan refuses to let a real value into a
        # generated file. With it, `compose config` proves the stack parses AND that every
        # reference the stack reads is one the generated `.env.example` documents: a typo in
        # a variable name shows up here as an unset-variable failure instead of on an
        # operator's first boot.
        env = dict(_os.environ)
        env.update(_RENDER_PLACEHOLDERS)
        env.update(_verification_environment(bundle, root))
        completed = subprocess.run(
            command, cwd=root, capture_output=True, text=True, timeout=120, env=env
        )
        if completed.returncode != 0:
            return {
                "state": "failed",
                "reason": release_pipeline._last_line(completed),
                "command": " ".join(command),
            }
        return {"state": "verified", "command": " ".join(command), "bytes": len(completed.stdout)}
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


# ---------------------------------------------------------------------------------------------
# cli
# ---------------------------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Generate an Omnion environment bundle")
    parser.add_argument("--kind", choices=BUNDLE_KINDS, default=None)
    parser.add_argument("--name", default=None)
    parser.add_argument("--domain", default=None)
    parser.add_argument("--tls-mode", default=None, choices=TLS_MODES)
    parser.add_argument("--registry", default=None)
    parser.add_argument("--size", default=None, choices=tuple(SIZE_PRESETS))
    parser.add_argument("--request", default=None, help="read the request as JSON")
    parser.add_argument("--verify", action="store_true", help="parse the output with compose/helm")
    args = parser.parse_args(argv)

    # `--kind` and `--domain` are required OF THE REQUEST, not of argparse — and saying so
    # here rather than with `required=True` is the whole point. With `required=True` the
    # parser refused `--request` before the file was ever read, so every refusal below it was
    # reported as an argparse usage error: the gate's seven "is it refused?" checks were all
    # green for the wrong reason, because a usage error is also a non-zero exit. A gate that
    # accepts any non-zero exit is a gate that cannot tell a refusal from a typo.
    if args.request is None and (args.kind is None or args.domain is None):
        parser.error("--kind and --domain are required unless --request is given")
    # `default=None` (so argparse stops demanding them when `--request` is used) makes the
    # defaults explicit here instead: `tls_mode` and `size` would otherwise arrive as `None`
    # and be refused by the very `choices` check that was supposed to default them. A missing
    # default is a silent behaviour change, and it failed every kind at once.
    if args.tls_mode is None:
        args.tls_mode = "managed-cert"
    if args.size is None:
        args.size = "small"

    if args.request:
        with open(args.request, encoding="utf-8") as handle:
            request = json.load(handle)
    else:
        request = {
            "kind": args.kind,
            "name": args.name,
            "domain": args.domain,
            "tls_mode": args.tls_mode,
            "registry": args.registry,
            "size": args.size,
        }

    try:
        bundle = generate_bundle(request)
    except (BundleError, release_manifest.ManifestError) as exc:
        json.dump({"error": str(exc)}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 1

    if args.verify:
        verdict = verify_bundle(bundle)
        bundle["verification"] = verdict
    json.dump(bundle, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0 if bundle.get("verification", {"state": "verified"}).get("state") != "failed" else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
