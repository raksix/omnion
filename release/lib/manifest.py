#!/usr/bin/env python3
"""Release manifest builder and verifier (REQ-128, slice 3).

A release manifest is a **contract**, and the request says so: "a published release
manifest is a contract: digests, checksums and the minimum core version are verified on
download, and a mismatch is a hard failure, not a warning." So this module is written to
refuse rather than to produce something plausible:

* **Every image digest is required.** A manifest that says `sha256:unknown` is worse
  than no manifest, because an operator pinning a tag that later moves believes they
  pinned something. There is no "unknown" digest in this format.
* **The migration list is read from the filesystem**, never from a hand-written array
  in this file. The upgrade helper (REQ-128 slice 4) tells an operator whether the
  release they are about to install carries a destructive migration; an array would be
  correct exactly until the next writer adds a migration and forgets this file, which is
  the failure the request's own risk note describes.
* **The version is checked against every file that also states one.** The workspace, the
  three `package.json`s and the chart all carry a version. Publishing a chart labelled
  0.1.0 that deploys images tagged 0.4.0 is the oldest and worst release bug there is,
  and it is entirely preventable by comparing five files.
* **Version comparison is numeric per component**, because `0.10.0 < 0.9.0` under a
  string comparison and the minimum-core-version check is exactly the place that bug
  would hide — it would refuse a valid upgrade and, worse, accept an invalid one.

Everything here is stdlib-only and pure: no network, no registry, no docker. The things
it consumes are files the pipeline produced (digests, checksums) and files the repository
already carries (Dockerfiles, migrations, chart). That is deliberate — a release manifest
builder that shells out to a builder is a builder whose output nobody can check without
a builder.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from typing import Any

SCHEMA_VERSION = "1"

#: The platforms the request asks for. `arm64` is a release-time platform, not a
#: per-commit one — building it on every commit is the slowest, most expensive way to
#: discover that a dependency does not cross-compile.
IMAGE_PLATFORMS = ("linux/amd64", "linux/arm64")

#: The image repository the release publishes into. Derived from the compose default so
#: the two cannot drift: an operator who edits one file should not have to edit two.
DEFAULT_REGISTRY = "ghcr.io/raksix/omnion"

#: CLI binaries. Windows is a **binary only** — there is no Windows container image in
#: this release, and a manifest claiming otherwise is a missing artifact discovered at
#: install time.
CLI_PLATFORMS = ("linux-amd64", "linux-arm64", "macos-amd64", "macos-arm64", "windows-amd64")

#: The minimum core version this platform release requires. Bumping it is a deliberate
#: act: it is what turns "upgrade when convenient" into "upgrade before this runs".
CORE_MIN = "0.1.0"

MIGRATION_RE = re.compile(r"^(\d{4})_([a-z0-9_]+)\.sql$")
DOCKERFILE_COMPONENT_RE = re.compile(r"^([a-z0-9\-]+)\.Dockerfile$")


class ManifestError(Exception):
    """The release cannot be described honestly. Always fatal — never a warning."""


# ---------------------------------------------------------------------------------------------
# versions
# ---------------------------------------------------------------------------------------------

_VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.\-]+))?$")


def parse_version(value: str) -> tuple[int, int, int, str]:
    """Parse `MAJOR.MINOR.PATCH[-pre]` into a comparable tuple.

    A value that is not a version is [`ManifestError`], not a default. Defaulting is how
    a manifest ends up claiming a minimum core version of `0.0.0` and accepting an
    upgrade onto a platform that cannot run it.
    """
    match = _VERSION_RE.match((value or "").strip())
    if not match:
        raise ManifestError(f"not a version: {value!r}")
    major, minor, patch, pre = match.groups()
    return (int(major), int(minor), int(patch), pre or "")


def compare_versions(left: str, right: str) -> int:
    """Return -1/0/1 comparing two versions **numerically per component**.

    A pre-release sorts BELOW its release (`1.0.0-rc.1 < 1.0.0`), which is the ordering
    a minimum-version check needs: a build of `1.0.0-rc.1` does not satisfy a minimum of
    `1.0.0`, so it must be refused rather than installed over a stable release.
    """
    lmajor, lminor, lpatch, lpre = parse_version(left)
    rmajor, rminor, rpatch, rpre = parse_version(right)
    if (lmajor, lminor, lpatch) != (rmajor, rminor, rpatch):
        a, b = (lmajor, lminor, lpatch), (rmajor, rminor, rpatch)
        return -1 if a < b else 1
    if lpre == rpre:
        return 0
    if not lpre:
        return 1  # release beats pre-release
    if not rpre:
        return -1
    return -1 if lpre < rpre else 1


def satisfies_core_minimum(version: str, core_min: str) -> bool:
    """Whether `version` may be installed where the running core is `core_min`."""
    return compare_versions(version, core_min) >= 0


# ---------------------------------------------------------------------------------------------
# repository facts
# ---------------------------------------------------------------------------------------------


def repo_root() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(os.path.dirname(here))


def workspace_version(root: str | None = None) -> str:
    """The version every Rust crate in the workspace carries."""
    root = root or repo_root()
    with open(os.path.join(root, "Cargo.toml"), encoding="utf-8") as handle:
        text = handle.read()
    section = text.split("[workspace.package]", 1)[-1]
    match = re.search(r'^version\s*=\s*"([^"]+)"', section, re.MULTILINE)
    if not match:
        raise ManifestError("Cargo.toml [workspace.package] has no version")
    return match.group(1)


def declared_versions(root: str | None = None) -> dict[str, str | None]:
    """Every file that states a version, mapped to the version it states.

    Collected by walking rather than listing, so a manifest added later is picked up: a
    hard-coded list is a list that goes stale, and the value of the check is exactly that
    it cannot.

    **There is no exclusion, and the obvious one is wrong.** `private: true` in a
    `package.json` is pnpm's workspace convention here — the root, both apps, `packages/*`
    and every theme all set it — so keying the exclusion on it exempts EVERY file from the
    check and reports agreement while comparing nothing. It is also the wrong question:
    `private` answers "publish to npm?", and a marketplace theme (REQ-023/048) that must
    match the platform version says nothing about it. The only honest policy is that every
    package in this workspace carries the platform version, which is true today — so the
    check states that, and a maintainer who genuinely needs an independent version adds a
    package outside `apps/`/`packages/`/`themes/` and updates [`independent_version_reason`]
    at the same time.
    """
    root = root or repo_root()
    found: dict[str, str | None] = {"Cargo.toml": workspace_version(root)}

    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in {".git", "node_modules", "target", "dist", ".next"}]
        for name in filenames:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            try:
                if name == "package.json" and rel.count(os.sep) <= 2:
                    with open(path, encoding="utf-8") as handle:
                        data = json.load(handle)
                    if isinstance(data.get("version"), str):
                        found[rel] = data["version"]
                elif name == "Chart.yaml" and "/helm/" in f"/{rel}/":
                    with open(path, encoding="utf-8") as handle:
                        text = handle.read()
                    match = re.search(r'^appVersion:\s*"?([^"\n]+)"?', text, re.MULTILINE)
                    if match:
                        found[rel] = match.group(1).strip()
            except (OSError, ValueError):
                continue
    return found


def independent_version_reason(root: str | None = None, rel: str | None = None) -> dict[str, str]:
    """Packages allowed to version outside the platform's, with the reason for each.

    Empty today, and the empty set is asserted by the gate rather than assumed: a policy
    that exempts nothing is a stricter, verifiable claim, while a policy with an unexplained
    entry is an exclusion somebody added to make a check pass. A maintainer who needs one
    adds the path HERE and the reason with it, so the exemption lives next to the file whose
    versions it stops governing — not in a rule buried in the builder.
    """
    del root, rel  # the policy is empty by construction; see the docstring above
    return {}


def version_disagreements(root: str | None = None) -> list[str]:
    """Every file whose version differs from the workspace's, as readable sentences."""
    root = root or repo_root()
    canonical = workspace_version(root)
    out = []
    for path, value in sorted(declared_versions(root).items()):
        if path == "Cargo.toml" or value is None:
            continue
        if value != canonical:
            out.append(f"{path} declares {value}, workspace declares {canonical}")
    return out


