//! The upgrade plan: which steps, in which order, and where the point of no return is
//! (docs/requests/REQ-128, slice 4).
//!
//! This is the Rust half of `release/lib/upgrade.py`. Both exist because the same plan is needed
//! in two places — a release engineer running the helper on a build box, and the panel rendering
//! it on a live install — and a plan computed twice is two plans that agree until they do not.
//! The boundaries where they could disagree are listed in [`PlanBuilder`].
//!
//! ## The four decisions a plan encodes
//!
//! 1. **Migrations run before new code serves traffic.** In compose the `api` service
//!    `depends_on` the one-shot `migrate` job with `service_completed_successfully`; in
//!    Kubernetes the migration is a `pre-install`/`pre-upgrade` hook Job with a weight. The
//!    step list is the same order both express.
//! 2. **The backup is the first step and it is not optional.** The database rollback path is a
//!    restore, and a restore needs a backup taken *before* the first migration. A plan that put
//!    the backup after the deploy would be a plan whose rollback cannot work.
//! 3. **The point of no return attaches to a migration, not to the deploy.** From the first
//!    irreversible migration the schema is one-way. Marking the deploy step would teach
//!    operators that the decision happens when the new code starts — which is after the fact.
//! 4. **A command an operator will paste must not carry a credential.** Every generated command
//!    goes through [`command_carries_credential`], and a step whose command fails the check is
//!    refused rather than rendered with the value stripped: a plan with a redacted command is a
//!    plan the operator cannot run.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{DeploymentError, Result};
use crate::manifest::{
    COMPOSE_STACK_KINDS, Destructiveness, ReleaseManifest, TOPOLOGIES, VERDICT_REVERSIBLE, Version,
    compose_stack_file, destructiveness,
};

/// The step kinds a plan's `steps` column may contain.
///
/// `backup` and `manual` are in the set because the honest plan for a release with an
/// unverifiable migration is: back up, read this, decide. A plan that rendered only deploy steps
/// would hide the decision behind a progress bar.
pub const STEP_KINDS: &[&str] = &["backup", "migrate", "deploy", "verify", "manual"];

/// The readiness path each topology waits on. **Readiness, not liveness**: a draining or
/// dependency-broken instance answers `/healthz` while `/readyz` still refuses, so an upgrade
/// judged on liveness rolls back a perfectly healthy pod.
pub const READYZ_PATH: &str = "/readyz";

/// One step of an upgrade plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Step {
    /// One of [`STEP_KINDS`].
    pub kind: String,
    /// What the operator reads.
    pub text: String,
    /// The command to run, when there is one. `None` is a real answer — a verify step whose
    /// check is a path the operator opens is not a step with a missing command.
    pub command: Option<String>,
    /// This step moves the database one way only.
    pub destructive: bool,
    /// This step is the point of no return.
    pub point_of_no_return: bool,
    /// The migrations this step applies, on a `migrate` step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrations: Option<Vec<String>>,
    /// The image this step rolls to, on a `deploy` step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Where the release notes for the target are, on the read-the-notes step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    /// What to ask and what to expect, on a `verify` step that has no command.
    ///
    /// A step whose command was `curl https://<your domain>/readyz` was wrong in a way only an
    /// operator could find: a release manifest does not carry an install's own domain, so the
    /// command fails on the host it is pasted into. The step therefore carries the *check* and
    /// the verifier accepts either form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<VerifyCheck>,
}

/// What a verify step asks, and what it expects back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifyCheck {
    /// The path to open.
    pub path: String,
    /// The status that means the new version is serving.
    pub expect_status: u16,
    /// How the operator asks, phrased for a host whose domain only they know.
    pub how: String,
}

/// The rollback split: what an operator can undo with a tag, and what needs a restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rollback {
    /// Always available — it is a tag change, not a schema change.
    pub application: ApplicationRollback,
    /// Available only when the release shipped a verified down script.
    pub database: DatabaseRollback,
}

/// The application rollback: a previous image tag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplicationRollback {
    /// Always `true` today. A field rather than a constant because the day a release pins an
    /// image by digest with no previous digest reachable, it stops being true — and the screen
    /// should then say so rather than promise a button.
    pub available: bool,
    /// The command, when one was generated.
    pub command: Option<String>,
}

/// The database rollback: a down script, a restore, or an honest "nobody knows".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseRollback {
    /// `true` only for a verified down script.
    pub available: bool,
    /// `down-script`, `restore-from-backup` or `unknown`.
    pub method: String,
    /// The verdict this came from.
    pub verdict: String,
    /// Why the verdict is what it is.
    pub reason: String,
}

/// One item of the printable checklist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChecklistItem {
    /// Index into the plan's `steps`.
    pub index: usize,
    /// The step's kind.
    pub kind: String,
    /// The step's text.
    pub text: String,
    /// Whether the item is a destructive step.
    pub destructive: bool,
}

/// The checklist, and whether it may render as complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checklist {
    /// Every step the operator has not done yet.
    pub items: Vec<ChecklistItem>,
    /// Whether this plan needs an acknowledgement before it is complete.
    pub requires_acknowledgement: bool,
    /// Whether the acknowledgement has been given.
    pub acknowledged: bool,
    /// Whether the checklist may render as complete.
    pub complete: bool,
}

