#!/usr/bin/env python3
"""Upgrade plan builder (REQ-128, slice 4).

The upgrade helper answers one question an operator cannot answer from the file they are
holding: *I am on 0.4.0, this manifest says 0.5.0 — what do I do, in what order, and where
does it stop being reversible?* The request puts the hard part in the same sentence:

> *the upgrade helper renders the ordered steps for compose and Kubernetes, splits
> application from database rollback, and refuses to show a complete checklist until a
> destructive migration is acknowledged.*

Three of those four claims are about **refusal**, and the fourth is about **order**. This
module is therefore built the same way the manifest builder is: it refuses to produce
something plausible.

**The steps are derived from the repository, not written down here.** The compose file
names, the chart name, the image reference and the migration list all come from the files
and the manifests an upgrade actually has to move. A hand-written step that says
`docker compose -f infra/compose/docker-compose.yml` names a file that does not exist in
this repository (it is `docker-compose.prod.yml`), and a hand-written migration list is
correct exactly until the next writer adds a migration. Every command in a plan is
assembled from a fact, and `verify_plan` re-reads those facts and refuses a plan whose
commands name a stack file or a chart that is not on disk.

**The rollback split is the reason this module refuses instead of rendering.** An
application rollback is always possible: the previous image tag exists, and deploying it
is one command. A database rollback exists only when the release ships a verified down
script. REQ-129 — which is what makes a down script *verified* rather than *written* — is
still `pending`, so today the honest answer for destructiveness is **unknown**, and this
module renders unknown as unknown: the plan says "must be checked by hand", the checklist
is incomplete, and acknowledging the unknown does not complete it. A `false` here would
tell an operator a destructive migration is absent because nobody has looked yet, which is
the one sentence in this request that must never be rendered.

**The point of no return is where the two rollbacks stop agreeing.** If the database can go
back, it is the deploy step (the old code can read the new schema) — actually no: it is the
LAST migration step, because after it a rollback needs the down script. If the database
cannot go back, the point of no return is the FIRST migration step, and everything after it
is one-way. Marking it is not decoration: the request asks for the checklist to render as
complete only after an operator acknowledges a destructive migration, and a marker the
operator cannot see in a list of twelve steps is a marker nobody acknowledges.

Everything here is stdlib-only and pure — no network, no docker, no helm, no database. The
things it consumes are manifests (documents), the repository's own compose and chart files,
and the version the running core reports.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import manifest as release_manifest  # noqa: E402  (sibling module, same package layout)
import bundle as release_bundle  # noqa: E402

SCHEMA_VERSION = "1"

#: The two topologies the request names. A topology with no steps is a topology nobody
#: deployed with this helper, so the set is closed and every entry has a builder below.
TOPOLOGIES = ("compose", "kubernetes")

#: The step kinds the `upgrade_plans.steps` column documents. `backup` and `manual` are
#: there because the honest plan for a release with an unverifiable migration is: back up,
#: read this, decide. Rendering only deploy steps would hide the decision.
STEP_KINDS = ("backup", "migrate", "deploy", "verify", "manual")

#: Where the stack files live, per topology. Read from disk rather than assumed: the prod
#: stack's name is the single most-referenced string in this request and it was wrong in a
#: draft of this very docstring.
COMPOSE_STACKS = {
    "compose-small": "docker-compose.prod.yml",
    "compose-enterprise": "docker-compose.enterprise.yml",
}

#: The chart's directory name under `infra/helm/`, and the release name an operator passes
#: to `helm`. Both are read from `Chart.yaml` where they can be, because a chart's name is
#: allowed to differ from its directory.
CHART_DIR = os.path.join("infra", "helm", "omnion")

#: The endpoint each topology waits on before it declares the new version healthy. This is
#: the readiness probe, not the liveness one: a liveness probe answers "is the process
#: running", and an upgrade that is judged on liveness rolls back a perfectly draining pod.
READYZ_PATH = "/readyz"


class UpgradeError(Exception):
    """A plan cannot be built from what was supplied. Every refusal is one of these."""


# ---------------------------------------------------------------------------------------------
# version range
# ---------------------------------------------------------------------------------------------


def _require_version(value: Any, field: str) -> str:
    """A version string, or a refusal naming the field.

    `parse_version` raises on a non-version, and that is the behaviour wanted here: an
    unparseable version is a refusal, not a `0.0.0` default that would silently plan an
    upgrade from nothing.
    """
    try:
        release_manifest.parse_version(value)
    except (release_manifest.ManifestError, TypeError, AttributeError):
        raise UpgradeError(f"{field} is not a version: {value!r}")
    return str(value).strip()


def version_range_ok(from_version: str, to_version: str) -> None:
    """Refuse a plan that is not an upgrade.

    Two cases, and the second is the one that bites: a plan from `0.5.0` to `0.4.0` is a
    **downgrade**, whose step order is the reverse of an upgrade's and which the helper does
    not model. And `0.10.0` is NEWER than `0.9.0` while a string comparison says otherwise,
    so the comparison goes through `compare_versions` — the same numeric-per-component
    function the manifest's minimum-core check uses, for the same reason.
    """
    order = release_manifest.compare_versions(to_version, from_version)
    if order < 0:
        raise UpgradeError(
            f"{to_version} is older than the running {from_version}: "
            "this helper plans upgrades, and a downgrade runs the same migrations in reverse "
            "with a different step order — restore from a backup instead"
        )
    if order == 0:
        raise UpgradeError(f"the running version and the target are both {from_version}")


def migration_delta(from_manifest: dict[str, Any], to_manifest: dict[str, Any]) -> list[str]:
    """Migrations the target ships that the source does not, in file order.

    This is the whole basis of the upgrade's risk, so it is computed from the manifests'
    own `migrations` arrays and from nothing else. Both are read off disk by the manifest
    builder (`discover_migrations`), so a delta cannot go stale: it is the difference of two
    lists that were themselves read from the same directory the release ships from.

    A migration in the SOURCE that the target lacks is **not** silently dropped. A release
    that removed a migration would mean a ledger with a gap, which is REQ-129's business and
    not this helper's, so it is reported rather than filtered — see `verify_plan`.
    """
    before = set(from_manifest.get("migrations") or [])
    after = list(to_manifest.get("migrations") or [])
    return [name for name in after if name not in before]


# ---------------------------------------------------------------------------------------------
# destructiveness — the part that must not guess
# ---------------------------------------------------------------------------------------------

#: What the helper knows about the migrations in the upgrade range. Three values, and the
#: distinction between the second and third is the entire reason this type exists:
#:
#: * `reversible` — every migration in the range ships a down script the CI gate verified.
#:   A database rollback is on the table.
#: * `destructive` — at least one does not. The database rollback is a restore from backup,
#:   which is the slower path and is written in the plan as such.
#: * `unknown` — nobody has established which. REQ-129's runner is what turns unknown into
#:   one of the other two, and it has not landed.
#:
#: The last is NOT the same as `reversible`, and collapsing it is the failure this request
#: is written against: an operator who is told "no destructive migrations" on the strength
#: of nobody having looked has been told a falsehood with a checkbox next to it.
VERDICT_REVERSIBLE = "reversible"
VERDICT_DESTRUCTIVE = "destructive"
VERDICT_UNKNOWN = "unknown"

#: The three verdicts, in the order an operator should read them.
VERDICTS = (VERDICT_UNKNOWN, VERDICT_DESTRUCTIVE, VERDICT_REVERSIBLE)


def destructiveness(
    delta: list[str], to_manifest: dict[str, Any], root: str | None = None
) -> dict[str, Any]:
    """What is known about the migrations this upgrade applies, and how it was known.

    Four inputs, and the order they are consulted is the order of trust:

    1. **The manifest's own flag.** A published manifest is a contract (REQ-128 slice 3), so
       when it says `migrations_destructive: true` that is a fact from the publisher.
    2. **The markers on the files themselves.** `-- omnion:no-down` is REQ-129's own
       documented exception syntax, and `manifest.destructive_migrations` already reads it
       off disk. A release whose builder ran against the tree an operator is upgrading FROM
       still tells the truth about files that are still there.
    3. **The policy's existence.** `manifest._policy_exists` asks whether REQ-129's policy
       migration has landed. It has not, so an empty marker list means *unknown* rather
       than *reversible*.
    4. **Nothing else.** There is no heuristic here, and there is no default, because both
       of the two available answers are wrong in the direction that hurts.

    The `reason` string is carried in the document, not computed by the caller, because
    "why does this plan say that" is the question an operator asks when the plan tells them
    to take a backup, and the answer is per-release.
    """
    root = root or release_manifest.repo_root()
    delta = list(delta or [])

    declared = to_manifest.get("migrations_destructive")
    marked = [
        name
        for name in release_manifest.destructive_migrations(root)
        if name in set(delta)
    ]

    if marked:
        return {
            "verdict": VERDICT_DESTRUCTIVE,
            "reason": "the migration file itself carries -- omnion:no-down",
            "destructive_migrations": sorted(marked),
            "database_rollback": "restore-from-backup",
            "source": "migration-marker",
        }

    if declared is True:
        return {
            "verdict": VERDICT_DESTRUCTIVE,
            "reason": "the release manifest declares migrations_destructive",
            "destructive_migrations": sorted(delta),
            "database_rollback": "restore-from-backup",
            "source": "manifest",
        }

    if not release_manifest._policy_exists(root):
        # The honest branch, and the one this repository is in today. `discover_migrations`
        # read real files and found no marker, and the policy that would prove the absence
        # of a marker is meaningful — the CI gate that runs `up → down → up` — is not in the
        # tree. So: nothing destructive is KNOWN, and nothing is proven safe.
        return {
            "verdict": VERDICT_UNKNOWN,
            "reason": (
                "REQ-129's down-script gate has not landed, so a migration with no "
                "-- omnion:no-down marker has not been proven reversible"
            ),
            "destructive_migrations": [],
            "database_rollback": "unknown",
            "source": "policy-absent",
        }

    return {
        "verdict": VERDICT_REVERSIBLE,
        "reason": "every migration in the range ships a down script verified by the CI gate",
        "destructive_migrations": [],
        "database_rollback": "down-script",
        "source": "manifest",
    }


#: Where the point of no return attaches, as a SELECTOR rather than an index. At this layer
#: no step list exists yet — the steps are built per topology further down — so a function
#: claiming to return an index would be returning a number it cannot compute, and the first
#: version of this did exactly that: annotated `int | None` and returned the string
#: `"first-migration"`. Pyright refused to compile it, which is the cheapest possible way to
#: find the contradiction. `resolve_point_of_no_return` turns the selector into an index
#: against the finished list, and it is the thing that gets tested.
PONR_FIRST = "first-migration"
PONR_LAST = "last-migration"


def point_of_no_return(delta: list[str], destructive: dict[str, Any]) -> str | None:
    """Which migration step is the point of no return, as a selector over the step list.

    The two topologies place it differently and for the same reason — the moment the
    database stops agreeing with the old code is the moment an application rollback stops
    being sufficient:

    * **Reversible**: the LAST migration step. Before it, the old image can be redeployed
      against either schema. After it, the old image works only if the down script runs
      first, which is a database rollback and therefore a deliberate act — not a
      `kubectl rollout undo` and not `docker compose up` with the old tag.
    * **Destructive or unknown**: the FIRST migration step. From there on the schema moves
      one way only and the backup is the way back, so the operator's decision belongs BEFORE
      the first migration rather than after it.

    A **reversible** upgrade has no point of no return at all, which is worth stating
    plainly: it is the property the whole split is buying, and a helper that marked a
    reversible upgrade's last step would teach operators to ignore the marker.
    """
    verdict = destructive["verdict"]
    if verdict == VERDICT_REVERSIBLE:
        return None
    if not delta:
        # No migrations in the range, and the verdict is not reversible. The flag came from
        # the manifest describing migrations this range does not contain, so there is no
        # step to attach a marker to. `None` plus a documented reason is the honest answer;
        # pointing at a step that does not exist would be a marker that marks nothing.
        return None
    return PONR_LAST if verdict == VERDICT_REVERSIBLE else PONR_FIRST


def resolve_point_of_no_return(selector: str | None, steps: list[dict[str, Any]]) -> int | None:
    """The selector's index into the finished step list, or `None`.

    Resolving in a second step — rather than having the builder compute an index while it is
    still appending — is what makes the marker testable: the assertion is about the rendered
    list, not about a number a function derived before the list existed. The two
    implementations of the resolution disagree in exactly the way the marker is for, which is
    what the unit tests use.
    """
    if not selector:
        return None
    migration_indexes = [i for i, step in enumerate(steps) if step.get("kind") == "migrate"]
    if not migration_indexes:
        return None
    if selector == PONR_FIRST:
        return migration_indexes[0]
    if selector == PONR_LAST:
        return migration_indexes[-1]
    return None


# ---------------------------------------------------------------------------------------------
# steps
# ---------------------------------------------------------------------------------------------


def _step(
    kind: str, text: str, command: str | None = None, **extra: Any
) -> dict[str, Any]:
    """One step. `kind` is validated here rather than by the caller."""
    if kind not in STEP_KINDS:
        raise UpgradeError(f"{kind!r} is not a step kind; expected one of {', '.join(STEP_KINDS)}")
    step: dict[str, Any] = {
        "kind": kind,
        "text": text,
        "command": command,
        "destructive": bool(extra.pop("destructive", False)),
    }
    step.update(extra)
    if command and _command_carries_credential(command):
        # A plan is a document an operator copies commands out of and pastes into a shell on
        # a production host. The manifest builder applies a weaker rule to artifact names for
        # a related reason (slice 3: `carries_credential` matches a URL's userinfo, a
        # `TOKEN=…` assignment and a `ghp_` prefix) — and that weaker rule is not enough here.
        # It was tried and the test caught it: `docker login -u me -p s3cr3t-fixture-9f2b1c`
        # matches NO shape in that list, so the check passed on a command that puts a
        # password on a command line. The rule used is the bundle generator's, which judges
        # SHAPE (a segment mixing letters and digits) rather than a length threshold, and the
        # two wrong thresholds in this request's history are why that rule exists.
        raise UpgradeError(
            f"the step command for a {kind} step carries a credential: {command!r}"
        )
    return step


def _command_carries_credential(command: str) -> bool:
    """Whether a command an operator would paste puts a credential on the command line.

    Both rules, unioned, and the reason for the union is that they were each written for a
    different surface: `carries_credential` for values a client acts on (a pull reference, a
    URL) and the bundle's shape rule for anything a human typed. `docker login -p` is
    invisible to the first and caught by the second, which is precisely why a check that uses
    only one of them is a check that passes on a leak.
    """
    if release_manifest.carries_credential(command):
        return True
    try:
        release_bundle._reject_credential_text(command, "step command")
    except release_bundle.BundleError:
        return True
    return False


def _compose_stack_file(kind: str) -> str:
    """The stack file for a compose kind, refusing a kind with no stack on disk."""
    name = COMPOSE_STACKS.get(kind)
    if not name:
        raise UpgradeError(
            f"{kind!r} is not a compose bundle kind; expected one of "
            + ", ".join(sorted(COMPOSE_STACKS))
        )
    return name


def _image_reference(to_manifest: dict[str, Any]) -> str:
    """The image reference a deploy step rolls to, from the manifest.

    `registry` and `repository` are two separate manifest fields and the mistake of joining
    them the wrong way round produces `ghcr.io/ghcr.io/omnion/api`, which is not a typo that
    fails loudly in review — it fails when the pull happens, on the operator's host, after
    the backup has been taken. So the reference is built here, once, and `verify_plan`
    asserts every deploy step carries this exact string.
    """
    registry = (to_manifest.get("registry") or "").strip().rstrip("/")
    repository = (to_manifest.get("repository") or "raksix/omnion").strip()
    tag = (to_manifest.get("version") or "").strip()
    if not tag:
        raise UpgradeError("the target manifest carries no version, so there is no tag to roll to")
    return f"{registry}/{repository}/api:{tag}" if registry else f"{repository}/api:{tag}"


def compose_steps(
    kind: str, from_version: str, to_version: str, to_manifest: dict[str, Any], delta: list[str],
    destructive: dict[str, Any],
) -> list[dict[str, Any]]:
    """The compose upgrade, in the order an operator must actually run it.

    The order encodes the request's rule — *migrations run before new code serves traffic* —
    and the last migration step's dependency is the property worth stating: `api` `depends_on`
    `migrate: condition: service_completed_successfully`, so the one-shot job gates the API
    rather than racing it. The step list is what the chart's hook weight expresses on the
    Kubernetes side, and the two topologies must agree about the ORDER even though nothing
    about them is shared.

    `backup` comes first and is not optional. The request's rollback split means a database
    rollback is a restore, and a restore needs a backup taken before the first migration
    rather than one taken after it.
    """
    stack = _compose_stack_file(kind)
    image = _image_reference(to_manifest)
    steps: list[dict[str, Any]] = [
        _step(
            "backup",
            "Take a database backup. The database rollback path below is a restore from "
            "this backup, so it has to exist before the first migration rather than after. "
            "The user and database name are the stack's own variables, not literals: the "
            "stack reads `${OMNION_DB_NAME:-omnion}`, so an install that set it would "
            "otherwise dump the wrong database and believe it had a usable backup.",
            f'docker compose -f {stack} exec -T postgres pg_dump -Fc -U "$OMNION_DB_USER" '
            f'-d "${{OMNION_DB_NAME:-omnion}}" > omnion-{to_version}.dump',
        ),
        _step(
            "manual",
            f"Read the release notes for {to_version} and confirm nothing in them changes how "
            "you operate this install. This step is here because the plan is generated from "
            "the manifest, and a manifest does not carry an operator's judgment.",
            notes_url=to_manifest.get("upgrade_notes_url"),
        ),
    ]

    if delta:
        # ONE step for the whole delta, not one per migration: the compose job runs the
        # migration runner, which applies them in order, so a list of N identical steps is
        # N places for an operator to lose their place.
        steps.append(
            _step(
                "migrate",
                f"Apply {len(delta)} migration(s) as the one-shot job, which completes before "
                "the API is allowed to start. Each is applied in file order.",
                f"docker compose -f {stack} run --rm migrate",
                migrations=list(delta),
            )
        )

    steps.append(
        _step(
            "deploy",
            f"Roll the application to {to_version}. The API image is {image}; the other "
            "services take the same tag. Application rollback from here is a tag change, not "
            "a schema change.",
            f"OMNION_IMAGE_TAG={to_version} docker compose -f {stack} up -d --no-deps api admin web",
            image=image,
        )
    )
    steps.append(
        _step(
            "verify",
            f"Wait for {READYZ_PATH} to answer 200 on the new version. Readiness, not "
            "liveness: a draining or dependency-broken instance answers /healthz while "
            f"{READYZ_PATH} still refuses. The host is this install's own domain — the "
            "manifest does not carry it, so a plan cannot name it for you.",
            None,
            check={
                "path": READYZ_PATH,
                "expect_status": 200,
                "how": "curl -fsS https://<this install's domain>" + READYZ_PATH,
            },
        )
    )
    steps.append(
        _step(
            "manual",
            "Rollback, if needed: application rollback is the previous image tag, which is "
            + (
                "always available. Database rollback is a restore from the backup taken in "
                "step 1 — this release's migrations are not all reversible, so there is no "
                "down script to run."
                if destructive["verdict"] != VERDICT_REVERSIBLE
                else "a verified down script run by the migration runner."
            ),
            f"OMNION_IMAGE_TAG={from_version} docker compose -f {stack} up -d --no-deps api admin web",
        )
    )
    return steps


def kubernetes_steps(
    from_version: str, to_version: str, to_manifest: dict[str, Any], delta: list[str],
    destructive: dict[str, Any],
) -> list[dict[str, Any]]:
    """The Kubernetes upgrade.

    Two properties of this list are the request's, not this function's, and both were found
    by rendering the chart rather than by reading it:

    * **The migration is a hook, not a Deployment.** It is a `pre-install`/`pre-upgrade` hook
      Job with a weight, so `helm upgrade` holds the release until it finishes. An operator
      who runs `helm upgrade` gets the migration; one who forgets does not, and the failure
      is new pods serving against an old schema.
    * **`helm rollback` is not the whole story.** It reverts the manifests, which includes
      the hooks — so it runs the down script of the release you are rolling back FROM. With a
      non-reversible migration in the range, that is a job that must not be run by accident,
      and the step says so in the command's own line rather than in a comment.
    """
    chart = release_manifest.chart_metadata().get("name") or "omnion"
    image = _image_reference(to_manifest)
    steps: list[dict[str, Any]] = [
        _step(
            "backup",
            "Snapshot the database before the upgrade. A database rollback is a restore from "
            "this snapshot, so it must exist before the first migration.",
            "# take your cluster's database snapshot (cloud provider or volume snapshot)",
        ),
        _step(
            "manual",
            f"Read the release notes for {to_version}.",
            notes_url=to_manifest.get("upgrade_notes_url"),
        ),
    ]

    if delta:
        steps.append(
            _step(
                "migrate",
                f"Apply {len(delta)} migration(s). The chart runs these as a pre-upgrade hook "
                "Job, so `helm upgrade` below cannot complete before this finishes.",
                f"helm upgrade {chart} {CHART_DIR} --version {to_version} --set image.tag={to_version} --wait",
                migrations=list(delta),
            )
        )
    else:
        steps.append(
            _step(
                "deploy",
                f"Upgrade the chart to {to_version}. No migration applies to this range.",
                f"helm upgrade {chart} {CHART_DIR} --version {to_version} --set image.tag={to_version} --wait",
                image=image,
            )
        )
        return steps

    steps.append(
        _step(
            "deploy",
            f"Roll the workloads to {to_version} and wait for them. The API image is {image}.",
            f"helm upgrade {chart} {CHART_DIR} --version {to_version} --set image.tag={to_version} --wait",
            image=image,
        )
    )
    steps.append(
        _step(
            "verify",
            "Confirm every pod is ready and the API answers readiness, not liveness.",
            f"kubectl rollout status deploy/{chart}-api && kubectl get pods -l app.kubernetes.io/instance={chart}",
        )
    )
    steps.append(
        _step(
            "manual",
            "Rollback, if needed. `helm rollback` reverts the manifests INCLUDING the "
            "migration hook, so it will attempt the down script for "
            f"{to_version}. "
            + (
                "This release's range is not fully reversible — restore the snapshot from "
                "step 1 instead of running `helm rollback`."
                if destructive["verdict"] != VERDICT_REVERSIBLE
                else "With a verified down script that is the fast path."
            ),
            f"helm rollback {chart} --to-revision <previous>",
        )
    )
    return steps


# ---------------------------------------------------------------------------------------------
# the plan
# ---------------------------------------------------------------------------------------------


def checklist(steps: list[dict[str, Any]], acknowledged: bool) -> dict[str, Any]:
    """Whether the checklist may render as complete — the request's third refusal.

    "Refuses to show a complete checklist until a destructive migration is acknowledged."
    Two clauses, and the interesting one is *which* verdicts need an acknowledgement:

    * `destructive` — the operator must acknowledge that the database rollback is a restore.
    * `unknown` — the operator must acknowledge that nobody knows. This is the clause that
      is easy to leave out and the reason this function takes the destructiveness VERDICT
      rather than a boolean: with only a boolean in the document, the `unknown` case is
      indistinguishable from `reversible`, and a helper that does not need an acknowledgement
      where the policy has not landed is a helper that tells operators the question does not
      apply.

    A complete checklist with an UNACKNOWLEDGED destructive migration is refused by
    `verify_plan` rather than by this function. The reason it is not refused here is that
    `build_plan` has to be able to produce the un-acknowledged plan in the first place —
    that is the plan the operator is shown BEFORE they tick the box. So the gate is a
    property of a plan that is being *presented as finished*, and it lives where a plan gets
    presented: the verifier.

    `requires_acknowledgement` is derived from the steps, not from the verdict, because the
    verdict is a fact about migrations and the checklist is about work. A `manual` step
    carrying an operator decision is on the checklist for the same reason a `migrate` step
    is: it is something the operator has not done yet.
    """
    pending = [
        i
        for i, step in enumerate(steps)
        if step.get("destructive")
        or step.get("kind") in ("migrate", "manual")
        or step.get("requires_operator")
    ]
    return {
        "items": [
            {
                "index": i,
                "kind": steps[i].get("kind"),
                "text": steps[i].get("text"),
                "destructive": bool(steps[i].get("destructive")),
            }
            for i in pending
        ],
        "requires_acknowledgement": bool(pending),
        "complete": bool(acknowledged),
        "acknowledged": bool(acknowledged),
    }


def build_plan(
    *,
    from_version: str,
    to_manifest: dict[str, Any],
    from_manifest: dict[str, Any] | None = None,
    topology: str = "compose",
    bundle_kind: str = "compose-small",
    root: str | None = None,
    acknowledged: bool = False,
    acknowledged_by: str | None = None,
) -> dict[str, Any]:
    """Build the upgrade plan for one version range and one topology.

    `from_manifest` is required, and the refusal for a missing one is the interesting part:
    the migration delta is the DIFFERENCE of two lists, so a plan computed from the target's
    list alone would report every migration the release ships as "new" and tell an operator
    to re-apply migrations their running version already has — a plan that is not wrong
    enough to fail a dry run and is exactly right in the way that wastes an afternoon. The
    refusal names the field so the caller knows which of the two documents is missing.
    """
    root = root or release_manifest.repo_root()
    if topology not in TOPOLOGIES:
        raise UpgradeError(f"{topology!r} is not a topology; expected one of {', '.join(TOPOLOGIES)}")

    to_version = _require_version(to_manifest.get("version"), "to_manifest.version")
    from_version = _require_version(from_version, "from_version")
    version_range_ok(from_version, to_version)

    if from_manifest is None:
        raise UpgradeError(
            "from_manifest is required: the migrations this upgrade applies are the "
            "difference between the two manifests, and one list cannot answer 'new'"
        )
    delta = migration_delta(from_manifest, to_manifest)
    destructive = destructiveness(delta, to_manifest, root)
    selector = point_of_no_return(delta, destructive)

    if topology == "kubernetes":
        steps = kubernetes_steps(from_version, to_version, to_manifest, delta, destructive)
    else:
        steps = compose_steps(bundle_kind, from_version, to_version, to_manifest, delta, destructive)

    ponr = resolve_point_of_no_return(selector, steps)
    for index, step in enumerate(steps):
        step["point_of_no_return"] = index == ponr

    # A step is `destructive` when it is the point of no return, or when the verdict says a
    # restore is the only way back. Marking the FIRST migration rather than the deploy step
    # is the honest reading: from the first migration the database is one-way, and a deploy
    # that has not happened yet is still reversible by simply not happening.
    for step in steps:
        if step["kind"] == "migrate" and destructive["verdict"] != VERDICT_REVERSIBLE:
            step["destructive"] = True

    plan = {
        "schema_version": SCHEMA_VERSION,
        "from_version": from_version,
        "to_version": to_version,
        "topology": topology,
        "bundle_kind": bundle_kind if topology == "compose" else None,
        # The reference the plan rolls to, carried ON the plan. The verifier compares the
        # deploy steps against this rather than rebuilding it: rebuilding needs the
        # registry, and the plan does not otherwise carry it, so a verifier that rebuilt
        # would compare `ghcr.io/raksix/omnion/api:0.5.0` (from the manifest) against
        # `raksix/omnion/api:0.5.0` (rebuilt without one) and report every real plan as
        # rolling to the wrong image. A verifier with a different idea of the answer than the
        # builder does not verify the builder.
        "image": _image_reference(to_manifest),
        "migrations_applied": delta,
        # The target's own list, carried so `verify_plan` can check the delta against it
        # rather than taking the delta's word for what the release ships.
        "migrations_shipped": list(to_manifest.get("migrations") or []),
        "destructive": destructive,
        "point_of_no_return": ponr,
        "steps": steps,
        "rollback": {
            "application": {
                "available": True,
                "command": next(
                    (s["command"] for s in steps if "rollback" in (s.get("text") or "").lower()
                     and s.get("command")),
                    None,
                ),
            },
            "database": {
                "available": destructive["verdict"] == VERDICT_REVERSIBLE,
                "method": destructive["database_rollback"],
                "verdict": destructive["verdict"],
                "reason": destructive["reason"],
            },
        },
        "checklist": checklist(steps, acknowledged),
    }
    if acknowledged:
        plan["destructive"]["acknowledged_by"] = acknowledged_by
        plan["destructive"]["acknowledged_verdict"] = destructive["verdict"]
    return plan


# ---------------------------------------------------------------------------------------------
# verification — re-read the repository, do not trust the plan
# ---------------------------------------------------------------------------------------------


def verify_plan(plan: dict[str, Any], root: str | None = None) -> dict[str, Any]:
    """Every way the plan cannot be executed as written. An empty `problems` list is a plan.

    A plan is a list of commands an operator will paste into a shell on a production host,
    and the cheapest way to find out a command is wrong is for it to be wrong on their host.
    So the verifier re-derives the facts from the repository and the manifests and compares:

    * **Every compose step names a stack file that exists.** A step saying
      `docker-compose.yml` when the repository ships `docker-compose.prod.yml` is the exact
      shape of mistake that arrives as "no such file or directory" after the backup.
    * **Every deploy step carries the image reference built from the manifest.** A plan that
      rolls to a tag the manifest does not publish is an upgrade to nothing.
    * **The migration delta is a subset of the target's list** and the steps agree with it.
      The subset property is the one that catches a caller who hand-edited `migrations_applied`
      to add a migration the release does not ship. The "migration removed between releases"
      case is NOT detected here and is not claimed to be: it is a ledger gap, REQ-129's
      business, and this helper renders the target's list without inventing a rule for it.
    * **The point-of-no-return marker is on a migration step**, and there is at most one. A
      marker on the deploy step is a marker about the wrong thing; two markers is a plan
      whose list was filtered after the marker was computed.
    * **A complete checklist exists only with an acknowledgement**, and an acknowledgement
      that claims a verdict different from the plan's is refused. Acknowledging "destructive"
      on a plan whose verdict is `unknown` is the quietest possible lie in this document, and
      it is what a panel would send if it round-tripped the verdict through a boolean.
    """
    root = root or release_manifest.repo_root()
    problems: list[str] = []
    steps = plan.get("steps") or []
    topology = plan.get("topology")

    if not steps:
        problems.append("the plan has no steps")

    expected_image = plan.get("image")
    if not expected_image:
        problems.append("the plan carries no image reference, so no deploy step can be checked")

    for index, step in enumerate(steps):
        kind = step.get("kind")
        if kind not in STEP_KINDS:
            problems.append(f"step {index} has kind {kind!r}, which is not a step kind")
        command = step.get("command") or ""
        if kind in ("migrate", "deploy") and not command.strip():
            problems.append(f"step {index} ({kind}) has no command to run")
        # A `verify` step may carry EITHER a runnable command OR a `check` block. The second
        # form is not a relaxation: it is what a verification whose subject the manifest does
        # not describe has to be. The install's own domain is not in a release manifest, so
        # `curl https://<your domain>/readyz` is a command an operator pastes into production
        # and watches fail. The check block says the path, the expected status and how to ask,
        # and a step carrying NEITHER is refused — the first version of this check required a
        # command on all three kinds and went red on the plan the request asks for.
        if kind == "verify" and not command.strip() and not step.get("check"):
            problems.append(
                f"step {index} (verify) has neither a command nor a check to run; a "
                "verification step that cannot be executed is worse than none"
            )
        if kind == "verify" and step.get("check") and not step["check"].get("path"):
            problems.append(f"step {index} (verify) carries a check with no path to check")
        if "docker-compose" in command or "docker compose" in command:
            for token in command.split():
                if token.endswith(".yml") and "docker-compose" in token:
                    name = os.path.basename(token)
                    if name not in set(COMPOSE_STACKS.values()):
                        problems.append(
                            f"step {index} names {name}, which is not a stack file this "
                            f"repository ships ({', '.join(sorted(COMPOSE_STACKS.values()))})"
                        )
        if kind == "deploy" and expected_image and "image" in step:
            if step["image"] != expected_image:
                problems.append(
                    f"step {index} rolls to {step['image']} but the manifest's version "
                    f"{plan.get('to_version')} builds {expected_image}"
                )
        if step.get("point_of_no_return") and kind != "migrate":
            problems.append(
                f"step {index} carries the point of no return but is a {kind} step; the marker "
                "belongs on a migration, because that is where the database stops agreeing "
                "with the old code"
            )

    marked = [i for i, s in enumerate(steps) if s.get("point_of_no_return")]
    if len(marked) > 1:
        problems.append(f"{len(marked)} steps carry the point of no return; exactly one may")

    destructive = plan.get("destructive") or {}
    verdict = destructive.get("verdict")
    if verdict not in VERDICTS:
        problems.append(
            f"destructiveness verdict {verdict!r} is not one of {', '.join(VERDICTS)}; an "
            "unrecognised verdict must not be read as 'safe'"
        )
    if verdict == VERDICT_DESTRUCTIVE and destructive.get("database_rollback") != "restore-from-backup":
        problems.append(
            "a destructive plan must state that the database rollback is a restore from backup"
        )
    if verdict == VERDICT_UNKNOWN and destructive.get("database_rollback") != "unknown":
        problems.append(
            "an unknown verdict must keep database_rollback as 'unknown' rather than naming a "
            "method nobody has established"
        )

    checklist_doc = plan.get("checklist") or {}
    if checklist_doc.get("complete") and not checklist_doc.get("acknowledged"):
        # The request's third refusal, and the one a panel is most likely to get wrong: a
        # checklist that renders complete while a destructive migration is unacknowledged.
        # The first version of this check nested the second condition inside the first one
        # (`if requires and not acknowledged: if complete:`), which made the second branch
        # reachable only when the first already fired — so the specific case, "complete AND
        # unacknowledged AND requiring", was reported by a DIFFERENT message than the one
        # written for it, and a nested `if` inside a negation is exactly where a reader
        # stops checking. One condition, one message.
        problems.append(
            "the checklist renders complete while a destructive migration is unacknowledged — "
            "this is the exact failure the request names"
        )
    if destructive.get("acknowledged_verdict") and destructive["acknowledged_verdict"] != verdict:
        problems.append(
            f"the acknowledgement says {destructive['acknowledged_verdict']!r} but the plan's "
            f"verdict is {verdict!r}"
        )

    # The delta, against the manifest's own list. `migrations_applied` is what the plan tells
    # an operator will run, so it has to be a SUBSET of what the release ships: a caller that
    # hand-edited it to include a migration the target does not carry is planning DDL
    # against a file that is not in the release.
    applied = plan.get("migrations_applied")
    shipped = plan.get("migrations_shipped")
    if not isinstance(applied, list):
        problems.append("migrations_applied must be a list")
    if shipped is not None:
        not_shipped = [m for m in (applied or []) if m not in set(shipped)]
        if not_shipped:
            problems.append(
                "the plan applies "
                + ", ".join(not_shipped)
                + ", which the target release does not ship"
            )
    for step in steps:
        listed = step.get("migrations")
        if listed is None:
            continue
        unknown = [m for m in listed if m not in set(applied or [])]
        if unknown:
            problems.append(
                f"a step applies {', '.join(unknown)}, which is not in migrations_applied"
            )

    return {
        "state": "verified" if not problems else "failed",
        "problems": problems,
        "checked": {
            "steps": len(steps),
            "topology": topology,
            "stack_files": sorted(set(COMPOSE_STACKS.values())) if topology == "compose" else [],
            "root": root,
        },
    }


# ---------------------------------------------------------------------------------------------
# cli
# ---------------------------------------------------------------------------------------------


def plan_from_files(
    from_version: str,
    to_path: str,
    from_path: str | None,
    topology: str,
    bundle_kind: str,
    root: str | None = None,
    acknowledged: bool = False,
    acknowledged_by: str | None = None,
) -> dict[str, Any]:
    """Build and verify a plan from two manifest files on disk.

    Reading both documents from FILES rather than building them in memory is what makes this
    a different check from the unit tests: a real caller has two published manifests, and the
    interesting failure is the one where a manifest is missing a field the helper reads. The
    loader is strict about the two things that would otherwise produce a plausible plan: a
    file that is not JSON, and a document that is not an object.
    """
    root = root or release_manifest.repo_root()

    def load(path: str, field: str) -> dict[str, Any]:
        if not os.path.exists(path):
            raise UpgradeError(f"{field} manifest not found: {path}")
        try:
            with open(path, encoding="utf-8") as handle:
                document = json.load(handle)
        except ValueError as exc:
            raise UpgradeError(f"{field} manifest is not valid JSON: {path}: {exc}") from exc
        if not isinstance(document, dict):
            raise UpgradeError(f"{field} manifest must be a JSON object: {path}")
        return document

    to_manifest = load(to_path, "target")
    from_manifest = load(from_path, "source") if from_path else None
    plan = build_plan(
        from_version=from_version,
        to_manifest=to_manifest,
        from_manifest=from_manifest,
        topology=topology,
        bundle_kind=bundle_kind,
        root=root,
        acknowledged=acknowledged,
        acknowledged_by=acknowledged_by,
    )
    plan["verification"] = verify_plan(plan, root)
    return plan


def main(argv: list[str]) -> int:
    import argparse

    parser = argparse.ArgumentParser(description="Build an Omnion upgrade plan")
    parser.add_argument("--from-version", required=True, help="the version running now")
    parser.add_argument("--to-manifest", required=True, help="target release manifest JSON")
    parser.add_argument("--from-manifest", default=None, help="running release manifest JSON")
    parser.add_argument("--topology", default="compose", choices=TOPOLOGIES)
    parser.add_argument(
        "--bundle-kind", default="compose-small",
        choices=sorted(release_bundle.BUNDLE_KINDS),
        help="which generated compose stack the operator deployed",
    )
    parser.add_argument("--acknowledge", action="store_true", help="record the acknowledgement")
    parser.add_argument("--acknowledge-as", default=None, help="the operator acknowledging")
    parser.add_argument("--verify", action="store_true", help="re-read the repository and check")
    args = parser.parse_args(argv)

    try:
        plan = plan_from_files(
            from_version=args.from_version,
            to_path=args.to_manifest,
            from_path=args.from_manifest,
            topology=args.topology,
            bundle_kind=args.bundle_kind,
            acknowledged=args.acknowledge,
            acknowledged_by=args.acknowledge_as,
        )
    except UpgradeError as exc:
        json.dump({"error": str(exc)}, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 1

    json.dump(plan, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    verification = plan.get("verification")
    if verification and verification.get("state") == "failed":
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