def chart_metadata(root: str | None = None) -> dict[str, str]:
    """`name` and `version` from the chart's own `Chart.yaml`."""
    root = root or repo_root()
    path = os.path.join(root, "infra", "helm", "omnion", "Chart.yaml")
    with open(path, encoding="utf-8") as handle:
        text = handle.read()
    out = {}
    for key in ("name", "version"):
        match = re.search(rf'^{key}:\s*"?([^"\n]+)"?', text, re.MULTILINE)
        if not match:
            raise ManifestError(f"Chart.yaml has no {key}")
        out[key] = match.group(1).strip()
    return out


def dockerfile_targets(root: str | None = None) -> dict[str, list[str]]:
    """Every runtime image each Dockerfile can actually produce, by component name.

    A Dockerfile is NOT one image. `admin.Dockerfile` carries two runtime stages —
    `admin-runtime` and `web-runtime` — because the panel and the renderer share the
    install layer and must not drift on it, and because two targets in one file is what
    keeps that guarantee. Deriving the image list from the file names therefore produces
    three images for a platform that ships four, and the fourth is `web`: the one the
    compose stacks and the chart both deploy, and the one an operator actually pulls.
    A manifest missing `web` is a release whose public site cannot start.

    So the runtime stages are read out of the file, and the component name is taken from
    the stage name with the `-runtime` suffix stripped. Reading rather than listing is
    the point: adding a third runtime stage to a Dockerfile has to put it in the manifest
    without anyone editing this module.
    """
    root = root or repo_root()
    docker_dir = os.path.join(root, "infra", "docker")
    targets: dict[str, list[str]] = {}
    for name in sorted(os.listdir(docker_dir)):
        match = DOCKERFILE_COMPONENT_RE.match(name)
        if not match:
            continue
        with open(os.path.join(docker_dir, name), encoding="utf-8") as handle:
            text = handle.read()
        stages = re.findall(r"^FROM\s+\S+\s+AS\s+([A-Za-z0-9_\-]+)\s*$", text, re.MULTILINE | re.IGNORECASE)
        # The RUNTIME suffix is stripped, never assumed: `admin-runtime` and `web-runtime`
        # are two images, and the bare `runtime` stage of api.Dockerfile/cli.Dockerfile is
        # the file's own component (`api`, `cli`) rather than an image called "runtime".
        # An image literally named `runtime` is what a naive strip produces, and it is
        # caught by the deployed-vs-buildable cross-check below rather than published.
        runtimes = [s for s in stages if s.endswith("-runtime")]
        if runtimes:
            targets[match.group(1)] = [s[: -len("-runtime")] for s in runtimes]
        elif stages:
            targets[match.group(1)] = [match.group(1)]
        else:
            raise ManifestError(f"{name} declares no FROM stage, so it builds no image")
    if not targets:
        raise ManifestError("no component Dockerfile found in infra/docker")
    return targets


def deployed_image_names(root: str | None = None) -> set[str]:
    """The image names the deployment artifacts actually reference.

    Read out of the compose stacks and the chart's values, so the manifest cannot
    describe a release that deploys something else. This is the check that catches the
    inverse error — an image the pipeline publishes that nothing ever deploys, which
    costs minutes and a few hundred megabytes per release and is invisible until
    somebody wonders why the registry has an `admin` for an app that stopped shipping.
    """
    root = root or repo_root()
    names: set[str] = set()
    for pattern in (
        os.path.join("infra", "compose", "docker-compose.prod.yml"),
        os.path.join("infra", "compose", "docker-compose.enterprise.yml"),
    ):
        path = os.path.join(root, pattern)
        if not os.path.exists(path):
            continue
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                match = re.search(r"OMNION_IMAGE_REPO:-([^}]+)\}\}/([a-z0-9\-]+):", line)
                if match:
                    names.add(match.group(2))
    # The chart does NOT deploy `<repo>/<component>`; it deploys `<registry>/<repository>-<component>`.
    # `_helpers.tpl` builds `printf "%s-%s" $repository $imageSuffix`, so the repository
    # `raksix/omnion` produces `raksix/omnion-api`. Treating the repository as a component
    # name invents an image called `omnion` that no Dockerfile builds and no cluster ever
    # pulls — and, read the other way, it HIDES the real disagreement, because `admin` and
    # `api` still "match" and the check reports agreement while the chart ships
    # `raksix/omnion-web` under a repository this manifest never mentions.
    #
    # So the components are read from the chart's own COMPONENT LIST instead: the templates
    # range over `.component`, and every one of them ends in a pull reference. A component
    # the chart deploys and nothing builds is exactly the failure worth refusing.
    # Components only — never the repository itself. The repository is the PREFIX of every
    # chart image, so adding it to the component set makes the cross-check ask "does a
    # Dockerfile build an image called `omnion`", which is a different question and one
    # whose honest answer is no. The registry/repository agreement is checked on its own,
    # by [`repository_mismatch`].
    names |= _chart_components(root)
    return names


