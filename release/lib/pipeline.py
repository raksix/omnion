#!/usr/bin/env python3
"""The release pipeline's decision layer (REQ-128, slice 3).

The previous tick shipped the manifest — the document. This is the thing that decides
**whether a document may be published**, and it is deliberately the part that can run on a
build box. The request's own risk note is the reason: "a pipeline that cannot be exercised
on this box is exactly what the request's own risk note warns about shipping unverified".

So this module never builds anything itself. It:

* derives the release **plan** from the repository (the Dockerfiles' runtime stages, the
  documented CLI platforms, the chart, the compose stacks) rather than from a list kept
  here, so a fourth image or a sixth CLI platform appears in the plan without anyone
  editing this file;
* states, per stage, the **tool** that must run it, and probes this box for that tool —
  so a plan that cannot be executed on this machine is reported as unexecutable instead of
  silently skipped;
* runs the stages that CAN be run for real (`helm package`, `docker compose config`) and
  **writes no digest at all** for the stages it cannot, instead of writing a fabricated
  one;
* refuses to publish while any gate is red, any stage is blocked, the tag disagrees with
  the version, or the facts carry a synthetic marker.

That last rule is the one worth stating plainly. The manifest gate already has
`synthetic_facts()` for its own mutation suite, and a synthetic digest that reaches a
published manifest is a credential-shaped lie in a public repository. So a dry run that
cannot build an image produces a facts file **with a hole in it and a list of the holes**,
and `publish` reads that list. A dry run that invented digests would be green, complete,
and wrong — which is strictly worse than a red one, because a red one gets looked at.

Stdlib only. No network, no registry, no docker import: the tool probe shells out to
`--version` and nothing else, and the docker/helm commands are constructed, not executed by
this module's own logic beyond the two real ones named above.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import manifest as release_manifest  # noqa: E402  (sibling module, same package layout)

#: Marker every dry-run facts file carries, so a facts file produced by this module can
#: never be mistaken for one a real build produced — and so `publish` can refuse it while
#: any stage is blocked.
FACTS_SOURCE = "release-pipeline/dry-run"

#: The gates that must all be green before a tag may publish. Named as data because the
#: request names them: "nothing is published without the CI gate (tests + the REQ-129
#: migration verification) being green". A gate list is a list somebody maintains, so the
#: empty case is a failure, not a pass.
REQUIRED_GATES = ("tests", "migration-verification", "manifest")

GATE_COMMANDS: dict[str, list[str]] = {
    # The release manifest gate: builds a manifest from a fake tag and mutates it nineteen
    # ways. It is also the gate that is currently RED for a repository reason (a theme
    # declares 0.1.1 while the platform releases 0.1.0), which is exactly why running it
    # here rather than trusting a badge matters.
    "manifest": ["bash", "scripts/qa/release-manifest.sh"],
    "tests": ["bash", "scripts/qa/ci-fast.sh"],
    # REQ-129 owns the migration runner; until it lands this gate has no command and says
    # so, which keeps the required-gate list honest instead of quietly short.
    "migration-verification": [],
}


class PipelineError(Exception):
    """The release cannot be published, or cannot be described. Always fatal."""


# ---------------------------------------------------------------------------------------------
# the plan
# ---------------------------------------------------------------------------------------------


def _probe_result(commands: list[str], timeout: int = 60) -> tuple[bool, str]:
    """Whether a command runs here, and why not when it does not.

    Probed by ASKING the command, not by looking for a file: a `docker` binary that cannot
    talk to a daemon, or a `buildx` plugin that is not installed, is present and useless,
    and `shutil.which` reports the first as available and the second as "no such binary"
    — which is a different question from the one being asked. `docker buildx` is a docker
    SUBCOMMAND, so a file probe for `buildx` is wrong in a way that would block every image
    stage on a box with a perfectly good docker.
    """
    if not commands:
        return False, "no command is declared for this probe"
    if shutil.which(commands[0]) is None:
        return False, f"{commands[0]} is not installed"
    try:
        result = subprocess.run(commands, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as exc:  # pragma: no cover - environment
        return False, f"{commands[0]} could not be run: {exc}"
    if result.returncode != 0:
        return False, f"`{' '.join(commands)}` exited {result.returncode}: {_last_line(result)}"
    return True, ""


#: Per-stage requirements. `probe` answers "is this tool usable here" and is run for real;
#: `command` is what the stage WOULD run, and is run for real only where this box can do
#: it. They are separate because "docker exists" and "docker buildx exists" are different
#: facts, and because a stage whose command is a multi-GB build must be reported as
#: unrunnable WITHOUT attempting it — half a build is worse than none, and its output would
#: be a set of files nobody can name a digest for.
STAGES: dict[str, dict[str, Any]] = {
    "image": {"probe": ["docker", "buildx", "version"], "command": ["docker", "buildx", "build"]},
    "cli": {"probe": ["bash", "--version"], "command": ["bash", "release/lib/build-cli.sh"]},
    "chart": {"probe": ["helm", "version"], "command": ["helm", "package"]},
    "sbom": {"probe": ["syft", "version"], "command": ["syft", "packages"]},
    "compose-render": {
        "probe": ["docker", "compose", "version"],
        "command": ["docker", "compose", "config"],
    },
    "manifest": {
        "probe": ["python3", "release/lib/manifest.py", "schema"],
        "command": ["python3", "release/lib/manifest.py", "build"],
    },
}


def plan(root: str | None = None, version: str | None = None) -> list[dict[str, Any]]:
    """Every stage a release of `version` runs, in dependency order.

    **Derived, not declared.** The images come from the Dockerfiles' runtime stages
    (including the two in `admin.Dockerfile`), the CLI targets from
    `manifest.CLI_PLATFORMS`, the compose stacks from the files the manifest publishes, and
    the chart from the manifest's own chart step. A list written down here is a list that
    describes the release pipeline of the day it was written, and the release pipeline is
    exactly the thing that must not drift from the repository.

    Order matters and is the dependency order: images and the chart before SBOMs (an SBOM
    describes an image), the compose render before nothing (it validates the files, it
    produces no artifact), and the manifest last because it consumes every digest.
    """
    root = root or release_manifest.repo_root()
    version = version or release_manifest.workspace_version(root)
    stages: list[dict[str, Any]] = []

    for image in release_manifest.discover_images(root):
        component = image["name"].rsplit("/", 1)[-1]
        stages.append(
            {
                "id": f"image:{component}",
                "kind": "image",
                "target": component,
                "name": image["name"],
                "stage_kind": "image",
                "depends_on": [],
            }
        )

    for platform in release_manifest.CLI_PLATFORMS:
        name = f"omnion-{platform}"
        stages.append(
            {
                "id": f"cli:{platform}",
                "kind": "cli",
                "target": platform,
                "name": name,
                "stage_kind": "cli",
                "depends_on": [],
            }
        )

    stages.append(
        {
            "id": "chart:omnion",
            "kind": "chart",
            "target": "helm",
            "name": f"omnion-{version}.tgz",
            "stage_kind": "chart",
            "depends_on": [],
        }
    )

    # The compose files are VALIDATED, not built: they ship as source. Rendering them with
    # placeholders proves they interpolate, and the digest recorded is of the source file —
    # publishing a render would bake one operator's environment into a public artifact.
    for compose_name in ("docker-compose.prod.yml", "docker-compose.enterprise.yml"):
        stages.append(
            {
                "id": f"compose-render:{compose_name}",
                "kind": "compose",
                "target": compose_name,
                "name": compose_name,
                "stage_kind": "compose-render",
                "depends_on": [],
            }
        )

    for stage in [s for s in stages if s["kind"] == "image"]:
        sbom = f"{stage['target']}.sbom.json"
        stages.append(
            {
                "id": f"sbom:{stage['target']}",
                "kind": "sbom",
                "target": stage["target"],
                "name": sbom,
                "stage_kind": "sbom",
                "depends_on": [stage["id"]],
            }
        )

    stages.append(
        {
            "id": "manifest",
            "kind": None,
            "target": "release",
            "name": f"omnion-{version}-manifest.json",
            "stage_kind": "manifest",
            "depends_on": [s["id"] for s in stages],
        }
    )
    return stages


#: PLACEHOLDER_ENV. Every variable the compose stacks interpolate, with a value that is
#: obviously not a secret. The rendered output is validated and then DISCARDED — only the
#: source file's digest is recorded — so nothing here is ever published. It exists because
#: `docker compose config` refuses an unset required variable, and a validation step that
#: cannot run is a validation step nobody notices is missing.
PLACEHOLDER_ENV = {
    "OMNION_VERSION": "0.0.0-dry-run",
    "OMNION_IMAGE_REPO": release_manifest.DEFAULT_REGISTRY,
    "OMNION_DB_USER": "dryrun",
    "OMNION_DB_PASSWORD": "dryrun-placeholder",
    "OMNION_DB_NAME": "omnion",
    "OMNION_REDIS_URL": "redis://redis:6379",
    "OMNION_S3_ENDPOINT": "http://minio:9000",
    "OMNION_S3_BUCKET": "omnion",
    "OMNION_S3_REGION": "us-east-1",
    "OMNION_S3_ACCESS_KEY": "dryrun-placeholder",
    "OMNION_S3_SECRET_KEY": "dryrun-placeholder",
    "OMNION_PUBLIC_URL": "https://dryrun.invalid",
    "OMNION_CSRF_SECRET": "dryrun-placeholder",
    "OMNION_ADMIN_EMAIL": "dryrun@omnion.invalid",
    "OMNION_ADMIN_PASSWORD": "dryrun-placeholder",
    "OMNION_EXTERNAL_POSTGRES_DSN": "postgres://dryrun@postgres.dryrun.invalid:5432/omnion",
    "OMNION_EXTERNAL_REDIS_URL": "redis://redis.dryrun.invalid:6379",
    "OMNION_EXTERNAL_S3_ENDPOINT": "https://s3.dryrun.invalid",
    "OMNION_S3_EXTERNAL": "1",
    "OMNION_S3_FORCE_PATH_STYLE": "1",
}


# ---------------------------------------------------------------------------------------------
# stages
# ---------------------------------------------------------------------------------------------


def _sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return "sha256:" + digest.hexdigest()


def canonical_archive_digest(path: str) -> str:
    """A digest of a tarball's CONTENTS, independent of when it was packed.

    `helm package` writes the current wall-clock time into every member's mtime, so the
    bytes — and therefore the sha256 — differ on every run of an unchanged chart. Verified
    here rather than assumed: two packs of the same tree, two seconds apart, produced
    different tarball digests and identical member content.

    That makes the raw sha256 the wrong thing to compare for REPRODUCIBILITY, and the right
    thing to record for DOWNLOAD verification — an operator who pulled the artifact
    verifies against bytes, not against a recipe. So the pipeline records both, and this
    function is the one that answers "would a rebuild produce the same chart?".

    mtime is excluded; name, mode, and content are included. Content is what a chart
    template reads, mode is what determines whether it can be executed at all, and name is
    the lookup key — excluding any of the three would make two different charts hash alike.
    """
    import tarfile

    entries: list[str] = []
    with tarfile.open(path, "r:*") as archive:
        for member in sorted(archive.getmembers(), key=lambda m: m.name):
            if not member.isfile():
                continue
            handle = archive.extractfile(member)
            content = handle.read() if handle else b""
            entries.append(
                f"{member.name}\0{oct(member.mode)}\0{hashlib.sha256(content).hexdigest()}"
            )
    return "sha256:" + hashlib.sha256("\n".join(entries).encode("utf-8")).hexdigest()


def _run(command: list[str], cwd: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        command, cwd=cwd, capture_output=True, text=True, timeout=900, env=env
    )


def _last_line(completed: subprocess.CompletedProcess) -> str:
    """The last non-empty output line of a command that failed.

    stderr is preferred over stdout because a gate that fails usually explains itself on
    stderr, and a summary line taken from the wrong stream is a summary of nothing. The
    fallback chain exists because a tool may fail silently on one stream — a timeout writes
    nothing at all — and "failed with no output" is a true and useful sentence.
    """
    for stream in (completed.stderr, completed.stdout):
        for line in (stream or "").splitlines():
            if line.strip():
                return line.strip()
    return "failed with no output"


#: A YAML mapping key whose value is a credential. Deliberately a name pattern rather
#: than a value pattern: a value is unjudgeable (a password may look like a word), and the
#: question being asked of a stack file is never "does this string look secret" but "does
#: this key have a hard-coded value at all". Compose interpolates `${VAR}` in both files,
#: so a credential key with a value in the source is always the defect.
#:
#: **No `\b` word boundaries, and that is a fix rather than a style choice.** The first
#: version of this pattern wrapped every alternative in `\b…\b`, which is the textbook way
#: to write a word match — and it never fired on `OMNION_DB_PASSWORD`, because `_` is a word
#: character, so there is no boundary between `DB_` and `PASSWORD`. Every credential key in
#: this repository is `OMNION_<THING>_PASSWORD` or `OMNION_S3_SECRET_KEY`, which means the
#: pattern matched nothing in either compose stack, and the check reported a clean file it
#: had never read. A check that names the right concepts and finds none of them is worse
#: than no check, because it is believed.
#:
#: Substring matching is correct here precisely because the COST of a false positive is
#: low and the cost of a false negative is a shipped credential: over-matching merely
#: requires a key to be a reference rather than a literal, which is the house rule anyway.
CREDENTIAL_KEY_RE = re.compile(
    r"(?i)(password|passwd|secret|token|api[_-]?key|access[_-]?key|credential|private[_-]?key|dsn)"
)

#: `KEY: value` / `KEY=value` where the value is a bare literal rather than a reference.
#: A reference is `${…}`, `{{…}}` or empty. An empty value is fine — it is the correct way
#: to say "this must be supplied" — and a bare `KEY:` with nothing after it is not a leak.
_ASSIGNMENT_RE = re.compile(r"^\s*([A-Za-z][A-Za-z0-9_.-]*)\s*[:=]\s*(.+?)\s*$")

#: A value that embeds a credential IN ITSELF, whatever the key is called.
#:
#: The first version of this check asked only about the KEY, and the gate caught the gap
#: with a fixture whose key was `DATABASE_URL`: `postgres://admin:leaked@db:5432/omnion` is
#: a shipped password, and no amount of key-name matching finds it. `DSN` was in the key
#: pattern, which is why the repository's own `OMNION_DATABASE_URL` slipped past it — the
#: key says DATABASE, not DSN.
#:
#: **The rule is about the PASSWORD COMPONENT, not the whole userinfo, and the first
#: version of the rule got that wrong in the direction that matters.** Testing the whole
#: userinfo flagged this repository's own `docker-compose.prod.yml`, which composes
#: `postgres://${OMNION_DB_USER}:${OMNION_DB_PASSWORD}@postgres:5432/…` — every part of it a
#: reference, no secret in the file. A check that fires on a correct file gets switched off
#: by whoever it annoys, and the real leak it was written for then ships. So the userinfo is
#: split on `:` and only the part AFTER the colon is judged, because that is the password;
#: a hard-coded username is not a secret and is what the reference form legitimately pins.
_VALUE_CREDENTIAL_RE = re.compile(r"^[^:]+://([^/\s@]*):([^/\s@]*)@")

#: A part of a composed value that is not a reference at all. `omni` in
#: `postgres://omni:${OMNION_DB_PASSWORD}@db` is fine; `leaked` in
#: `postgres://omni:leaked@db` is a shipped password.
_LITERAL_SEGMENT_RE = re.compile(r"[A-Za-z0-9!#$%&'+\-.=^_`|~]")

#: A reference, in every form compose and helm use. Stripped before the literal test, so
#: what remains is the text an operator actually typed.
_REFERENCE_SEGMENT_RE = re.compile(r"\$\{[^{}]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\{\{.*?\}\}")

#: The only value in a rendered compose document that is expected to be a literal: the
#: image tag. `image: ghcr.io/raksix/omnion/api:0.1.0` is an artifact reference and must
#: not be read as an assignment to a key named `image`.
_RENDER_LITERAL_KEYS = {"image", "build", "container_name", "name", "command", "entrypoint"}

#: The keys a rendered document is expected to carry that are not credentials at all. The
#: compose render is a FLOW-STYLE document — `environment: {FOO=bar, BAZ=qux}` on one
#: line — so a line scanner sees the entire block as one `key=value` pair and matches it
#: against the first identifier in the line, which is not a key. That produced 100+ false
#: positives on a clean render, every one of them `condition=service_healthy` or
#: `restart=unless-stopped` read out of the middle of a map.
#:
#: So a rendered document is scanned only for the things a line scanner CANNOT confuse with
#: a credential: a value carrying URL userinfo, and a `KEY=value` pair whose KEY matches a
#: credential name AND whose value is not one of the supplied placeholders. The
#: credential-key test keeps the key pattern, which is what makes it survive a flow line —
#: the line is `environment: {PORT=3000, …}`, and the key pattern finds `PORT=3000` inside
#: it, correctly matching nothing.
_RENDER_SCAN_RE = re.compile(r"([A-Za-z][A-Za-z0-9_.-]*)=(\"[^\"]*\"|'[^']*'|[^,}\s]+)")


#: Compose's shell-expansion operators inside `${…}`: `${VAR:?message}`,
#: `${VAR:-default}`, `${VAR:+alt}`. The text after the operator is the operator's own
#: argument, never a literal the deployer typed, so it is removed before any test that asks
#: "did someone hard-code something here".
#:
#: The operator class is `[:?+-]` with the `-` LAST, and that ordering is not cosmetic:
#: written `[:-+]` the parser reads `-` and `+` as the ends of a character RANGE and the
#: module raises `re.error: bad character range` at import — so the check died before it
#: could report anything, and every caller got an exception rather than a verdict.
_SHELL_OPERATOR_RE = re.compile(r"\$\{[A-Za-z_][A-Za-z0-9_]*[:?+-][^{}]*\}")


def _strip_shell_operators(value: str) -> str:
    """`${VAR:?set VAR in .env}` → `${VAR}`. What the operator carries is its own text.

    Written as a substitution with an explicit name and argument rather than a clever
    lambda, because the first version used a conditional expression inside the replacement
    function and had a branch that returned its input unchanged — so `${VAR:-default}`
    passed through and the check that depended on it kept reporting the repository's own
    correct compose lines as hard-coded.
    """

    def replace(match: re.Match[str]) -> str:
        name = match.group(0)[2:].split(":")[0].split("-")[0].split("+")[0]
        return "${" + name + "}"

    return _SHELL_OPERATOR_RE.sub(replace, value)


def _is_pure_reference(value: str) -> bool:
    """Whether an assigned value is made of references and nothing else.

    The single rule the whole credential scan turns on, so it is stated once and by example
    rather than re-derived at each call site — the previous version asked "does this contain
    a `${`?", which is true for `${Y}-appended` and false for `hunter2`, so it passed the
    case that ships a literal and failed the case that obviously does.

    An empty value counts as pure: `API_KEY:` with nothing after it is how compose says
    "this must be supplied", and refusing it would be refusing the correct form.
    """
    stripped = value.strip().strip('"\'')
    if not stripped:
        return True
    # Anything that is not assignment-shaped is not a value at all (a list opener, an
    # anchor, a tag) and carries no literal.
    if stripped.startswith(("[", "{", "&", "*", "!", "?", "|", ">")):
        return True
    if stripped.startswith("$$"):
        return True
    remainder = _REFERENCE_SEGMENT_RE.sub("", _strip_shell_operators(stripped))
    return not _LITERAL_SEGMENT_RE.search(remainder)




def _literal_password(value: str) -> str | None:
    """The hard-coded password in a connection URL, or `None` if there is none.

    The whole userinfo is deliberately not the test. A URL like
    `postgres://omni:${OMNION_DB_PASSWORD}@db:5432/omnion` has a hard-coded USERNAME and
    no secret at all — it is the shape this repository itself uses, and it is correct,
    because a username is not a secret and an operator usually wants it pinned in the
    stack. What cannot be pinned is the password, so the userinfo is split on `:` and only
    the second half is examined.

    A component is accepted when it is empty (a deliberately passwordless connection) or
    composed entirely of references (`${VAR}`, `{{ … }}`, `$VAR`). Anything containing a
    bare alphanumeric character is a literal.
    """
    match = _VALUE_CREDENTIAL_RE.match(value)
    if not match:
        return None
    password = match.group(2)
    if not password:
        return None
    remainder = _REFERENCE_SEGMENT_RE.sub("", password)
    if _LITERAL_SEGMENT_RE.search(remainder):
        return password
    return None


def _read_document(source: str) -> str:
    """The text of `source`, which may already BE the text.

    A path is read; anything else is returned as-is. The second test — a string with no
    newline and no `:` — is what keeps a one-line YAML fragment passed as text from being
    treated as a filename and read (or, worse, silently not found). `os.path.exists` alone
    is not enough: a fragment like `POSTGRES_PASSWORD: hunter2` is not a path, but a
    fragment like `password` could plausibly be one.
    """
    if "\n" in source or ":" in source or " " in source:
        return source
    if os.path.isfile(source):
        with open(source, encoding="utf-8") as handle:
            return handle.read()
    return source


def _render_credentials(rendered: str, env: dict[str, str]) -> list[str]:
    """Credential values in a RENDERED compose document.

    A rendered document is not the same shape as a source one, and scanning it like one is
    how this check produced over a hundred findings on a perfectly clean render: compose
    emits `environment: {A=1, B=2}` in flow style, so the whole block is one line, the line
    scanner reads it as a single `key=value`, and the key it takes is whichever identifier
    appears first in the line. Every one of those findings was a real string from the file
    and none of it was a credential.

    So the render is scanned with a pair pattern that finds every `KEY=value` INSIDE the
    flow line, and a pair is reported only when its key names a credential AND its value is
    not one of the placeholders this run supplied. A value carrying URL userinfo is
    reported whatever the key is called, using the same password rule as the source scan.
    """
    allowed = set(env.values())
    found: list[str] = []
    for key, value in _RENDER_SCAN_RE.findall(rendered):
        if key in _RENDER_LITERAL_KEYS:
            continue
        stripped = value.strip().strip('"\'')
        if stripped in allowed:
            continue
        if _literal_password(stripped) is not None:
            found.append(f"{key}=<url with a literal password>")
            continue
        if CREDENTIAL_KEY_RE.search(key):
            found.append(f"{key}={stripped[:60]}")
    return found


def _literal_credentials(source: str, rendered: str | None = None, env: dict[str, str] | None = None) -> list[str]:
    """Credential keys in a stack file that carry a literal instead of a reference.

    `source` may be the file's TEXT or its PATH — accepting both, and deciding by looking,
    because the first version of this accepted only text and the second only a path, and
    the gate's own fixture passed a PATH and got `[]` for a file containing
    `POSTGRES_PASSWORD: hunter2`. A helper whose two call sites disagree about its argument
    type is a helper that reports on a document nobody read, and it reported a leak as
    clean. The discriminator is `os.path.exists` plus a newline-free, colon-free heuristic,
    so a one-line document passed as text is not mistaken for a filename.

    With `rendered`, the comparison is against what was actually interpolated: a value in
    the render that is not one of the supplied `env` values is a literal the SOURCE never
    showed — an `environment:` list, an `image:` tag with inline auth, or a value compose
    defaults. That is the direction a source-only scan cannot see, and it is the direction
    that ships a credential.
    """
    text = rendered if rendered is not None else _read_document(source)
    if rendered is not None:
        # A rendered document has a different shape and a different scanner. Routing it
        # through the source scanner is what produced the hundred-plus false positives.
        return _render_credentials(rendered, env or {})
    allowed = set((env or {}).values())
    found: list[str] = []
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        match = _ASSIGNMENT_RE.match(line)
        if not match:
            continue
        key, value = match.group(1), match.group(2)
        stripped = value.strip().strip('"\'')
        # The credential-in-the-value test is unconditional on the key, because
        # `DATABASE_URL: postgres://u:p@h/db` is a leak whatever the key is called.
        embedded = _literal_password(stripped)
        if embedded is not None:
            found.append(f"{key}=<url with a literal password>")
            continue
        if not CREDENTIAL_KEY_RE.search(key):
            continue
        # The question is not "is there a reference in here" — a value of `hunter2` has
        # none and a value of `${Y}-appended` has one, and both are wrong. The question is
        # whether the value is made of references and NOTHING ELSE, after compose's own
        # `:?message` / `:-default` arguments are removed. So:
        #
        #   `${Y}`                 pure  → clean
        #   `${Y}${Z}`             pure  → clean
        #   `${Y:?set Y in .env}`  pure  → clean (the sentence is the operator's argument)
        #   `${Y}-appended`       mixed → a literal is shipping beside the reference
        #   `hunter2`              bare  → a literal is shipping
        #
        # The first version asked `_looks_like_reference`, which is true for anything
        # containing a `${` — so `${Y}-appended` passed while `hunter2` failed, and the
        # check caught the case nobody has and missed the one a reviewer would. Asking "is
        # the remainder empty" is the question that has an answer either way.
        if not _is_pure_reference(value):
            found.append(f"{key}={value.strip()[:60]}")
    return found


def stage_availability(stage: dict[str, Any]) -> tuple[bool, str]:
    """Whether this stage's TOOL is usable here, and why not when it is not.

    Distinct from whether the stage's work can be done here: `syft` may be installed on a
    box with no image to describe. `run_stage` asks both questions, in that order, because
    the answers lead to different reports — "tool missing" is a builder problem, "tool
    present, work too heavy" is a policy decision, and conflating them produces a report
    that says the pipeline is broken on a machine where the pipeline is merely idle.
    """
    kind = stage.get("stage_kind", "")
    spec = STAGES.get(kind)
    if spec is None:
        return False, f"no stage specification for {kind!r}"
    return _probe_result(spec["probe"])


def run_stage(stage: dict[str, Any], root: str, workdir: str) -> dict[str, Any]:
    """Run one stage for real where this box can, and report honestly where it cannot.

    A stage that cannot run is reported `blocked` with the tool and the reason. It is never
    reported `skipped` and never given a digest: the manifest builder requires a digest for
    every artifact, and a dry run that supplied one would be fabricating a published
    checksum — the single most consequential thing a release pipeline can make up.
    """
    spec = STAGES.get(stage.get("stage_kind", ""), {})
    available, reason = stage_availability(stage)
    result: dict[str, Any] = {
        "id": stage["id"],
        "kind": stage["kind"],
        "name": stage["name"],
        "state": "blocked",
        "reason": reason,
        "digest": None,
        "content_digest": None,
        "size_bytes": None,
        "file": None,
    }
    if not available:
        result["reason"] = (
            f"`{' '.join(spec.get('probe', []))}` is unusable here: {reason}"
        )
        return result

    os.makedirs(workdir, exist_ok=True)
    if stage["id"] == "chart:omnion":
        destination = os.path.join(workdir, "chart")
        os.makedirs(destination, exist_ok=True)
        chart_dir = os.path.join(root, "infra", "helm", "omnion")
        completed = _run(["helm", "package", chart_dir, "-d", destination], root)
        if completed.returncode != 0:
            result["reason"] = f"helm package failed: {completed.stderr.strip()[:200]}"
            return result
        packaged = os.path.join(destination, stage["name"])
        if not os.path.exists(packaged):
            result["reason"] = f"helm package produced no {stage['name']}"
            return result
        content = canonical_archive_digest(packaged)
        # Repack and compare, because a build that cannot be reproduced cannot be audited
        # and a digest that changes on every run teaches an operator that the manifest is
        # noise. `helm package` stamps wall-clock mtimes into the archive, so the BYTES are
        # expected to differ — asserting that would be asserting a falsehood. The CONTENTS
        # are what must match, and this compares them.
        second = os.path.join(workdir, "chart-repack")
        os.makedirs(second, exist_ok=True)
        repeated = _run(["helm", "package", chart_dir, "-d", second], root)
        repacked = os.path.join(second, stage["name"])
        if repeated.returncode != 0 or not os.path.exists(repacked):
            result["reason"] = f"helm repack failed: {repeated.stderr.strip()[:200]}"
            return result
        repack_content = canonical_archive_digest(repacked)
        if repack_content != content:
            result["reason"] = (
                "the chart is not reproducible: two packs of the same tree produced "
                f"different content digests ({content[:19]}… vs {repack_content[:19]}…)"
            )
            return result
        result["state"] = "verified"
        result["file"] = packaged
        result["digest"] = _sha256_file(packaged)
        result["content_digest"] = content
        result["reproducible"] = True
        result["size_bytes"] = os.path.getsize(packaged)
        return result

    if stage["kind"] == "compose":
        env = dict(os.environ)
        env.update(PLACEHOLDER_ENV)
        compose_file = os.path.join(root, "infra", "compose", stage["name"])
        # TWO assertions about ONE render, in this order, and both can fail.
        #
        # The first version of this check looked for the placeholder string in the RENDER
        # and refused when it found one. The placeholders are what this function just
        # injected, so they are in the render BY CONSTRUCTION and the check was red for
        # every input — a rule that cannot pass is a rule that reports nothing, and it was
        # about to be committed as if it were a guarantee. Comparing the render against
        # the values the render was given proves nothing at all.
        #
        # What is worth proving is two-sided:
        #
        #   1. the SOURCE assigns no literal to a credential-shaped key — a hard-coded
        #      `POSTGRES_PASSWORD: hunter2` in a stack file is the defect, and it is
        #      visible in the file;
        #   2. every credential value in the RENDER is one of the placeholders this
        #      function supplied — which is positive proof that the key is wired to the
        #      variable rather than shadowed by a literal the source never shows, and it
        #      is the direction that can actually fail.
        literal = _literal_credentials(compose_file)
        if literal:
            result["reason"] = (
                f"{stage['name']} assigns a literal to a credential key: " + "; ".join(literal)
            )
            return result
        completed = _run(["docker", "compose", "-f", compose_file, "config"], root, env=env)
        if completed.returncode != 0:
            result["reason"] = f"compose config failed: {completed.stderr.strip()[:200]}"
            return result
        rendered = completed.stdout
        if not rendered.strip():
            result["reason"] = "compose config produced an empty document"
            return result
        interpolated = _literal_credentials(compose_file, rendered=rendered, env=PLACEHOLDER_ENV)
        if interpolated:
            result["reason"] = (
                "the rendered stack carries a credential value that is not one of the "
                "placeholders supplied to it: " + "; ".join(interpolated)
            )
            return result
        result["state"] = "verified"
        result["file"] = compose_file
        result["digest"] = _sha256_file(compose_file)
        result["size_bytes"] = os.path.getsize(compose_file)
        return result

    if stage["id"] == "manifest":
        facts_path = os.path.join(workdir, "facts.json")
        completed = _run(["python3", "release/lib/manifest.py", "build", facts_path, "--out",
                          os.path.join(workdir, stage["name"])], root)
        if completed.returncode != 0:
            result["reason"] = f"manifest build failed: {(completed.stderr or '').strip()[:200]}"
            return result
        path = os.path.join(workdir, stage["name"])
        result["state"] = "verified"
        result["file"] = path
        result["digest"] = _sha256_file(path)
        result["size_bytes"] = os.path.getsize(path)
        return result

    # image / cli / sbom: the tool is present (the probe passed) but the work is a real
    # build, which is a multi-GB, multi-minute affair this box cannot do. It is reported
    # blocked with THAT reason rather than attempted and half-finished — a half-built image
    # has no digest to record, and a file left in the workdir is a file the next run
    # mistakes for a cached artifact.
    result["reason"] = (
        f"`{' '.join(spec.get('command', []))}` is available but this is a real build, "
        "which this box cannot do; run the tag pipeline on a builder"
    )
    return result


def dry_run(root: str | None = None, version: str | None = None, workdir: str | None = None) -> dict[str, Any]:
    """Run the plan as far as this machine allows, and report the rest.

    Returns a facts document shaped for `manifest.build_manifest` **plus** the per-stage
    report. The digests present are real checksums of real files this run produced or
    verified; the digests absent are absent, and `publish` reads the absence as a refusal.
    """
    root = root or release_manifest.repo_root()
    version = version or release_manifest.workspace_version(root)
    workdir = workdir or os.path.join(root, ".release-dry-run")
    os.makedirs(workdir, exist_ok=True)

    stages = plan(root, version)
    # Two passes: the manifest stage consumes the digests of every other stage, so the
    # facts file has to exist before it runs. Ordering by dependency would do it, but the
    # manifest stage is a single well-known id and the skip is easier to read than a
    # topological sort of a list that is already in order.
    ordered = [s for s in stages if s["id"] != "manifest"] + [s for s in stages if s["id"] == "manifest"]
    results = [run_stage(stage, root, workdir) for stage in ordered]

    facts = _facts_document(version, root, results)
    facts_path = os.path.join(workdir, "facts.json")
    with open(facts_path, "w", encoding="utf-8") as handle:
        json.dump(facts, handle, indent=2, sort_keys=True)
        handle.write("\n")
    # The stage report is written as an artifact of the run rather than returned only.
    # It is the evidence for the run's claims, and a gate that has to RE-RUN the stages to
    # see it is a gate measuring a different run than the one it is checking — which is
    # how a run-to-run difference (a chart digest that carries a timestamp) gets reported
    # as a defect in the pipeline.
    report_path = os.path.join(workdir, "stages.json")
    with open(report_path, "w", encoding="utf-8") as handle:
        json.dump(results, handle, indent=2, sort_keys=True)
        handle.write("\n")

    blocked = [r["id"] for r in results if r["state"] != "verified"]
    return {
        "version": version,
        "source_commit": _safe_commit(root),
        "facts_path": facts_path,
        "report_path": report_path,
        "stages": results,
        "blocked": blocked,
        "complete": not blocked,
    }


def _safe_commit(root: str) -> str:
    try:
        return release_manifest.git_commit(root)
    except release_manifest.ManifestError:
        return "0" * 40


def _facts_document(version: str, root: str, results: list[dict[str, Any]]) -> dict[str, Any]:
    """Shape the stage results into the facts document the manifest builder consumes.

    Only `verified` stages contribute an entry. A blocked stage contributes nothing, so the
    builder's own refusal — "no digest supplied for X" — is what an operator sees, rather
    than a plausible number.
    """
    section_of = {"image": "images", "cli": "cli", "chart": "charts", "sbom": "sboms", "compose": "compose"}
    facts: dict[str, Any] = {section: {} for section in section_of.values()}
    for result in results:
        kind = result.get("kind")
        if not kind or result["state"] != "verified" or not result.get("digest"):
            continue
        section = section_of.get(kind)
        if section is None:
            continue
        name = result["name"]
        if kind == "image" and not name.startswith(release_manifest.DEFAULT_REGISTRY + "/"):
            # The builder rewrites the registry when the caller overrides it, so the facts
            # are keyed on the DEFAULT reference. Keying them on anything else is a
            # mismatch the builder reports as a missing digest.
            name = f"{release_manifest.DEFAULT_REGISTRY}/{name.rsplit('/', 1)[-1]}"
        entry: dict[str, Any] = {
            "digest": result["digest"],
            "size_bytes": result.get("size_bytes"),
            "stage": result["id"],
        }
        if result.get("content_digest"):
            # A second digest, and the two answer different questions. `digest` is the
            # bytes, which is what an operator verifies after downloading. `content_digest`
            # is the archive's contents with timestamps removed, which is what proves a
            # rebuild of the same chart produces the same thing. Publishing only the first
            # makes the chart's digest unreproducible by construction.
            entry["content_digest"] = result["content_digest"]
        facts[section][name] = entry
    facts["_synthetic"] = False
    facts["_source"] = FACTS_SOURCE
    facts["_version"] = version
    facts["_blocked_stages"] = [r["id"] for r in results if r["state"] != "verified"]
    del root  # the document describes the run, not the tree it ran in
    return facts


# ---------------------------------------------------------------------------------------------
# gates
# ---------------------------------------------------------------------------------------------


def run_gates(root: str | None = None, gates: list[str] | None = None) -> dict[str, Any]:
    """Run the required gates here, and report each one's real exit status.

    A gate with no declared command is `unrunnable`, never `passed`. The request says
    nothing publishes unless the migration verification is green; while REQ-129 has not
    built its runner there is no such command, and a gate list that silently drops the
    entries it cannot run is how "the gate was green" becomes a sentence with no subject.
    """
    root = root or release_manifest.repo_root()
    names = list(gates if gates is not None else REQUIRED_GATES)
    missing = [name for name in REQUIRED_GATES if name not in names]
    report: dict[str, Any] = {"gates": {}, "missing_required": missing}
    for name in names:
        commands = GATE_COMMANDS.get(name, [])
        available, reason = _probe_result(commands) if commands else (False, "no command is declared for this gate yet")
        if not commands:
            report["gates"][name] = {"state": "unrunnable", "reason": reason, "exit_code": None}
            continue
        if not available:
            report["gates"][name] = {"state": "unrunnable", "reason": reason, "exit_code": None}
            continue
        completed = _run(commands, root)
        report["gates"][name] = {
            "state": "green" if completed.returncode == 0 else "red",
            "reason": "" if completed.returncode == 0 else _last_line(completed),
            "exit_code": completed.returncode,
        }
    report["green"] = (
        not missing
        and bool(report["gates"])
        and all(g["state"] == "green" for g in report["gates"].values())
    )
    return report


# ---------------------------------------------------------------------------------------------
# publish refusal
# ---------------------------------------------------------------------------------------------

#: Why a publish is refused, in the order they are reported. Order is the operator's
#: reading order: what the tag says, what the gates say, what the build produced.
REFUSAL_ORDER = (
    "tag-version",
    "prerelease-on-stable",
    "gates",
    "missing-gates",
    "blocked-stages",
    "synthetic-facts",
    "attestation",
)


def publish_refusals(
    tag: str,
    version: str,
    gates_report: dict[str, Any],
    facts: dict[str, Any],
    *,
    attestation: bool,
    channel: str = "stable",
) -> list[str]:
    """Every reason this tag may not be published, as sentences.

    Returns ALL of them rather than the first. A pipeline that stops at the first refusal
    makes an operator fix one problem per run; and the second problem is usually the reason
    the first was introduced.
    """
    refusals: list[str] = []

    tag_version = tag[1:] if tag.startswith("v") else tag
    if tag_version != version:
        refusals.append(
            f"tag {tag!r} does not name the version being released ({version}); "
            "a tag is the release, so a mismatch publishes under a name the code denies"
        )

    # A pre-release may not reach the stable channel. The CHANNEL is a parameter because
    # the first version of this check hard-coded it to "stable" on both sides of the
    # comparison, so the branch was unreachable for every input — a check that cannot fail,
    # wearing the costume of one. A beta pipeline that publishes a `-rc` build as stable is
    # the exact mistake this is for, and it is only visible if the channel is the caller's
    # to state.
    if "-" in version and channel == "stable":
        refusals.append(
            f"pre-release {version} may not publish to the stable channel; "
            "publish it to a pre-release channel or drop the suffix"
        )

    if not gates_report.get("gates"):
        refusals.append("no gate was run")
    red = [name for name, g in gates_report.get("gates", {}).items() if g.get("state") == "red"]
    unrunnable = [
        name for name, g in gates_report.get("gates", {}).items() if g.get("state") == "unrunnable"
    ]
    if red:
        details = "; ".join(
            f"{name}: {gates_report['gates'][name].get('reason') or 'failed'}" for name in red
        )
        refusals.append(f"the CI gate is red, so nothing may be published — {details}")
    if unrunnable:
        refusals.append(
            "these required gates could not be run, which is not the same as passing: "
            + ", ".join(unrunnable)
        )
    for name in gates_report.get("missing_required", []):
        refusals.append(f"required gate {name!r} was not in the list at all")

    blocked = facts.get("_blocked_stages")
    if blocked:
        refusals.append(
            f"{len(blocked)} build stage(s) produced no artifact: " + ", ".join(blocked)
        )
    if facts.get("_synthetic"):
        refusals.append(
            "the build facts are synthetic; a synthetic digest must never reach a published manifest"
        )

    if not attestation:
        refusals.append(
            "no attestation: the images carry no signature or provenance statement to verify"
        )
    return refusals


# ---------------------------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------------------------


def _emit(document: Any) -> None:
    json.dump(document, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="pipeline.py", description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    plan_parser = sub.add_parser("plan", help="print the release plan for a version")
    plan_parser.add_argument("--version", default=None)
    plan_parser.add_argument("--json", action="store_true", help="machine-readable output")

    gates_parser = sub.add_parser("gates", help="run the required gates and report each")
    gates_parser.add_argument(
        "--only", action="append", default=None, help="run only these gates (default: all required)"
    )

    dry_parser = sub.add_parser("dry-run", help="run the plan as far as this machine allows")
    dry_parser.add_argument("--version", default=None)
    dry_parser.add_argument("--workdir", default=None)
    dry_parser.add_argument("--facts-out", default=None)

    publish_parser = sub.add_parser(
        "publish-check", help="report every reason a tag may not be published"
    )
    publish_parser.add_argument("--tag", required=True)
    publish_parser.add_argument("--version", default=None)
    publish_parser.add_argument("--facts", required=True)
    publish_parser.add_argument(
        "--attestation", action="store_true", help="assert the images were signed/provenanced"
    )
    publish_parser.add_argument(
        "--channel", default="stable", help="the channel this tag publishes to"
    )

    args = parser.parse_args(argv)
    root = release_manifest.repo_root()
    version = getattr(args, "version", None) or release_manifest.workspace_version(root)

    if args.command == "plan":
        stages = plan(root, version)
        if args.json:
            _emit(stages)
            return 0
        for stage in stages:
            available, reason = stage_availability(stage)
            state = "runnable" if available else f"BLOCKED — {reason}"
            print(f"{stage['id']:<44} {stage['kind'] or 'validate':<8} {state}")
        blocked = [s["id"] for s in stages if not stage_availability(s)[0]]
        print(f"\n{len(stages)} stages, {len(blocked)} blocked on this machine")
        return 0

    if args.command == "gates":
        report = run_gates(root, args.only)
        _emit(report)
        return 0 if report["green"] else 1

    if args.command == "dry-run":
        report = dry_run(root, version, args.workdir)
        for result in report["stages"]:
            mark = "ok  " if result["state"] == "verified" else "BLOCK"
            detail = result["digest"] or result["reason"]
            print(f"  {mark} {result['id']:<44} {detail}")
        print(f"\n{len(report['stages']) - len(report['blocked'])}/{len(report['stages'])} stages produced an artifact")
        if report["blocked"]:
            print(f"facts: {report['facts_path']} (incomplete — {len(report['blocked'])} stages have no digest)")
        else:
            print(f"facts: {report['facts_path']}")
        if args.facts_out:
            shutil.copyfile(report["facts_path"], args.facts_out)
        return 0

    if args.command == "publish-check":
        with open(args.facts, encoding="utf-8") as handle:
            facts = json.load(handle)
        gates_report = run_gates(root)
        refusals = publish_refusals(
            args.tag, version, gates_report, facts, attestation=args.attestation, channel=args.channel
        )
        _emit(
            {
                "tag": args.tag,
                "version": version,
                "publishable": not refusals,
                "refusals": refusals,
                "gates": gates_report["gates"],
            }
        )
        return 0 if not refusals else 1

    return 2


if __name__ == "__main__":
    sys.exit(main())