/// The whole plan, as stored in `upgrade_plans.steps` and as the screen renders it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradePlan {
    /// The version this install is running.
    pub from_version: String,
    /// The version being upgraded to.
    pub to_version: String,
    /// One of [`TOPOLOGIES`].
    pub topology: String,
    /// The compose stack this plan targets, on the compose topology.
    pub bundle_kind: Option<String>,
    /// The image reference the plan rolls to, carried ON the plan.
    ///
    /// A verifier that rebuilt this would compare the manifest's `ghcr.io/raksix/omnion/api`
    /// against its own un-prefixed rebuild and report every real plan as rolling to the wrong
    /// image. A verifier with a different idea of the answer than the builder does not verify
    /// the builder.
    pub image: Option<String>,
    /// The migrations this upgrade applies — the DIFFERENCE of the two manifests' lists.
    pub migrations_applied: Vec<String>,
    /// What is known about their reversibility.
    pub destructive: Destructiveness,
    /// Index of the point-of-no-return step, when there is one.
    pub point_of_no_return: Option<usize>,
    /// The ordered steps.
    pub steps: Vec<Step>,
    /// The rollback split.
    pub rollback: Rollback,
    /// The printable checklist.
    pub checklist: Checklist,
    /// Who acknowledged the warning, when they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<uuid::Uuid>,
}

// -------------------------------------------------------------------------------------------
// The credential rule
// -------------------------------------------------------------------------------------------

/// `true` when a command line carries a credential, by **shape**.
///
/// Two shapes, both structural rather than value-based, and both taken from the release
/// pipeline's own scanner rather than invented here — a third copy of this rule is a third
/// place for the three to disagree:
///
/// * `scheme://user:pass@host` — a connection string with userinfo, in every syntax (a docker
///   login, a git remote, an S3 URL).
/// * `-p <value>` after a login or a password flag — `docker login -u me -p s3cr3t-fixture-9f2b1c`
///   matches **no** credential shape the first version looked for, and that command puts a
///   password on a process list visible to every other user on the box.
///
/// Shape over a length threshold on purpose: a threshold fires on a checksum, an image digest
/// and a base64 blob, so a rule using one trains its reader to ignore the line.
pub fn command_carries_credential(command: &str) -> bool {
    let has_userinfo = command.split_whitespace().any(|token| {
        let Some(scheme_end) = token.find("://") else {
            return false;
        };
        let rest = &token[scheme_end + 3..];
        // Userinfo ends at the last `@` before the first `/` — a path may contain one.
        let host_part = rest.split('/').next().unwrap_or(rest);
        let Some((userinfo, _host)) = host_part.rsplit_once('@') else {
            return false;
        };
        // `user:pass@host` is a credential. `user@host` is not: it is how a connection string
        // names an account that authenticates some other way, and the compose stacks write
        // `postgres://omnion@postgres/omnion` on their healthy path. The first version of this
        // rule fired on the mere presence of an `@`, and the test caught it on exactly that
        // line — a scanner that fires on correct code is a scanner whose real findings get
        // ignored, which is the failure the request is written against.
        userinfo.contains(':')
    });
    if has_userinfo {
        return true;
    }
    // A password flag: `-p`, `--password`, `--token`, `-p=value` — but NOT `--password-file`,
    // which names a file rather than a value.
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        let flag = token.split('=').next().unwrap_or(token);
        let matches = matches!(
            flag,
            "-p" | "--password" | "--token" | "--api-key" | "--secret"
        );
        if !matches {
            continue;
        }
        // `--flag=value` carries its own value; a bare `--flag` carries the next token.
        if token.contains('=') {
            return true;
        }
        match tokens.clone().next() {
            Some(next) if !next.starts_with('-') => {
                let _ = next;
                return true;
            }
            _ => {}
        }
        let _ = tokens.next();
    }
    false
}

// -------------------------------------------------------------------------------------------
// The builder
// -------------------------------------------------------------------------------------------

/// What the plan builder needs to know beyond the two manifests.
///
/// `marked_destructive` and `policy_exists` are passed in rather than read off disk because the
/// API has no business walking a repository: the release fetch (REQ-024's update check) is what
/// discovered the markers and whether the policy exists, and a builder that re-read the tree
/// would be answering a different question on a host that ships no source.
#[derive(Debug, Clone, Default)]
pub struct PlanContext {
    /// Migrations whose file carries `-- omnion:no-down`.
    pub marked_destructive: Vec<String>,
    /// Whether REQ-129's down-script gate has landed.
    pub policy_exists: bool,
    /// The image reference of the target release.
    pub target_image: Option<String>,
}

impl PlanContext {
    /// A context for a release whose markers and policy state are both unknown.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }
}

/// Build the upgrade plan for one version range and one topology.
///
/// [`upgrade::build_plan`] is the entry point; this struct exists so the two topologies' step
/// builders can be tested against each other and so a plan's derivation is inspectable.
#[derive(Debug, Clone)]
pub struct PlanBuilder<'a> {
    /// The release this install is running.
    pub from: &'a ReleaseManifest,
    /// The release being upgraded to.
    pub to: &'a ReleaseManifest,
    /// One of [`TOPOLOGIES`].
    pub topology: String,
    /// The compose stack, on the compose topology.
    pub bundle_kind: String,
    /// The markers and the policy state.
    pub context: PlanContext,
}