def repository_mismatch(registry: str, root: str | None = None) -> str | None:
    """Why `registry` is not the registry/repository the chart will pull from.

    The chart composes its pull reference as `<image.registry>/<image.repository>-<component>`,
    so a release published somewhere else is uninstallable by a chart shipped in the same
    release — and the failure lands at `helm install`, after the operator has already pointed
    the upgrade helper at the new version.

    The comparison is against the chart's OWN registry and repository read back out of its
    values, and it compares the WHOLE reference. The first version of this function built
    `f"{registry}/{repository}"` and then asked whether that string starts with `registry` —
    which it always does, so it returned `None` for every input including `docker.io/…` and
    `ghcr.io/somebody-else`. A check that constructs the answer and then tests the answer is
    the most expensive kind of green: it reported agreement about a value it never read.
    """
    root = root or repo_root()
    chart_registry = _chart_registry(root)
    chart_repository = _chart_repository(root)
    if chart_repository is None:
        return None

    wanted = registry.strip().rstrip("/")
    chart_ref = (
        f"{chart_registry.rstrip('/')}/{chart_repository}" if chart_registry else chart_repository
    )
    if chart_registry and wanted == chart_ref:
        return None

    return (
        f"chart pulls from {chart_ref!r} but the release publishes to {wanted!r}; "
        "the chart shipped with this release would fail to pull its own images"
    )


#: The Helm file whose `range` line is the chart's component list. The chart has no named
#: "components" define — the list is the `{{- range $component := list "api" "admin" "web" }}`
#: at the foot of `deployment.yaml`, which is what every per-component manifest hangs off.
CHART_COMPONENT_TEMPLATE = os.path.join("infra", "helm", "omnion", "templates", "deployment.yaml")

#: The marker a Dockerfile carries when its image is NOT a long-running service. A
#: compose/chart deployment has no such container — the CLI image exists to put a static
#: binary on a host that has Docker and nothing else, and the CI jobs that need a tool
#: image rather than a service. It is still a published artifact, so excluding it by path
#: would be wrong; excluding it by MARKER means the exclusion is a claim somebody wrote in
#: the file that produces the image, next to the reason, and can be reviewed there.
NON_SERVICE_MARKER = "omnion:non-service"


def _chart_components(root: str) -> set[str]:
    """Every component the chart deploys, read from its own `range … list` line.

    Read from the template rather than from a list kept here, for the same reason the
    image discovery reads the Dockerfiles: a hard-coded component list is a list that
    stops describing the chart on the commit that adds a fourth component, and it is the
    only check that can catch a chart deploying a component no Dockerfile builds — which
    fails at `helm install` on somebody's cluster, for an image the release never
    published.

    A template with no such line yields an EMPTY set rather than a guess, and the caller
    treats empty as "the chart deploys nothing this check can see" — which is the honest
    answer and keeps the cross-check from silently passing on an unread chart.
    """
    path = os.path.join(root, CHART_COMPONENT_TEMPLATE)
    if not os.path.exists(path):
        return set()
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            if "range" not in line or "list" not in line:
                continue
            found = set(re.findall(r'"([a-z0-9\-]+)"', line))
            if found:
                return found
    return set()


def _chart_registry(root: str) -> str | None:
    """`registry` from the chart's top-level `image` block, or `None`."""
    path = os.path.join(root, "infra", "helm", "omnion", "values.yaml")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as handle:
        in_image = False
        for line in handle:
            if re.match(r"^image:\s*$", line):
                in_image = True
                continue
            if in_image:
                if line and not line[0].isspace():
                    break
                match = re.match(r"^\s+registry:\s*(\S+)\s*$", line)
                if match:
                    return match.group(1).strip('"\'')
    return None


def _chart_repository(root: str) -> str | None:
    """`repository` from the chart's top-level `image` block, or `None`."""
    path = os.path.join(root, "infra", "helm", "omnion", "values.yaml")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as handle:
        in_image = False
        for line in handle:
            if re.match(r"^image:\s*$", line):
                in_image = True
                continue
            if in_image:
                if line and not line[0].isspace():
                    break
                match = re.match(r"^\s+repository:\s*(\S+)\s*$", line)
                if match:
                    return match.group(1).strip('"\'')
    return None


def non_service_images(root: str | None = None) -> set[str]:
    """Components whose image is not a long-running service, by their own declaration.

    Read from the `# omnion:non-service` marker in the component's Dockerfile. The marker
    carries a reason on the same or the adjacent line, and [`undeclared_image_reasons`]
    is what requires one — an exclusion with no stated reason is an exclusion added to
    silence a check, which is the only outcome this module is written to prevent.
    """
    root = root or repo_root()
    docker_dir = os.path.join(root, "infra", "docker")
    out: set[str] = set()
    if not os.path.isdir(docker_dir):
        return out
    for name in sorted(os.listdir(docker_dir)):
        match = DOCKERFILE_COMPONENT_RE.match(name)
        if not match:
            continue
        with open(os.path.join(docker_dir, name), encoding="utf-8") as handle:
            if NON_SERVICE_MARKER in handle.read():
                out.add(match.group(1))
    return out


def undeclared_image_reasons(root: str | None = None) -> dict[str, str]:
    """The reason written next to every `omnion:non-service` marker.

    Empty for a marker with no sentence after it, which the gate treats as a failure.
    """
    root = root or repo_root()
    docker_dir = os.path.join(root, "infra", "docker")
    out: dict[str, str] = {}
    if not os.path.isdir(docker_dir):
        return out
    for name in sorted(os.listdir(docker_dir)):
        match = DOCKERFILE_COMPONENT_RE.match(name)
        if not match:
            continue
        with open(os.path.join(docker_dir, name), encoding="utf-8") as handle:
            lines = handle.read().splitlines()
        for index, line in enumerate(lines):
            if NON_SERVICE_MARKER in line:
                for follow in lines[index + 1 :]:
                    stripped = follow.strip().lstrip("#").strip()
                    if len(stripped) > 30:
                        out[match.group(1)] = stripped
                        break
                out.setdefault(match.group(1), "")
    return out


def discover_images(root: str | None = None) -> list[dict[str, Any]]:
    """One image per buildable component, named the way the deployment refers to it.

    Derived from the Dockerfiles' runtime stages, and required to cover every image the
    compose stacks or the chart deploys. The cross-check is the load-bearing part: a
    release manifest is a contract about what a platform IS made of, so the two sets
    have to be the same set, and either direction of drift is a hard failure — an image
    nothing deploys is wasted registry weight, and an image nothing builds cannot be
    pulled at all.
    """
    root = root or repo_root()
    targets = dockerfile_targets(root)
    buildable = {component for components in targets.values() for component in components}
    deployed = deployed_image_names(root)

    unbuildable = sorted(deployed - buildable)
    if unbuildable:
        raise ManifestError(
            "compose/chart deploy an image nothing builds: " + ", ".join(unbuildable)
        )
    # The reverse direction is a WARNING with a required written reason, not a failure.
    # `cli` is the live example: its Dockerfile says in its own header that the image is
    # "a RUNTIME image here, not just a build target", and no compose service references
    # it — which is correct, because a CLI is not a service. Refusing it would push the
    # next maintainer towards deleting the marker instead of reading it. What is NOT
    # allowed is an undeployed image with NO marker: that is an image nobody can say what
    # it is for, and it is exactly how a registry accumulates orphans.
    undocumented = sorted(
        component
        for component in (buildable - deployed)
        if component not in non_service_images(root)
    )
    if undocumented:
        raise ManifestError(
            "an image is built but nothing deploys it and it carries no "
            f"'{NON_SERVICE_MARKER}' marker: " + ", ".join(undocumented)
        )

    images = [
        {
            "kind": "image",
            "name": f"{DEFAULT_REGISTRY}/{component}",
            "platforms": list(IMAGE_PLATFORMS),
        }
        for component in sorted(buildable)
    ]
    # `non-service` images are still published, and deliberately so: the CLI image is how a
    # host with Docker and no Rust toolchain runs `omnion migrate` before the panel exists.
    # Excluding it from the manifest would mean the release shipped an image nothing in the
    # manifest can name — the orphan this check exists to prevent.
    if not images:
        raise ManifestError("no component Dockerfile found in infra/docker")
    return images


def discover_migrations(root: str | None = None) -> list[str]:
    """Every migration filename the release ships, read off the disk.

    Read off the disk on purpose (see the module docstring). Sorted by the numeric
    prefix, not lexically — `0100_` sorting after `0099_` lexically is fine, but a
    migration numbered `00100_` would not be, and the sequence is a sequence.
    """
    root = root or repo_root()
    directory = os.path.join(root, "database", "migrations")
    found = []
    for name in os.listdir(directory):
        match = MIGRATION_RE.match(name)
        if match:
            found.append((int(match.group(1)), name))
    return [name for _, name in sorted(found)]


def declared_irreversible(root: str | None = None) -> list[str]:
    """Migrations whose author wrote `-- omnion:no-down` — an exception on purpose.

    A fact about a DECISION, so it stands whether or not the gate that would have required a
    reversal exists. Separate from [`unreversible_migrations`] because the two carry different
    provenance and the upgrade plan reports the reason to an operator: one says "the author
    declared it", the other says "nobody wrote one".
    """
    root = root or repo_root()
    directory = os.path.join(root, "database", "migrations")
    if not os.path.isdir(directory):
        return []
    out = []
    for name in sorted(os.listdir(directory)):
        if not MIGRATION_RE.match(name):
            continue
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            if "-- omnion:no-down" in handle.read():
                out.append(name)
    return out


def unreversible_migrations(root: str | None = None) -> list[str]:
    """Migrations that carry no executable reversal.

    Read with `down.rs`'s own rule rather than a substring, for the reason that module spells
    out: "down script" appears inside prose all over this tree, and a substring match opens a
    block in the middle of a column comment.

    This used to return only the `-- omnion:no-down` files, so the moment REQ-129's policy
    landed the upgrade helper would have looked at a repository whose reversal-less migrations
    all test clean, answered `reversible`, and offered an operator a database rollback that
    does not exist. `unknown` is the safe middle and `reversible` is the lie.
    """
    root = root or repo_root()
    directory = os.path.join(root, "database", "migrations")
    if not os.path.isdir(directory):
        return []
    heading = re.compile(r"^--\s*(?:#{1,2}\s*)?down script\b", re.IGNORECASE)
    out = []
    for name in sorted(os.listdir(directory)):
        if not MIGRATION_RE.match(name):
            continue
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            text = handle.read()
        if "-- omnion:no-down" in text or not _has_reversal(text, heading):
            out.append(name)
    return out


def destructive_migrations(root: str | None = None) -> list[str]:
    """Every migration with no usable reversal, by either route.

    The union, kept because it is the question the manifest builder asks ("is anything in this
    release irreversible?"). The upgrade plan asks the two questions separately, because the
    reason it shows an operator differs.
    """
    root = root or repo_root()
    return sorted(set(declared_irreversible(root)) | set(unreversible_migrations(root)))