impl<'a> PlanBuilder<'a> {
    /// The migrations the target ships that this install has not applied.
    ///
    /// The **difference** of the two lists, and a target-only list would report every migration
    /// the release ships as new — a plan that is not wrong enough to fail a dry run and is
    /// exactly right in the way that wastes an afternoon of re-applying migrations.
    #[must_use]
    pub fn migration_delta(&self) -> Vec<String> {
        let from: std::collections::HashSet<&String> = self.from.migrations.iter().collect();
        self.to
            .migrations
            .iter()
            .filter(|name| !from.contains(*name))
            .cloned()
            .collect()
    }

    /// The steps of this plan.
    fn steps(&self, delta: &[String], destructive: &Destructiveness) -> Vec<Step> {
        if self.topology == "kubernetes" {
            kubernetes_steps(self.from, self.to, delta, destructive)
        } else {
            compose_steps(self, delta, destructive)
        }
    }

    /// Build the whole plan. See [`UpgradePlan`] for what it means.
    pub fn build(
        &self,
        acknowledged: bool,
        acknowledged_by: Option<uuid::Uuid>,
    ) -> Result<UpgradePlan> {
        if !TOPOLOGIES.contains(&self.topology.as_str()) {
            return Err(DeploymentError::UnknownVocabulary(format!(
                "{:?} is not a topology; expected one of {}",
                self.topology,
                TOPOLOGIES.join(", ")
            )));
        }
        if self.topology == "compose"
            && self.bundle_kind != "compose-small"
            && self.bundle_kind != "compose-enterprise"
        {
            // NOT `BUNDLE_KINDS.contains(..)`, which is the set the bundle GENERATOR accepts and
            // which therefore includes `helm`. The first draft of this check used it, and the
            // test caught the result: a `compose` plan for a `helm` target passed validation and
            // then had its stack file resolved by `compose_stack_file(..).unwrap_or(<the small
            // stack>)` — so the panel would have handed a Helm operator a list of `docker
            // compose` commands against the wrong stack, with no error anywhere. The two sets
            // are different vocabularies for different things, and a check that accepts the
            // larger one accepts a plan that cannot be executed.
            return Err(DeploymentError::UnknownVocabulary(format!(
                "{:?} is not a compose stack; a compose plan targets one of {}",
                self.bundle_kind,
                COMPOSE_STACK_KINDS.join(", ")
            )));
        }

        let from = Version::parse(&self.from.version, "from_version")?;
        let to = Version::parse(&self.to.version, "to_version")?;
        match to.cmp_to(&from) {
            std::cmp::Ordering::Less => {
                return Err(DeploymentError::NotAnUpgrade(format!(
                    "{} is older than the running {}: this plans upgrades, and a downgrade runs \
                     the same migrations in reverse with a different step order — restore from a \
                     backup instead",
                    self.to.version, self.from.version
                )));
            }
            std::cmp::Ordering::Equal => {
                return Err(DeploymentError::NotAnUpgrade(format!(
                    "the running version and the target are both {}",
                    self.from.version
                )));
            }
            std::cmp::Ordering::Greater => {}
        }

        let delta = self.migration_delta();
        let destructive = destructiveness(
            &delta,
            self.to.migrations_destructive,
            &self.context.marked_destructive,
            self.context.policy_exists,
        );

        let mut steps = self.steps(&delta, &destructive);
        let ponr = point_of_no_return(&delta, &destructive, &steps);

        for (index, step) in steps.iter_mut().enumerate() {
            step.point_of_no_return = Some(index) == ponr;
            // A migration step is destructive whenever the range is not proven reversible: from
            // the FIRST of them the database moves one way only.
            if step.kind == "migrate" && destructive.verdict != VERDICT_REVERSIBLE {
                step.destructive = true;
            }
        }

        let rollback = Rollback {
            application: ApplicationRollback {
                available: true,
                command: steps
                    .iter()
                    .find(|step| {
                        step.text.to_lowercase().contains("rollback")
                            || step
                                .command
                                .as_deref()
                                .is_some_and(|c| c.contains(&self.from.version))
                    })
                    .and_then(|step| step.command.clone()),
            },
            database: DatabaseRollback {
                available: destructive.verdict == VERDICT_REVERSIBLE,
                method: destructive.database_rollback.clone(),
                verdict: destructive.verdict.clone(),
                reason: destructive.reason.clone(),
            },
        };

        let checklist = checklist(&steps, acknowledged);
        Ok(UpgradePlan {
            from_version: self.from.version.clone(),
            to_version: self.to.version.clone(),
            topology: self.topology.clone(),
            bundle_kind: (self.topology == "compose").then(|| self.bundle_kind.clone()),
            image: self.context.target_image.clone(),
            migrations_applied: delta,
            destructive,
            point_of_no_return: ponr,
            steps,
            rollback,
            checklist,
            acknowledged_by: acknowledged.then_some(acknowledged_by).flatten(),
        })
    }
}