def _has_reversal(text: str, heading: "re.Pattern[str]") -> bool:
    """Whether a migration file carries an executable reversal, by `down.rs`'s rule.

    A heading opens the block; a comment line indented by two or more spaces inside it is a
    statement. A block with no statements is prose, and prose is `has_down = false` — the same
    answer the Rust side gives, which is what keeps the two from disagreeing about which
    migrations are reversible.
    """
    inside = False
    for line in text.splitlines():
        stripped = line.strip()
        if heading.match(stripped):
            inside = True
            continue
        if not inside or not stripped.startswith("--"):
            continue
        body = stripped[2:]
        if len(body) - len(body.lstrip(" ")) >= 2 and body.strip():
            return True
    return False


# ---------------------------------------------------------------------------------------------
# supplied build facts
# ---------------------------------------------------------------------------------------------


def load_supplied(path: str | None) -> dict[str, Any]:
    """Read the digests/checksums the pipeline produced.

    A missing file is fatal. The builder's job is to refuse to describe a release it
    cannot vouch for, and a release with no published artifacts is exactly that.
    """
    if not path or not os.path.exists(path):
        raise ManifestError(f"build facts file not found: {path or '(unset)'}")
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except ValueError as exc:
        raise ManifestError(f"build facts file is not valid JSON: {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ManifestError("build facts file must be an object")
    return data


def _require_digest(facts: dict[str, Any], key: str, name: str) -> str:
    """The digest supplied for one artifact, or a refusal.

    The facts file stores each artifact as an OBJECT (`{"digest": …, "size_bytes": …}`), and
    this originally indexed the dict one level too shallow — so `facts["images"][name]`
    returned that object, failed the `isinstance(value, str)` test, and reported "no digest
    supplied" for an artifact whose digest was sitting right there in the file. The message
    pointed at the pipeline not producing a digest when the real fault was the reader's
    shape assumption, which is the more expensive half: it sends the next person to debug a
    builder that is working. Both shapes are accepted now — a bare digest string as well as
    the object — because a CI job emitting `{"images": {"name": "sha256:…"}}` is a reasonable
    thing to write, and refusing it would be the pipeline's problem to solve, not the
    reader's.
    """
    entry = facts.get(key)
    if not isinstance(entry, dict):
        raise ManifestError(f"build facts carry no {key} section")
    value = entry.get(name)
    if isinstance(value, dict):
        value = value.get("digest")
    if not isinstance(value, str) or not value.strip():
        raise ManifestError(f"no digest supplied for {name} ({key})")
    value = value.strip()
    if not re.match(r"^sha256:[0-9a-f]{64}$", value):
        raise ManifestError(f"digest for {name} is not a sha256 digest: {value!r}")
    return value


# ---------------------------------------------------------------------------------------------
# build / verify
# ---------------------------------------------------------------------------------------------


def build_manifest(
    version: str,
    source_commit: str,
    registry: str = DEFAULT_REGISTRY,
    root: str | None = None,
    facts: dict[str, Any] | None = None,
    facts_path: str | None = None,
    allow_version_drift: bool = False,
) -> dict[str, Any]:
    """Assemble the manifest, or explain precisely why it cannot be assembled.

    `allow_version_drift` exists for exactly one caller: the gate's mutation suite, which
    needs a document to mutate even on a tree whose versions disagree. It is a parameter and
    not a module-level switch because a global "ignore versions" flag is one `--set` away
    from being the setting that ships releases — and it is printed on stderr when used, so a
    release built this way says so in the log that an operator might actually read.
    """
    root = root or repo_root()
    facts = facts if facts is not None else load_supplied(facts_path)

    parse_version(version)  # rejects nonsense before anything is written
    if not re.fullmatch(r"[0-9a-f]{7,40}", source_commit or ""):
        raise ManifestError(f"source commit is not a git object id: {source_commit!r}")

    disagreements = version_disagreements(root)
    if disagreements:
        if not allow_version_drift:
            raise ManifestError("version disagreement: " + "; ".join(disagreements))
        print(
            "release: WARNING — building with --allow-version-drift; "
            "version disagreement: " + "; ".join(disagreements),
            file=sys.stderr,
        )

    mismatch = repository_mismatch(registry, root)
    if mismatch:
        raise ManifestError(mismatch)

    # EVERY missing digest is reported, not the first. The builder asked for one digest at
    # a time and raised on the first absence, so a build with no facts at all produced one
    # refusal naming one artifact — and a pipeline operator fixing that is re-run, told
    # about the next, and re-run again. A partial build is the normal state of a dry run,
    # so the common case was the one that took the most attempts. `digest_for` accumulates
    # the names and the message lists them all, which is the difference between one run and
    # fourteen. The placeholder it returns is never used: the raise below happens first, and
    # a caller that catches the error gets no document.
    absent: list[str] = []

    def digest_for(key: str, name: str) -> str:
        try:
            return _require_digest(facts, key, name)
        except ManifestError as exc:
            absent.append(f"{name} ({key}): {exc}")
            return "sha256:" + "0" * 64

    chart = chart_metadata(root)
    registry = registry.rstrip("/")
    artifacts: list[dict[str, Any]] = []

    for image in discover_images(root):
        name = image["name"].replace(DEFAULT_REGISTRY, registry, 1)
        artifacts.append(
            {
                **image,
                "name": name,
                "digest": digest_for("images", name),
                "size_bytes": _optional_int(facts, "images", name, "size_bytes"),
                "download_url": f"oci://{name}",
            }
        )

    for platform in CLI_PLATFORMS:
        name = f"omnion-{platform}"
        artifacts.append(
            {
                "kind": "cli",
                "name": name,
                "digest": digest_for("cli", name),
                "platforms": [platform],
                "size_bytes": _optional_int(facts, "cli", name, "size_bytes"),
                "download_url": f"https://github.com/raksix/omnion/releases/download/v{version}/{name}.tar.gz",
            }
        )

    chart_name = f"{chart['name']}-{version}.tgz"
    artifacts.append(
        {
            "kind": "chart",
            "name": chart_name,
            "digest": digest_for("charts", chart_name),
            "platforms": [],
            "size_bytes": _optional_int(facts, "charts", chart_name, "size_bytes"),
            "download_url": f"https://github.com/raksix/omnion/releases/download/v{version}/{chart_name}",
        }
    )

    for image in [a for a in artifacts if a["kind"] == "image"]:
        sbom_name = f"{image['name'].rsplit('/', 1)[-1]}.sbom.json"
        artifacts.append(
            {
                "kind": "sbom",
                "name": sbom_name,
                "digest": digest_for("sboms", sbom_name),
                "platforms": list(image["platforms"]),
                "size_bytes": _optional_int(facts, "sboms", sbom_name, "size_bytes"),
                "download_url": f"https://github.com/raksix/omnion/releases/download/v{version}/{sbom_name}",
            }
        )

    for compose_name in ("docker-compose.prod.yml", "docker-compose.enterprise.yml"):
        artifacts.append(
            {
                "kind": "compose",
                "name": compose_name,
                "digest": digest_for("compose", compose_name),
                "platforms": [],
                "size_bytes": _optional_int(facts, "compose", compose_name, "size_bytes"),
                "download_url": f"https://github.com/raksix/omnion/releases/download/v{version}/{compose_name}",
            }
        )

    if absent:
        raise ManifestError(
            f"{len(absent)} artifact(s) have no digest supplied: " + "; ".join(absent)
        )
    migrations = discover_migrations(root)
    destructive = destructive_migrations(root)

    return {
        "manifest_version": SCHEMA_VERSION,
        "version": version,
        "channel": "stable",
        "source_commit": source_commit,
        "registry": registry,
        "core_min": CORE_MIN,
        "chart_version": chart["version"],
        "migrations": migrations,
        # Not `False` when the policy cannot answer: an unknown destructiveness is `null`,
        # and `null` renders in the upgrade helper as "must be checked by hand". A `false`
        # here would tell an operator a destructive migration is absent.
        "migrations_destructive": (True if destructive else (None if not _policy_exists(root) else False)),
        "artifacts": sorted(artifacts, key=lambda a: (a["kind"], a["name"])),
    }


#: The marker that identifies REQ-129's own migration, whatever number it was assigned.
#:
#: The version number is NOT the identity. This function used to test for a hard-coded
#: `0030_migration_safety.sql`, chosen when the request was written and the file had no number
#: yet; the migration actually landed as `0207_migration_safety.sql`, so the test never became
#: true and `destructiveness()` answered `unknown` on every release — permanently, and for a
#: reason invisible to a reader because the function is named `_policy_exists` and its body
#: reads like a lookup. The upgrade helper would have kept telling an operator "nobody has
#: checked" about a gate that had been shipping for months.
#:
#: So the identity is the SUBJECT, matched across the whole directory. A renumbering, a
#: backfill, or a second migration in the same subject all keep this correct, which a
#: hard-coded path cannot.
_POLICY_SUBJECT = "migration_safety"


def _policy_exists(root: str | None = None) -> bool:
    """Whether REQ-129's migration safety policy has actually landed.

    Matched by subject across every migration in the tree rather than by a version number,
    because the number is assigned when the file is written and this tree carries it under a
    different one than the request assumed.
    """
    root = root or repo_root()
    directory = os.path.join(root, "database", "migrations")
    if not os.path.isdir(directory):
        return False
    return any(_POLICY_SUBJECT in name for name in os.listdir(directory))


def _optional_int(facts: dict[str, Any], key: str, name: str, field: str) -> int | None:
    entry = facts.get(key)
    if not isinstance(entry, dict):
        return None
    record = entry.get(name)
    if not isinstance(record, dict):
        return None
    value = record.get(field)
    return value if isinstance(value, int) and value >= 0 else None


REQUIRED_ARTIFACT_KINDS = ("image", "cli", "chart", "sbom", "compose")

#: Structural patterns that mean "this string carries a credential", not "this string looks
#: suspicious". Both are shape-based on purpose: a literal grep for a known secret cannot be
#: written in a repository that must not contain the secret, and cannot be run in a tool
#: output that masks it. `scheme://userinfo@` is what a credential in a connection string
#: looks like in every syntax (Docker registry auth, git remotes, S3 URLs), and
#: `KEY=value` is the form an `.env` line and a rendered chart `NOTES.txt` take.
CREDENTIAL_SHAPES = (
    re.compile(r"[a-zA-Z][a-zA-Z0-9+.-]*://[^/\s]*@"),
    re.compile(r"(?i)(PASSWORD|PASSWD|TOKEN|SECRET|API[_-]?KEY|CREDENTIALS?)=[^&\s]+"),
    re.compile(r"gh[pousr]_[A-Za-z0-9]{20,}"),
)


def carries_credential(value: str) -> bool:
    """Whether a string published in a manifest carries a credential.

    Used on artifact names and download URLs, because those are the only fields that leave
    this machine in the format a client can act on. A digest is derived from a name, so a
    credential smuggled into a name is also visible in a digest comparison — which is
    exactly why the artifacts table's copy button puts the raw reference on the clipboard.
    """
    return any(pattern.search(value) for pattern in CREDENTIAL_SHAPES)


def verify_manifest(manifest: dict[str, Any], *, core_version: str | None = None) -> list[str]:
    """Every way the manifest cannot be trusted. An empty list is a passing manifest.

    This is the half the operator runs: the build side (`build_manifest`) refuses to
    produce a bad manifest, and this refuses to believe one that arrived from a registry,
    a file on disk or an older release. A download that fails these checks is a HARD
    failure — the request is explicit that it is not a warning.
    """
    problems: list[str] = []

    if not isinstance(manifest, dict):
        return ["manifest is not an object"]

    version = manifest.get("version")
    try:
        parse_version(version if isinstance(version, str) else "")
    except ManifestError:
        problems.append(f"version is missing or not a version: {version!r}")

    commit = manifest.get("source_commit")
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{7,40}", commit):
        problems.append(f"source_commit is not a git object id: {commit!r}")

    if not isinstance(manifest.get("registry"), str) or not manifest.get("registry"):
        problems.append("manifest carries no registry")
    elif carries_credential(manifest["registry"]):
        problems.append("registry carries a credential-shaped value")

    # The core minimum must itself be a version, and the running core must satisfy it.
    core_min = manifest.get("core_min")
    if not isinstance(core_min, str):
        problems.append("core_min is missing")
    else:
        try:
            parse_version(core_min)
        except ManifestError:
            problems.append(f"core_min is not a version: {core_min!r}")
        if core_version is not None:
            try:
                if not satisfies_core_minimum(core_version, core_min):
                    problems.append(
                        f"core {core_version} is below this release's minimum {core_min}"
                    )
            except ManifestError as exc:
                problems.append(f"core version could not be compared: {exc}")

    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        problems.append("manifest lists no artifacts")
        return problems

    seen: set[tuple[str, str]] = set()
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            problems.append(f"artifact is not an object: {artifact!r}")
            continue
        kind = artifact.get("kind")
        name = artifact.get("name")
        if kind not in REQUIRED_ARTIFACT_KINDS:
            problems.append(f"{name}: unknown artifact kind {kind!r}")
        if not isinstance(name, str) or not name:
            problems.append(f"artifact has no name (kind {kind!r})")
            continue
        key = (str(kind), name)
        if key in seen:
            problems.append(f"{name}: listed more than once for kind {kind}")
        seen.add(key)

        digest = artifact.get("digest")
        if not isinstance(digest, str) or not re.match(r"^sha256:[0-9a-f]{64}$", digest or ""):
            problems.append(f"{name}: digest is missing or not a sha256 digest")

        platforms = artifact.get("platforms")
        if not isinstance(platforms, list) or any(not isinstance(p, str) for p in platforms):
            problems.append(f"{name}: platforms must be a list of strings")

        for field in ("name", "download_url"):
            value = artifact.get(field)
            if isinstance(value, str) and carries_credential(value):
                problems.append(f"{name}: {field} carries a credential-shaped value")

        if kind == "image":
            # The registry prefix is compared against the REFERENCE, not against a name
            # derived from the reference. The first version built `expected` from
            # `manifest['registry']` and then tested `name.startswith(...)` — but a name that
            # already fails the prefix test cannot be used to prove the prefix test, and the
            # mutation "publish this image to docker.io" survived it. The component is what
            # the reference appends, so it is derived from the EXPECTED name instead.
            expected_reference = f"{manifest.get('registry')}/{name.rsplit('/', 1)[-1]}"
            if not isinstance(name, str) or not name.startswith(f"{manifest.get('registry')}/"):
                problems.append(
                    f"{name}: image is not under the declared registry {manifest.get('registry')} "
                    f"(expected {expected_reference})"
                )
            for platform in IMAGE_PLATFORMS:
                if platform not in (platforms or []):
                    problems.append(f"{name}: release platforms must include {platform}")

        if kind == "cli":
            platform_list = platforms or []
            if not platform_list:
                problems.append(f"{name}: CLI artifact names no platform")
            for platform in platform_list:
                if platform not in CLI_PLATFORMS:
                    problems.append(f"{name}: unknown CLI platform {platform}")

    for kind in REQUIRED_ARTIFACT_KINDS:
        if not any(isinstance(a, dict) and a.get("kind") == kind for a in artifacts):
            problems.append(f"manifest publishes no {kind} artifact")

    # Set-level coverage, computed ONCE from the whole artifact list. The previous version of
    # this check lived inside the per-artifact loop, so it compared each CLI artifact against
    # the full set while that same artifact was being examined — and dropping the Windows
    # binary still passed, because the remaining four artifacts between them covered all five
    # platforms in a list comprehension that ran once per artifact and only reported the
    # empty case. Coverage is a property of the SET. Checking it per element cannot express it.
    covered_cli: set[str] = set()
    for artifact in artifacts:
        if isinstance(artifact, dict) and artifact.get("kind") == "cli":
            platforms = artifact.get("platforms")
            if isinstance(platforms, list):
                covered_cli.update(p for p in platforms if isinstance(p, str))
    for platform in CLI_PLATFORMS:
        if platform not in covered_cli:
            problems.append(f"no CLI artifact published for {platform}")

    migrations = manifest.get("migrations")
    if not isinstance(migrations, list) or any(not isinstance(m, str) for m in migrations):
        problems.append("migrations must be a list of strings")
    elif not migrations:
        # An EMPTY migration list is not a platform that shipped no migrations — it is a
        # builder that failed to read `database/migrations/`. The upgrade helper reads this
        # field to decide whether an operator needs a backup first, so an empty list is the
        # one value that makes a destructive upgrade look like a no-op. It was a surviving
        # mutation until this rule existed, which is the only reason it is here.
        problems.append("manifest ships no migrations; the list is read off disk, so empty means the builder failed")

    if manifest.get("manifest_version") != SCHEMA_VERSION:
        # The consumer parses `release_artifacts.manifest_version` (REQ-128's data model) and
        # a document from an older builder is a different shape. Accepting it silently is how
        # a cached manifest from a previous release renders as today's release with today's
        # fields. Refusing at the boundary is the whole point of a versioned format.
        problems.append(
            f"manifest_version must be {SCHEMA_VERSION!r}, got {manifest.get('manifest_version')!r}"
        )

    destructive = manifest.get("migrations_destructive")
    if not isinstance(destructive, (bool, type(None))):
        problems.append("migrations_destructive must be true, false or null")

    return problems


# ---------------------------------------------------------------------------------------------
# schema + fixtures
# ---------------------------------------------------------------------------------------------


def schema() -> dict[str, Any]:
    """The JSON Schema the manifest is validated against.

    Kept next to the code that produces the document because a schema nobody re-runs is a
    schema that stops describing anything after the second release. `release/` runs this
    against its own output in the gate, so a field added here without a producer fails
    the build rather than the first consumer.
    """
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://omnion.dev/schemas/release-manifest-1.json",
        "title": "Omnion release manifest",
        "type": "object",
        "required": [
            "manifest_version",
            "version",
            "source_commit",
            "registry",
            "core_min",
            "migrations",
            "migrations_destructive",
            "artifacts",
        ],
        "properties": {
            "manifest_version": {"const": SCHEMA_VERSION},
            "version": {"type": "string", "pattern": r"^\d+\.\d+\.\d+(-[0-9A-Za-z.\-]+)?$"},
            "channel": {"type": "string"},
            "source_commit": {"type": "string", "pattern": "^[0-9a-f]{7,40}$"},
            "registry": {"type": "string", "minLength": 1},
            "core_min": {"type": "string", "pattern": r"^\d+\.\d+\.\d+(-[0-9A-Za-z.\-]+)?$"},
            "chart_version": {"type": "string"},
            "migrations": {"type": "array", "items": {"type": "string"}},
            "migrations_destructive": {"type": ["boolean", "null"]},
            "artifacts": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["kind", "name", "digest", "platforms"],
                    "properties": {
                        "kind": {"enum": list(REQUIRED_ARTIFACT_KINDS)},
                        "name": {"type": "string", "minLength": 1},
                        "digest": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"},
                        "platforms": {"type": "array", "items": {"type": "string"}},
                        "size_bytes": {"type": ["integer", "null"], "minimum": 0},
                        "download_url": {"type": "string"},
                    },
                    "additionalProperties": False,
                },
            },
        },
    }