/// Where the point of no return attaches, given the verdict and the finished step list.
///
/// A **reversible** upgrade has no point of no return at all, which is worth stating plainly:
/// it is the property the whole split is buying, and a helper that marked a reversible
/// upgrade's last step would teach operators to ignore the marker.
pub fn point_of_no_return(
    delta: &[String],
    destructive: &Destructiveness,
    steps: &[Step],
) -> Option<usize> {
    if destructive.verdict == VERDICT_REVERSIBLE {
        return None;
    }
    if delta.is_empty() {
        // The flag came from the manifest describing migrations this range does not contain, so
        // there is no step to attach a marker to. `None` plus the documented reason is the honest
        // answer; a marker pointing at a step that does not exist marks nothing.
        return None;
    }
    let migration_indexes: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| step.kind == "migrate")
        .map(|(index, _)| index)
        .collect();
    if destructive.verdict == crate::manifest::VERDICT_DESTRUCTIVE
        || destructive.verdict == crate::manifest::VERDICT_UNKNOWN
    {
        migration_indexes.first().copied()
    } else {
        migration_indexes.last().copied()
    }
}

/// The checklist, and whether it may render as complete.
pub fn checklist(steps: &[Step], acknowledged: bool) -> Checklist {
    // Derived from the steps rather than from the verdict: the verdict is a fact about
    // migrations and the checklist is about work the operator has not done. A `manual` step
    // carrying a decision is on the checklist for the same reason a `migrate` step is.
    let items: Vec<ChecklistItem> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| step.destructive || step.kind == "migrate" || step.kind == "manual")
        .map(|(index, step)| ChecklistItem {
            index,
            kind: step.kind.clone(),
            text: step.text.clone(),
            destructive: step.destructive,
        })
        .collect();
    Checklist {
        requires_acknowledgement: !items.is_empty(),
        acknowledged,
        complete: acknowledged,
        items,
    }
}

// -------------------------------------------------------------------------------------------
// The two topologies
// -------------------------------------------------------------------------------------------

fn compose_steps(
    builder: &PlanBuilder<'_>,
    delta: &[String],
    destructive: &Destructiveness,
) -> Vec<Step> {
    // `expect` and not `unwrap_or(<the small stack>)`: the builder has already refused every
    // bundle kind that is not a compose stack, so the only way to reach the fallback is a new
    // kind added to the vocabulary without a stack file — and a wrong stack file produces a plan
    // that reads correctly and executes against the wrong services. Failing loudly is the
    // cheaper of the two.
    let stack = compose_stack_file(&builder.bundle_kind)
        .expect("the builder refuses a bundle kind that is not a compose stack");
    let image = builder.context.target_image.clone();
    let to = &builder.to.version;
    let from = &builder.from.version;

    let mut steps = vec![
        Step {
            kind: "backup".into(),
            text: "Take a database backup. The database rollback path is a restore from this \
                   backup, so it has to exist before the first migration rather than after. The \
                   user and database name are the stack's own variables, not literals: the stack \
                   reads `${OMNION_DB_NAME:-omnion}`, so an install that set it would otherwise \
                   dump the wrong database and believe it had a usable backup."
                .into(),
            // The stack's own variable, quoted — not the default value that happens to match.
            command: Some(format!(
                "docker compose -f {stack} exec -T postgres pg_dump -Fc -U \"$OMNION_DB_USER\" \
                 -d \"${{OMNION_DB_NAME:-omnion}}\" > omnion-{to}.dump"
            )),
            destructive: false,
            point_of_no_return: false,
            migrations: None,
            image: None,
            notes_url: None,
            check: None,
        },
        Step {
            kind: "manual".into(),
            text: format!(
                "Read the release notes for {to} and confirm nothing in them changes how you \
                 operate this install. This step is here because the plan is generated from the \
                 manifest, and a manifest does not carry an operator's judgment."
            ),
            command: None,
            destructive: false,
            point_of_no_return: false,
            migrations: None,
            image: None,
            notes_url: builder.to.upgrade_notes_url.clone(),
            check: None,
        },
    ];

    if !delta.is_empty() {
        // ONE step for the whole delta, not one per migration: the compose job runs the
        // migration runner, which applies them in order, so a list of N identical steps is N
        // places for an operator to lose their place.
        steps.push(Step {
            kind: "migrate".into(),
            text: format!(
                "Apply {} migration(s) as the one-shot job, which completes before the API is \
                 allowed to start. Each is applied in file order.",
                delta.len()
            ),
            command: Some(format!("docker compose -f {stack} run --rm migrate")),
            destructive: false,
            point_of_no_return: false,
            migrations: Some(delta.to_vec()),
            image: None,
            notes_url: None,
            check: None,
        });
    }

    steps.push(Step {
        kind: "deploy".into(),
        text: format!(
            "Roll the application to {to}. The API image is {}. Application rollback from here \
             is a tag change, not a schema change.",
            image.as_deref().unwrap_or("(from the manifest)")
        ),
        command: Some(format!(
            "OMNION_IMAGE_TAG={to} docker compose -f {stack} up -d --no-deps api admin web"
        )),
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: image.clone(),
        notes_url: None,
        check: None,
    });

    steps.push(Step {
        kind: "verify".into(),
        text: format!(
            "Wait for {READYZ_PATH} to answer 200 on the new version. Readiness, not liveness: a \
             draining or dependency-broken instance answers /healthz while {READYZ_PATH} still \
             refuses. The host is this install's own domain — the manifest does not carry it, so a \
             plan cannot name it for you."
        ),
        // A command, not a `curl https://<your domain>/readyz` that fails on the operator's own
        // host: the step carries the CHECK and the verifier accepts either form.
        command: None,
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: None,
        notes_url: None,
        check: Some(VerifyCheck {
            path: READYZ_PATH.to_owned(),
            expect_status: 200,
            how: format!("curl -fsS https://<this install's domain>{READYZ_PATH}"),
        }),
    });

    steps.push(Step {
        kind: "manual".into(),
        text: format!(
            "Rollback, if needed: application rollback is the previous image tag ({from}), which \
             is always available. Database rollback is {}",
            if destructive.verdict == VERDICT_REVERSIBLE {
                "a verified down script run by the migration runner."
            } else {
                "a restore from the backup taken in step 1 — this release's migrations are not \
                 all proven reversible, so there is no down script to rely on."
            }
        ),
        command: Some(format!(
            "OMNION_IMAGE_TAG={from} docker compose -f {stack} up -d --no-deps api admin web"
        )),
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: None,
        notes_url: None,
        check: None,
    });
    steps
}

fn kubernetes_steps(
    from: &ReleaseManifest,
    to: &ReleaseManifest,
    delta: &[String],
    destructive: &Destructiveness,
) -> Vec<Step> {
    // The chart's directory name, from the tree the release ships. A plan that named a chart
    // directory this release does not have is a plan whose first command fails.
    const CHART: &str = "infra/helm/omnion";
    let to_version = &to.version;
    let upgrade = format!(
        "helm upgrade omnion {CHART} --version {to_version} --set image.tag={to_version} --wait"
    );

    let mut steps = vec![
        Step {
            kind: "backup".into(),
            text: "Snapshot the database before the upgrade. A database rollback is a restore \
                   from this snapshot, so it must exist before the first migration."
                .into(),
            command: Some(
                "# take your cluster's database snapshot (cloud provider or volume snapshot)"
                    .into(),
            ),
            destructive: false,
            point_of_no_return: false,
            migrations: None,
            image: None,
            notes_url: None,
            check: None,
        },
        Step {
            kind: "manual".into(),
            text: format!("Read the release notes for {to_version}."),
            command: None,
            destructive: false,
            point_of_no_return: false,
            migrations: None,
            image: None,
            notes_url: to.upgrade_notes_url.clone(),
            check: None,
        },
    ];

    // The migration is a hook, not a Deployment: `helm upgrade` holds the release until the
    // pre-upgrade Job finishes. A `deploy` step that ran migrations here would let new pods
    // serve against an old schema.
    if !delta.is_empty() {
        steps.push(Step {
            kind: "migrate".into(),
            text: format!(
                "Apply {} migration(s). The chart runs these as a pre-upgrade hook Job, so the \
                 `helm upgrade` below cannot complete before this finishes.",
                delta.len()
            ),
            command: Some(upgrade.clone()),
            destructive: false,
            point_of_no_return: false,
            migrations: Some(delta.to_vec()),
            image: None,
            notes_url: None,
            check: None,
        });
    }

    steps.push(Step {
        kind: "deploy".into(),
        text: format!(
            "Roll the workloads to {to_version} and wait for them. `helm rollback` reverts the \
             manifests INCLUDING the migration hook, so a rollback from {} will attempt the down \
             script for {to_version}.",
            from.version
        ),
        command: Some(upgrade),
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: None,
        notes_url: None,
        check: None,
    });

    steps.push(Step {
        kind: "verify".into(),
        text: "Confirm every pod is ready and the API answers readiness, not liveness.".into(),
        command: Some("kubectl rollout status deploy/omnion-api".into()),
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: None,
        notes_url: None,
        check: None,
    });

    steps.push(Step {
        kind: "manual".into(),
        text: format!(
            "Rollback, if needed. `helm rollback` reverts the manifests INCLUDING the migration \
             hook, so it will attempt the down script for {to_version}. {}",
            if destructive.verdict == VERDICT_REVERSIBLE {
                "With a verified down script that is the fast path."
            } else {
                "This release's range is not proven reversible — restore the snapshot from step 1 \
                 instead of running `helm rollback`."
            }
        ),
        command: Some("helm rollback omnion --to-revision <previous>".into()),
        destructive: false,
        point_of_no_return: false,
        migrations: None,
        image: None,
        notes_url: None,
        check: None,
    });
    steps
}

/// A plan as the `upgrade_plans` table stores it: the derived document plus the acknowledgement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPlan {
    /// The plan itself.
    pub plan: UpgradePlan,
    /// Who acknowledged the destructiveness warning.
    pub acknowledged_by: Option<uuid::Uuid>,
    /// When.
    pub acknowledged_at: Option<time::OffsetDateTime>,
}

/// Build a plan from two cached manifests. See [`PlanBuilder`].
pub fn build_plan(
    from: &ReleaseManifest,
    to: &ReleaseManifest,
    topology: &str,
    bundle_kind: &str,
    context: PlanContext,
) -> Result<UpgradePlan> {
    PlanBuilder {
        from,
        to,
        topology: topology.to_owned(),
        bundle_kind: bundle_kind.to_owned(),
        context,
    }
    .build(false, None)
}