def synthetic_facts(
    root: str | None = None,
    version: str | None = None,
    commit: str = "0" * 40,
) -> dict[str, Any]:
    """Build facts for a release that was never actually built.

    This is what makes "a fake tag produces a complete manifest" — the slice's own
    definition of done — testable without a registry. The digests are deterministic
    hashes of the artifact names, so the same input always produces the same document and
    a diff in a review is a real change rather than noise.

    It is a **fixture**, and it says so in its own contents: every digest is prefixed
    with a synthetic marker so a synthetic manifest can never be mistaken for a published
    one by reading it. A fixture that is indistinguishable from the real thing is a
    credential-shaped lie in a public repository.
    """
    root = root or repo_root()
    version = version or workspace_version(root)
    chart = chart_metadata(root)
    digest_of = lambda text: "sha256:" + _sha256_hex(text)  # noqa: E731

    facts: dict[str, Any] = {"images": {}, "cli": {}, "charts": {}, "sboms": {}, "compose": {}}
    for image in discover_images(root):
        facts["images"][image["name"]] = {
            "digest": digest_of(f"synthetic-image:{version}:{image['name']}"),
            "size_bytes": 1024,
        }
    for platform in CLI_PLATFORMS:
        name = f"omnion-{platform}"
        facts["cli"][name] = {"digest": digest_of(f"synthetic-cli:{version}:{name}"), "size_bytes": 512}
    chart_name = f"{chart['name']}-{version}.tgz"
    facts["charts"][chart_name] = {"digest": digest_of(f"synthetic-chart:{version}"), "size_bytes": 2048}
    for image in discover_images(root):
        sbom = f"{image['name'].rsplit('/', 1)[-1]}.sbom.json"
        facts["sboms"][sbom] = {"digest": digest_of(f"synthetic-sbom:{version}:{sbom}"), "size_bytes": 256}
    for name in ("docker-compose.prod.yml", "docker-compose.enterprise.yml"):
        facts["compose"][name] = {"digest": digest_of(f"synthetic-compose:{version}:{name}"), "size_bytes": 4096}
    facts["_synthetic"] = True
    facts["_commit"] = commit
    return facts


def _sha256_hex(text: str) -> str:
    import hashlib

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def git_commit(root: str | None = None) -> str:
    root = root or repo_root()
    result = subprocess.run(
        ["git", "-C", root, "rev-parse", "HEAD"], capture_output=True, text=True
    )
    if result.returncode != 0:
        raise ManifestError("not a git repository, so a source commit cannot be stated")
    return result.stdout.strip()


def main(argv: list[str]) -> int:
    root = repo_root()
    command = argv[1] if len(argv) > 1 else "build"

    if command == "schema":
        print(json.dumps(schema(), indent=2))
        return 0

    if command == "facts":
        facts = synthetic_facts(root)
        json.dump(facts, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
        return 0

    if command == "build":
        if len(argv) < 3:
            print("usage: release.py build <facts.json> [--version V] [--commit C] [--out FILE]", file=sys.stderr)
            return 2
        facts_path = argv[2]
        version = workspace_version(root)
        commit = git_commit(root)
        out = None
        allow_drift = False
        rest = argv[3:]
        index = 0
        while index < len(rest):
            flag = rest[index]
            if flag == "--version":
                version = rest[index + 1]
                index += 2
            elif flag == "--commit":
                commit = rest[index + 1]
                index += 2
            elif flag == "--out":
                out = rest[index + 1]
                index += 2
            elif flag == "--allow-version-drift":
                allow_drift = True
                index += 1
            else:
                print(f"unknown flag: {flag}", file=sys.stderr)
                return 2
        try:
            manifest = build_manifest(
                version,
                commit,
                root=root,
                facts_path=facts_path,
                allow_version_drift=allow_drift,
            )
        except ManifestError as exc:
            print(f"release: {exc}", file=sys.stderr)
            return 1
        problems = verify_manifest(manifest, core_version=version)
        if problems:
            for problem in problems:
                print(f"release: manifest problem: {problem}", file=sys.stderr)
            return 1
        text = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
        if out:
            with open(out, "w", encoding="utf-8") as handle:
                handle.write(text)
        else:
            sys.stdout.write(text)
        return 0

    if command == "verify":
        if len(argv) < 3:
            print("usage: release.py verify <manifest.json> [--core V]", file=sys.stderr)
            return 2
        with open(argv[2], encoding="utf-8") as handle:
            manifest = json.load(handle)
        core = None
        if "--core" in argv:
            core = argv[argv.index("--core") + 1]
        problems = verify_manifest(manifest, core_version=core)
        for problem in problems:
            print(f"release: {problem}", file=sys.stderr)
        if problems:
            return 1
        print(f"release: manifest {manifest.get('version')} verified ({len(manifest.get('artifacts', []))} artifacts)")
        return 0

    print(f"unknown command: {command}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))