/// The verification a caller runs before rendering a plan as finished.
///
/// A plan is a list of commands an operator will paste into a shell on a production host, and
/// the cheapest way to find a command is wrong is for it to be wrong on their host. Each
/// problem is one the plan cannot be executed as written, and an empty list is a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanProblem {
    /// Which check produced it.
    pub check: String,
    /// What is wrong, in a sentence the operator can act on.
    pub detail: String,
}

/// Verify a plan: every way it cannot be executed as written, against the manifests it was
/// built from.
#[must_use]
pub fn verify_plan(plan: &UpgradePlan, to: &ReleaseManifest) -> Vec<PlanProblem> {
    let mut problems = Vec::new();

    // 1. The delta must be a subset of what the target ships. A plan that claims a migration the
    //    target does not contain is re-applying something on every upgrade.
    for name in &plan.migrations_applied {
        if !to.migrations.contains(name) {
            problems.push(PlanProblem {
                check: "migration-delta".into(),
                detail: format!("the plan applies {name}, which the target manifest does not ship"),
            });
        }
    }

    // 2. Every command must be non-empty when it is present, and must not carry a credential.
    for (index, step) in plan.steps.iter().enumerate() {
        if !STEP_KINDS.contains(&step.kind.as_str()) {
            problems.push(PlanProblem {
                check: "step-kind".into(),
                detail: format!(
                    "step {index} has kind {:?}, which is not a step kind",
                    step.kind
                ),
            });
        }
        if let Some(command) = &step.command {
            if command.trim().is_empty() {
                problems.push(PlanProblem {
                    check: "empty-command".into(),
                    detail: format!("step {index} carries an empty command"),
                });
            }
            if command_carries_credential(command) {
                problems.push(PlanProblem {
                    check: "credential-in-command".into(),
                    detail: format!(
                        "step {index} ({}) carries a credential on its command line",
                        step.kind
                    ),
                });
            }
        }
        // A verify step with neither a command nor a check is a step the operator cannot do.
        if step.kind == "verify" && step.command.is_none() && step.check.is_none() {
            problems.push(PlanProblem {
                check: "unverifiable-verify-step".into(),
                detail: format!("step {index} is a verify step with nothing to check"),
            });
        }
    }

    // 3. The point of no return, when there is one, must be a migration step. A marker on the
    //    deploy step teaches operators the decision happens when the new code starts, which is
    //    after the first migration already ran.
    if let Some(ponr) = plan.point_of_no_return {
        match plan.steps.get(ponr) {
            Some(step) if step.kind == "migrate" => {}
            Some(step) => problems.push(PlanProblem {
                check: "point-of-no-return".into(),
                detail: format!(
                    "the point of no return is a {} step, not a migration",
                    step.kind
                ),
            }),
            None => problems.push(PlanProblem {
                check: "point-of-no-return".into(),
                detail: format!(
                    "the point of no return is index {ponr}, which the plan has no step at"
                ),
            }),
        }
    }

    // 4. A plan that needs an acknowledgement and has not had one cannot render as complete.
    if plan.checklist.requires_acknowledgement && !plan.checklist.acknowledged {
        problems.push(PlanProblem {
            check: "unacknowledged".into(),
            detail: format!(
                "the plan's destructiveness is {:?} and has not been acknowledged",
                plan.destructive.verdict
            ),
        });
    }

    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{VERDICT_DESTRUCTIVE, VERDICT_REVERSIBLE, VERDICT_UNKNOWN};
    use serde_json::Value;

    fn manifest(version: &str, migrations: &[&str], destructive: bool) -> ReleaseManifest {
        ReleaseManifest {
            version: version.into(),
            channel: "stable".into(),
            source_commit: Some("abc1234".into()),
            core_min: None,
            migrations: migrations.iter().map(|m| (*m).to_owned()).collect(),
            migrations_destructive: destructive,
            notes_md: String::new(),
            upgrade_notes_url: Some(format!("https://example.invalid/notes/{version}")),
            fetched_at: time::OffsetDateTime::UNIX_EPOCH,
            raw: Value::Null,
        }
    }

    fn context() -> PlanContext {
        PlanContext {
            marked_destructive: vec![],
            policy_exists: false,
            target_image: Some("ghcr.io/raksix/omnion/api:0.5.0".into()),
        }
    }

    #[test]
    fn the_delta_is_the_difference_of_two_manifests_and_not_the_targets_list() {
        let from = manifest("0.4.0", &["0001_a.sql", "0002_b.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql", "0003_c.sql"], false);
        let plan = build_plan(&from, &to, "compose", "compose-small", context()).expect("a plan");
        assert_eq!(
            plan.migrations_applied,
            vec!["0003_c.sql"],
            "a target-only list would re-apply two migrations this install already has"
        );
    }

    #[test]
    fn an_unmarked_range_with_no_policy_is_unknown_and_the_point_of_no_return_is_the_first_migration()
     {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
        let plan = build_plan(&from, &to, "compose", "compose-small", context()).expect("a plan");
        assert_eq!(plan.destructive.verdict, VERDICT_UNKNOWN);
        assert_eq!(plan.rollback.database.verdict, VERDICT_UNKNOWN);
        assert_eq!(
            plan.rollback.database.method, "unknown",
            "the method must not soften to a down script nobody ran"
        );
        assert!(!plan.rollback.database.available);
        assert!(plan.rollback.application.available);
        // The marker is on the FIRST migration, so the operator's decision belongs before it.
        let ponr = plan.point_of_no_return.expect("a marker");
        assert_eq!(plan.steps[ponr].kind, "migrate");
        assert!(plan.steps[ponr].destructive);
    }

    #[test]
    fn a_reversible_range_has_no_point_of_no_return_at_all() {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
        let mut ctx = context();
        ctx.policy_exists = true;
        let plan = build_plan(&from, &to, "compose", "compose-small", ctx).expect("a plan");
        assert_eq!(plan.destructive.verdict, VERDICT_REVERSIBLE);
        assert!(
            plan.point_of_no_return.is_none(),
            "marking a reversible upgrade's last step would teach operators to ignore the marker"
        );
        assert!(plan.rollback.database.available);
    }

    #[test]
    fn the_backup_is_the_first_step_and_the_migration_comes_before_the_deploy() {
        for topology in ["compose", "kubernetes"] {
            let from = manifest("0.4.0", &["0001_a.sql"], false);
            let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
            let plan = build_plan(&from, &to, topology, "compose-small", context())
                .unwrap_or_else(|e| panic!("{topology}: {e}"));
            let kinds: Vec<&str> = plan.steps.iter().map(|s| s.kind.as_str()).collect();
            assert_eq!(
                kinds.first(),
                Some(&"backup"),
                "{topology}: the rollback needs a backup first"
            );
            let migrate = kinds
                .iter()
                .position(|k| *k == "migrate")
                .expect("a migrate step");
            let deploy = kinds
                .iter()
                .position(|k| *k == "deploy")
                .expect("a deploy step");
            assert!(
                migrate < deploy,
                "{topology}: migrations run before new code serves traffic"
            );
            assert!(
                kinds.contains(&"verify"),
                "{topology}: a plan that never verifies is a guess"
            );
        }
    }

    #[test]
    fn a_verify_step_carries_a_check_rather_than_a_command_naming_a_domain_the_manifest_does_not_know()
     {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
        for topology in ["compose", "kubernetes"] {
            let plan =
                build_plan(&from, &to, topology, "compose-small", context()).expect("a plan");
            for step in &plan.steps {
                if step.kind == "verify" && step.command.is_none() {
                    let check = step.check.as_ref().expect("a check");
                    assert_eq!(check.path, READYZ_PATH);
                    assert_eq!(check.expect_status, 200);
                }
            }
        }
    }

    #[test]
    fn no_generated_command_carries_a_credential_on_either_topology() {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], true);
        for topology in ["compose", "kubernetes"] {
            let plan =
                build_plan(&from, &to, topology, "compose-small", context()).expect("a plan");
            for step in &plan.steps {
                if let Some(command) = &step.command {
                    assert!(
                        !command_carries_credential(command),
                        "{topology}: {} carries a credential",
                        step.kind
                    );
                }
            }
        }
    }

    #[test]
    fn the_credential_rule_fires_on_the_shapes_an_operator_would_paste_and_not_on_ones_they_would_not()
     {
        for leaking in [
            "docker login -u me -p s3cr3t-fixture-9f2b1c",
            "docker login --password=hunter2-fixture-1a2b",
            "psql postgres://user:pw-fixture-9@db/omnion",
            "curl -H 'Authorization: token' --token abc123-def456",
        ] {
            assert!(
                command_carries_credential(leaking),
                "must fire on {leaking:?}"
            );
        }
        for clean in [
            "docker compose -f infra/compose/docker-compose.prod.yml up -d",
            "kubectl rollout status deploy/omnion-api",
            "helm upgrade omnion infra/helm/omnion --version 0.5.0 --wait",
            "docker compose -f x.yml exec -T postgres pg_dump -Fc -U \"$OMNION_DB_USER\"",
            "psql postgres://omnion@db/omnion",
            "docker login -u me --password-stdin",
        ] {
            assert!(
                !command_carries_credential(clean),
                "must not fire on {clean:?}"
            );
        }
    }

    #[test]
    fn a_downgrade_and_a_same_version_range_are_both_refused() {
        let from = manifest("0.5.0", &["0001_a.sql"], false);
        let older = manifest("0.4.0", &["0001_a.sql"], false);
        let downgrade = build_plan(&from, &older, "compose", "compose-small", context())
            .expect_err("a downgrade is refused");
        assert!(
            matches!(downgrade, DeploymentError::NotAnUpgrade(_)),
            "a downgrade is a different step order, not an upgrade: {downgrade:?}"
        );
        let same = build_plan(&from, &from.clone(), "compose", "compose-small", context())
            .expect_err("the same version is refused");
        assert!(matches!(same, DeploymentError::NotAnUpgrade(_)));
    }

    #[test]
    fn an_unknown_topology_and_an_unknown_bundle_kind_are_refusals_naming_the_closed_set() {
        let from = manifest("0.4.0", &[], false);
        let to = manifest("0.5.0", &["0001_a.sql"], false);
        let error = build_plan(&from, &to, "nomad", "compose-small", context())
            .expect_err("a topology outside the set");
        assert!(matches!(error, DeploymentError::UnknownVocabulary(_)));
        let error = build_plan(&from, &to, "compose", "helm", context())
            .expect_err("helm is a bundle kind but not a compose stack");
        assert!(matches!(error, DeploymentError::UnknownVocabulary(_)));
    }

    #[test]
    fn the_verifier_finds_a_plan_whose_own_numbers_do_not_add_up() {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
        let plan = build_plan(&from, &to, "compose", "compose-small", context()).expect("a plan");

        // A fresh plan needs an acknowledgement it does not have — that is one real problem,
        // and the count matters: a verifier that reported everything as a problem would look
        // diligent and be useless.
        let problems = verify_plan(&plan, &to);
        assert_eq!(
            problems.len(),
            1,
            "expected only the acknowledgement: {problems:?}"
        );
        assert_eq!(problems[0].check, "unacknowledged");

        // A plan whose delta names a migration the target does not ship.
        let mut broken = plan.clone();
        broken.migrations_applied.push("9999_ghost.sql".into());
        let problems = verify_plan(&broken, &to);
        assert!(problems.iter().any(|p| p.check == "migration-delta"));

        // A marker moved off the migration step.
        let mut broken = plan.clone();
        let deploy = broken
            .steps
            .iter()
            .position(|s| s.kind == "deploy")
            .expect("a deploy step");
        broken.point_of_no_return = Some(deploy);
        let problems = verify_plan(&broken, &to);
        assert!(problems.iter().any(|p| p.check == "point-of-no-return"));

        // A command with a password on it.
        let mut broken = plan.clone();
        broken.steps[0].command = Some("docker login -u me -p s3cr3t-fixture-9f2b1c".into());
        let problems = verify_plan(&broken, &to);
        assert!(problems.iter().any(|p| p.check == "credential-in-command"));

        // A verify step with nothing to check.
        let mut broken = plan.clone();
        let verify = broken
            .steps
            .iter()
            .position(|s| s.kind == "verify")
            .expect("a verify step");
        broken.steps[verify].check = None;
        broken.steps[verify].command = None;
        let problems = verify_plan(&broken, &to);
        assert!(
            problems
                .iter()
                .any(|p| p.check == "unverifiable-verify-step")
        );
    }

    #[test]
    fn an_acknowledged_plan_verifies_clean_and_its_checklist_completes() {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], true);
        let mut ctx = context();
        ctx.marked_destructive = vec!["0002_b.sql".into()];
        let plan = PlanBuilder {
            from: &from,
            to: &to,
            topology: "compose".into(),
            bundle_kind: "compose-small".into(),
            context: ctx,
        }
        .build(true, Some(uuid::Uuid::nil()))
        .expect("a plan");
        assert_eq!(plan.destructive.verdict, VERDICT_DESTRUCTIVE);
        assert_eq!(plan.destructive.destructive_migrations, vec!["0002_b.sql"]);
        assert!(plan.checklist.complete);
        let problems = verify_plan(&plan, &to);
        assert!(
            problems.is_empty(),
            "an acknowledged, self-consistent plan has no problems: {problems:?}"
        );
    }

    #[test]
    fn a_range_with_no_migrations_in_it_has_no_marker_rather_than_a_pointless_one() {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        // The manifest declares destructive, but this range applies nothing.
        let to = manifest("0.4.1", &["0001_a.sql"], true);
        let plan = build_plan(&from, &to, "compose", "compose-small", context()).expect("a plan");
        assert!(plan.migrations_applied.is_empty());
        assert!(
            plan.point_of_no_return.is_none(),
            "a marker pointing at a migration step that does not exist marks nothing"
        );
        assert!(
            !plan.steps.iter().any(|s| s.kind == "migrate"),
            "a range with no migrations has no migrate step"
        );
    }

    #[test]
    fn the_checklist_lists_the_work_the_operator_has_not_done_and_never_reports_complete_before_it()
    {
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], false);
        let plan = build_plan(&from, &to, "compose", "compose-small", context()).expect("a plan");
        let kinds: Vec<&str> = plan
            .checklist
            .items
            .iter()
            .map(|i| i.kind.as_str())
            .collect();
        assert!(
            kinds.contains(&"migrate"),
            "the migration is on the checklist"
        );
        assert!(
            kinds.contains(&"manual"),
            "an operator's decision is on the checklist"
        );
        assert!(
            !kinds.contains(&"deploy"),
            "a deploy is not something to tick off in advance"
        );
        assert!(plan.checklist.requires_acknowledgement);
        assert!(
            !plan.checklist.complete,
            "an un-acknowledged plan is not complete"
        );
    }

    #[test]
    fn the_plan_is_json_round_trippable_because_the_table_stores_it() {
        // `upgrade_plans.steps` is a jsonb column: a plan that cannot survive a round trip
        // through serde cannot be stored, and the endpoint that stores it is the one the screen
        // reads.
        let from = manifest("0.4.0", &["0001_a.sql"], false);
        let to = manifest("0.5.0", &["0001_a.sql", "0002_b.sql"], true);
        let plan =
            build_plan(&from, &to, "kubernetes", "compose-small", context()).expect("a plan");
        let encoded = serde_json::to_string(&plan).expect("a plan encodes");
        let decoded: UpgradePlan = serde_json::from_str(&encoded).expect("a plan decodes");
        assert_eq!(decoded, plan);
        let as_value = json!({ "plan": plan });
        assert!(as_value["plan"]["steps"].is_array());
    }
}
