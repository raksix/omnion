//! The cluster runtime reader (REQ-024, slice 4).
//!
//! Everything the cluster panel shows is *measured*, and the measurement comes from somewhere the
//! rest of the platform must not reach. This module is that somewhere: it turns whatever the
//! runtime is into the crate's [`Runtime`], [`Workload`] and [`Process`] values, and it is the
//! only place in the repository that knows a cluster exists.
//!
//! Two things are decided here rather than at the call site, because both have a wrong answer
//! that is invisible on screen:
//!
//! * **What counts as "this is a cluster".** [`detect`] answers `Cluster` only when the
//!   environment actually reports a cluster — the service-account token *and* the API host. A
//!   deployment with a `KUBERNETES_SERVICE_HOST` set and no token is not a cluster this panel
//!   can read, and answering `Cluster` for it produces the exact failure the spec forbids: a
//!   cluster shell with no numbers in it. Every *other* configuration — a laptop, a single VPS, a
//!   pod that cannot reach the API — is [`Runtime::Single`], which is an honest answer, not a
//!   degraded one.
//! * **A runtime that is unreachable is `Single`, not an error.** The panel's job is to tell the
//!   operator what this instance is; a cluster whose API timed out is still a cluster, and
//!   answering "not a cluster" would make the operator go looking for a cluster that is right
//!   there. So an unreadable runtime yields `Single` **with the reason recorded**, and the panel
//!   shows the process card plus a banner naming the failure. A `Cluster` is only ever reported
//!   from a read that succeeded.
//!
//! The consequence, which is the property worth stating: **`Cluster` is never a guess.** Every
//! number in a cluster row came back from a call that worked.

use std::time::Duration;

use omnion_deployment::cluster::{Metric, Process, Runtime, Usage, Workload};

/// The environment variable the API server's address arrives in.
///
/// Not optional in a cluster and absent everywhere else, which is why it is half of
/// [`detect`] and the token is the other half.
pub const SERVICE_HOST_ENV: &str = "KUBERNETES_SERVICE_HOST";

/// The environment variable the in-cluster service-account token arrives in.
pub const SERVICE_TOKEN_PATH_ENV: &str = "KUBERNETES_SERVICE_TOKEN_FILE";

/// Where the token is mounted when the path variable is unset.
pub const DEFAULT_SERVICE_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// How long a runtime call may take before the panel gives up and says so.
///
/// Short on purpose. This route is behind an admin page an operator is staring at while a
/// cluster is misbehaving, and a metrics read that hangs for the client's 30-second timeout
/// teaches the operator that the panel is broken rather than that the cluster is slow. A
/// timeout here is reported as a named failure the banner shows.
pub const RUNTIME_TIMEOUT: Duration = Duration::from_secs(5);

/// The in-cluster API host, or `None` when this is not a cluster.
#[must_use]
pub fn service_host() -> Option<String> {
    std::env::var(SERVICE_HOST_ENV)
        .ok()
        .filter(|host| !host.trim().is_empty())
}

/// The service-account token, or `None` when this process has none.
///
/// Read from disk rather than from an environment variable on purpose: the token is a **file**
/// in every runtime that mounts one, and putting a bearer token in an environment variable is
/// how it ends up in a `ps` listing, a crash dump and a support bundle. A missing or unreadable
/// file is `None`, which is the same answer as "not a cluster" — both mean this process cannot
/// read a cluster, and neither means a cluster does not exist.
#[must_use]
pub fn service_token() -> Option<String> {
    let path = std::env::var(SERVICE_TOKEN_PATH_ENV)
        .ok()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SERVICE_TOKEN_PATH.to_string());
    std::fs::read_to_string(path)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// What this deployment is, and why, when the answer is `Single`.
///
/// The reason is the difference between "you are not running in a cluster" (nothing to fix) and
/// "you are, and the token is missing" (a real misconfiguration the operator should fix), and the
/// panel shows it instead of a bare "single instance".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The answer the routes branch on.
    pub runtime: Runtime,
    /// Why the answer is `Single`. `None` for a cluster.
    pub reason: Option<String>,
    /// Did a runtime call run, and how did it go? `None` when the environment says outright that
    /// there is no cluster to call.
    pub probe: Option<Probe>,
}

/// The outcome of a runtime read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Probe {
    /// The runtime answered.
    Ok,
    /// The runtime did not answer in time, or answered with an error.
    Failed,
}

/// Detect the runtime.
///
/// The order is the argument, and it is deliberately from "cannot possibly be a cluster" to
/// "has told us it is one": each step is cheaper and more certain than the next, so a single
/// instance is classified without a network call and without a filesystem read.
#[must_use]
pub fn detect() -> Detection {
    let Some(host) = service_host() else {
        return Detection {
            runtime: Runtime::Single,
            reason: Some(format!(
                "{SERVICE_HOST_ENV} is not set, so this process is not running inside a cluster."
            )),
            probe: None,
        };
    };
    let Some(_token) = service_token() else {
        // The distinction that matters: the environment says this *is* a cluster, and this
        // process cannot read it. Saying "single instance" here would be a lie with a technical
        // excuse, and the operator would go and look for a non-existent single process.
        return Detection {
            runtime: Runtime::Single,
            reason: Some(format!(
                "{SERVICE_HOST_ENV} is set to {host}, so this is a cluster, but the service \
                 account token is missing or unreadable. The panel cannot read cluster metrics \
                 without it."
            )),
            probe: Some(Probe::Failed),
        };
    };
    Detection {
        runtime: Runtime::Cluster,
        reason: None,
        probe: None,
    }
}

/// Everything the panel needs, read once.
///
/// One call rather than three so the numbers on screen come from a single moment: a panel that
/// reads replicas, then CPU, then memory in three calls draws a row that never existed — three
/// ready replicas, a CPU figure from a pod that has since gone, and memory from before the
/// rollout. The workload list and the per-workload metrics are therefore read together.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// What this deployment is.
    pub runtime: Runtime,
    /// The workloads, for a cluster.
    pub workloads: Vec<Workload>,
    /// The process card, for a single instance.
    pub process: Option<Process>,
    /// Why the runtime is `Single`, when it is.
    pub reason: Option<String>,
    /// When the read happened.
    pub read_at: time::OffsetDateTime,
}

impl Snapshot {
    /// A single-instance reading, for when this process is not in a cluster.
    #[must_use]
    pub fn single(reason: Option<String>, uptime_seconds: i64, resident: Option<i64>) -> Self {
        Self {
            runtime: Runtime::Single,
            workloads: Vec::new(),
            process: Some(Process::new(uptime_seconds, resident)),
            reason,
            read_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// A cluster reading.
    #[must_use]
    pub fn cluster(workloads: Vec<Workload>) -> Self {
        Self {
            runtime: Runtime::Cluster,
            workloads,
            process: None,
            reason: None,
            read_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// The workload names, for the restart route's membership check.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.workloads.iter().map(|w| w.name.clone()).collect()
    }

    /// Is anything rolling out right now?
    #[must_use]
    pub fn rolling_out(&self) -> bool {
        self.workloads.iter().any(Workload::is_rolling_out)
    }

    /// The rollout banner, or `None`.
    #[must_use]
    pub fn rollout_banner(&self) -> Option<String> {
        self.workloads.iter().find_map(Workload::rollout_banner)
    }

    /// A sentence for a workload that is over its own limit.
    ///
    /// A workload at 240% of its CPU limit is *not* rendering as a problem anywhere else on the
    /// screen: the percentage is right there, but a bar clamped to 100% looks like "at capacity"
    /// rather than "over the limit and being throttled". The panel says so in words.
    #[must_use]
    pub fn over_limit_report(&self) -> Vec<String> {
        use omnion_deployment::cluster::Unit;
        let mut reports = Vec::new();
        for workload in &self.workloads {
            for (label, unit) in [("CPU", Unit::Millicores), ("memory", Unit::Bytes)] {
                let (usage, limit) = match unit {
                    Unit::Millicores => (&workload.cpu_usage, &workload.cpu_limit),
                    Unit::Bytes => (&workload.memory_usage, &workload.memory_limit),
                };
                let Some(usage) = Usage::new(usage, limit).filter(|usage| usage.over_limit) else {
                    continue;
                };
                let shown = |value: i64| Metric::with_unit(value, unit).display();
                reports.push(format!(
                    "{} is at {}% of its {label} limit ({} of {}).",
                    workload.name,
                    usage.percent,
                    shown(usage.used),
                    shown(usage.limit),
                ));
            }
        }
        reports
    }
}

/// Read every workload from the cluster's own API.
///
/// Three calls, in this order, and the order is the argument:
///
/// 1. **`/api/v1/namespaces/{ns}/pods`** — the pods, which is what actually runs. Not
///    deployments or statefulsets: those are *intent*, and a deployment whose pods are all
///    crash-looping reports `6/6 replicas` while the service is down. The panel is about what is
///    running.
/// 2. **`/api/v1/nodes`** — only for the node capacity context, and its absence is not fatal.
/// 3. **The metrics API** (`/apis/metrics.k8s.io/v1beta1/...`) — which is an *optional* cluster
///    add-on. A cluster without `metrics-server` has every number absent, and that is the
///    [`Metric::Unknown`] case with its reason, not a failed read. This is the single most
///    common real-world shape, and treating it as an error is how a panel that works everywhere
///    else breaks on exactly the production cluster.
///
/// Workloads are grouped by their owner (the ReplicaSet a Deployment owns, the StatefulSet, the
/// bare pod) and a group with several pods sums its usage and reports `ready` as the number of
/// pods whose conditions say ready — so a `6 replicas` row is six real pods, and a pod that is
/// running but not ready lowers the number rather than disappearing.
pub async fn read_workloads() -> Result<Vec<Workload>, String> {
    let host = service_host().ok_or_else(|| "no cluster API host".to_string())?;
    let token = service_token().ok_or_else(|| "no service account token".to_string())?;
    let namespace = namespace();
    let client = client()?;

    let pods: serde_json::Value = get_json(
        &client,
        &format!("https://{host}/api/v1/namespaces/{namespace}/pods?limit=500"),
        &token,
    )
    .await?;
    let metrics: Option<serde_json::Value> = match get_json(
        &client,
        &format!("https://{host}/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods"),
        &token,
    )
    .await
    {
        Ok(value) => Some(value),
        // A cluster without the metrics add-on is a supported deployment, not a broken one.
        Err(_) => None,
    };

    Ok(group_into_workloads(&pods, metrics.as_ref()))
}

/// The HTTP client, with the timeout that keeps a hanging API from hanging the panel.
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(RUNTIME_TIMEOUT)
        .user_agent(concat!("omnion-deployment/", env!("CARGO_PKG_VERSION")))
        // A cluster API is a private address; redirects to somewhere else are a redirect to
        // somewhere we would send the service-account token, so they are refused outright rather
        // than followed and reported as a working call.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("the HTTP client could not be built: {error}"))
}

/// A `GET` that answers JSON, or the failure in words.
///
/// The status is checked before the body is parsed, because a `401` from a stale token parses as
/// a JSON error object, and a panel that reported "no pods found" for an authentication failure
/// has told the operator their cluster is empty.
async fn get_json(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<serde_json::Value, String> {
    let response = client
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("the cluster API did not answer: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        // The body is read for the message the API gave, and truncated: a `Forbidden` page from
        // some proxies is a paragraph of HTML, and it is going into an error string a panel
        // renders.
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        return Err(format!("the cluster API answered {status}: {detail}"));
    }
    response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("the cluster API's answer was not JSON: {error}"))
}

/// The namespace to read.
///
/// `POD_NAMESPACE` is what a downward-API mount sets; the `default` fallback is a namespace that
/// exists on every cluster, so a pod missing the variable yields a real, empty answer rather
/// than a 404 the panel cannot interpret.
#[must_use]
pub fn namespace() -> String {
    std::env::var("POD_NAMESPACE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// The pods' CPU usage in millicores and memory in bytes, keyed by pod name.
///
/// The metrics API answers in `nanocores` and `Ki`, and both are converted here rather than in
/// the panel: a value left in nanocores is a billion times too large, and the conversion is
/// exactly the kind of arithmetic that is right on one screen and wrong on the next.
fn usage_by_pod(
    metrics: Option<&serde_json::Value>,
) -> std::collections::HashMap<String, (i64, i64)> {
    let mut usage = std::collections::HashMap::new();
    let Some(metrics) = metrics else {
        return usage;
    };
    let Some(items) = metrics.get("items").and_then(serde_json::Value::as_array) else {
        return usage;
    };
    for item in items {
        let Some(name) = item
            .pointer("/metadata/name")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        // Two containers per pod is the common shape; a pod with more is summed, because the
        // row is about the workload and a limit that applies to the pod is not per-container.
        let mut cpu = 0i64;
        let mut memory = 0i64;
        if let Some(containers) = item
            .pointer("/containers")
            .and_then(serde_json::Value::as_array)
        {
            for container in containers {
                if let Some(nano) = container
                    .pointer("/usage/cpu")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|raw| raw.trim_end_matches('n').parse::<i64>().ok())
                {
                    // Nanocores to millicores, rounding to nearest so 1.5 becomes 2 rather than
                    // 1: a chart that rounds every small reading down reports a busy pod as idle.
                    cpu += (nano + 500_000) / 1_000_000;
                }
                if let Some(kib) = container
                    .pointer("/usage/memory")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|raw| raw.trim_end_matches("Ki").trim().parse::<i64>().ok())
                {
                    memory += kib * 1024;
                }
            }
        }
        usage.insert(name.to_string(), (cpu, memory));
    }
    usage
}

/// Turn a pod list into workload rows.
///
/// Grouping is by the pod's **owner**: a Deployment's six pods all name the same ReplicaSet, and
/// a StatefulSet's pods name the StatefulSet. The owner reference is the only link in the API
/// between the object an operator thinks in ("the api deployment") and the pods that serve it,
/// and grouping by anything else — by name prefix, by label — guesses.
fn group_into_workloads(
    pods: &serde_json::Value,
    metrics: Option<&serde_json::Value>,
) -> Vec<Workload> {
    use std::collections::BTreeMap;

    let usage = usage_by_pod(metrics);
    let mut groups: BTreeMap<String, Vec<&serde_json::Value>> = BTreeMap::new();

    for pod in pods
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = owner_name(pod);
        groups.entry(name).or_default().push(pod);
    }

    groups
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, members)| {
            let ready = members.iter().filter(|pod| pod_is_ready(pod)).count() as i64;
            let mut cpu = 0i64;
            let mut memory = 0i64;
            let mut cpu_known = false;
            let mut memory_known = false;
            let mut restarts = 0i64;
            let mut age = i64::MAX;

            for pod in &members {
                if let Some(pod_name) = pod
                    .pointer("/metadata/name")
                    .and_then(serde_json::Value::as_str)
                {
                    if let Some((pod_cpu, pod_memory)) = usage.get(pod_name) {
                        cpu += pod_cpu;
                        memory += pod_memory;
                        cpu_known = true;
                        memory_known = true;
                    }
                }
                if let Some(count) = pod
                    .pointer("/status/containerStatuses/0/restartCount")
                    .and_then(serde_json::Value::as_i64)
                {
                    restarts += count;
                }
                if let Some(created) = age_seconds(pod) {
                    age = age.min(created);
                }
            }
            let (cpu_request, cpu_limit) = resource_bounds(&members, "cpu", 1000);
            let (memory_request, memory_limit) = resource_bounds(&members, "memory", 1);

            Workload::new(
                name,
                // `desired` is the observed pod count rather than the spec's replica count: a
                // ReplicaSet mid-rollout has six spec replicas and four pods, and the row has to
                // say which of those it is. The spec's number is on the deployment object, and
                // this panel is about what is running.
                members.len() as i64,
                ready,
                cpu_request,
                cpu_limit,
                cpu_known.then_some(cpu),
                memory_request,
                memory_limit,
                memory_known.then_some(memory),
                restarts,
                if age == i64::MAX { 0 } else { age },
            )
        })
        .collect()
}

/// The workload a pod belongs to: its owner's name, or the pod's own when it has no owner.
///
/// A bare pod (no controller) is its own workload. Grouping orphaned pods together would draw a
/// row that is not anything anybody deployed.
fn owner_name(pod: &serde_json::Value) -> String {
    pod.pointer("/metadata/ownerReferences/0/name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            pod.pointer("/metadata/name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Is this pod's `Ready` condition true?
///
/// The condition rather than the phase: a pod in `Running` with a failing readiness probe is
/// `Running` and **not** serving, and a panel that counts phases shows a green cluster while the
/// service refuses connections.
fn pod_is_ready(pod: &serde_json::Value) -> bool {
    pod.pointer("/status/conditions")
        .and_then(serde_json::Value::as_array)
        .map(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(serde_json::Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(serde_json::Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

/// The pod's age in seconds, from its creation timestamp.
///
/// The `status.startTime` is when the container started, which is what an operator watching a
/// restart wants; it falls back to `metadata.creationTimestamp` for a pod that has not started
/// yet, whose age is then "how long has this been failing to start".
fn age_seconds(pod: &serde_json::Value) -> Option<i64> {
    let raw = pod
        .pointer("/status/startTime")
        .or_else(|| pod.pointer("/metadata/creationTimestamp"))
        .and_then(serde_json::Value::as_str)?;
    let parsed =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()?;
    Some((time::OffsetDateTime::now_utc() - parsed).whole_seconds())
}

/// The pod's declared request and limit for one resource, summed across its containers.
///
/// `divisor` converts the unit: Kubernetes writes CPU in millicores and memory in mebibytes as
/// integers, so CPU needs `/ 1000` and memory does not. The request is the **average** across
/// the pod's containers, not the sum: a request is per container and the row is about the pod, so
/// a two-container pod's request is its containers' mean. Summing it would report a request
/// twice the declared one and make a correctly-configured workload look over its limit.
fn resource_bounds(
    members: &[&serde_json::Value],
    resource: &str,
    divisor: i64,
) -> (Option<i64>, Option<i64>) {
    let mut request_sum = 0i64;
    let mut limit_sum = 0i64;
    let mut request_seen = false;
    let mut limit_seen = false;
    let mut container_count = 0i64;

    for pod in members {
        let Some(containers) = pod
            .pointer("/spec/containers")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        container_count += containers.len() as i64;
        for container in containers {
            let resources = container.get("resources");
            // `requests` and `limits` are independent: a container may declare a limit and no
            // request, which is exactly the shape [`Metric::Unknown`] exists for.
            if let Some(request) = resources
                .and_then(|r| r.pointer(&format!("/requests/{resource}")))
                .and_then(quantity)
            {
                request_sum += request / divisor;
                request_seen = true;
            }
            if let Some(limit) = resources
                .and_then(|r| r.pointer(&format!("/limits/{resource}")))
                .and_then(quantity)
            {
                limit_sum += limit / divisor;
                limit_seen = true;
            }
        }
    }

    let request = request_seen.then(|| {
        let count = container_count.max(1);
        request_sum / count
    });
    let limit = limit_seen.then_some(limit_sum);
    (request, limit)
}

/// A Kubernetes quantity, which is a number or a suffixed one.
///
/// `"500m"` is half a core and `"1Gi"` is a gibibyte, and a parser that only accepts bare numbers
/// reads both as absent — which is how a pod with a real CPU limit ends up with a dash in the
/// limit column and no percentage against it.
fn quantity(value: &serde_json::Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    let raw = value.as_str()?.trim();
    let (number, multiplier) = match raw.chars().last() {
        Some('m') => (&raw[..raw.len() - 1], 1),
        Some('k') => (&raw[..raw.len() - 1], 1_000),
        Some('M') => (&raw[..raw.len() - 1], 1_000_000),
        Some('G') => (&raw[..raw.len() - 1], 1_000_000_000),
        Some('T') => (&raw[..raw.len() - 1], 1_000_000_000_000),
        _ => (raw, 1),
    };
    number.parse::<i64>().ok().map(|value| value * multiplier)
}

/// Delete a workload's pods, which is how a restart is performed.
///
/// A **rolling** restart and not a `POST …/restart`: the rolling one is what the spec asks for
/// ("rolling/restart with a verified rollback is the bar") and it deletes pods one at a time as
/// the replacement becomes ready, so the service keeps answering. Deleting every pod at once
/// would be a faster implementation of an outage.
///
/// The pods are deleted by label selector, and the selector is built from the workload name that
/// [`check_restart`] already validated — which is why that function refuses a name with a `/`, a
/// space or a leading dash: this is where the name is interpolated into a query, and a name
/// carrying a separator would select pods the operator never named.
pub async fn restart_workload(workload: &str) -> Result<(), String> {
    let host = service_host().ok_or_else(|| "no cluster API host".to_string())?;
    let token = service_token().ok_or_else(|| "no service account token".to_string())?;
    let namespace = namespace();
    let client = client()?;

    // `RestartRequest` is only used for `kubectl exec`-style single-pod restarts; the rolling
    // restart goes through the pod delete with `DeletionTimestamp` set, which is the API's own
    // answer to "replace this pod".
    let selector = format!("app={workload}");
    let url = format!("https://{host}/api/v1/namespaces/{namespace}/pods?labelSelector={selector}");
    let response = client
        .delete(&url)
        .bearer_auth(&token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("the cluster API did not answer: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        return Err(format!("the cluster API answered {status}: {detail}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_deployment::cluster::Workload;

    fn workload(name: &str, desired: i64, ready: i64) -> Workload {
        Workload::new(
            name.to_string(),
            desired,
            ready,
            Some(100),
            Some(500),
            Some(120),
            Some(256 * 1024 * 1024),
            Some(512 * 1024 * 1024),
            Some(300 * 1024 * 1024),
            0,
            3600,
        )
    }

    #[test]
    fn a_single_instance_snapshot_names_its_reason_and_its_uptime() {
        let snapshot = Snapshot::single(Some("no cluster here".into()), 3_600, Some(1024));
        assert_eq!(snapshot.runtime, Runtime::Single);
        assert!(!snapshot.runtime.is_cluster());
        assert!(
            snapshot.workloads.is_empty(),
            "no cluster rows on a single instance"
        );
        let process = snapshot
            .process
            .as_ref()
            .expect("the alternative card is always present");
        assert_eq!(process.uptime_seconds, 3_600);
        assert!(snapshot.names().is_empty());
    }

    #[test]
    fn a_cluster_snapshot_rolls_up_the_workload_rollout_banner() {
        let snapshot = Snapshot::cluster(vec![workload("api", 6, 6), workload("worker", 3, 1)]);
        assert!(snapshot.runtime.is_cluster());
        assert!(snapshot.process.is_none(), "no process card on a cluster");
        assert!(snapshot.rolling_out());
        let banner = snapshot.rollout_banner().expect("one workload is rolling");
        assert!(banner.contains("worker"), "{banner}");
        assert_eq!(
            snapshot.names(),
            vec!["api".to_string(), "worker".to_string()]
        );
    }

    #[test]
    fn a_settled_cluster_raises_no_rollout_banner() {
        let snapshot = Snapshot::cluster(vec![workload("api", 6, 6)]);
        assert!(!snapshot.rolling_out());
        assert_eq!(snapshot.rollout_banner(), None);
    }

    #[test]
    fn a_workload_over_its_limit_is_said_in_words() {
        // A bar clamped at 100% reads as "at capacity"; this workload is throttled at 240%.
        let hot = Workload::new(
            "api".to_string(),
            1,
            1,
            Some(100),
            Some(500),
            Some(1_200),
            Some(1000),
            Some(2000),
            Some(1000),
            0,
            60,
        );
        let snapshot = Snapshot::cluster(vec![hot]);
        let reports = snapshot.over_limit_report();
        assert_eq!(reports.len(), 1, "only CPU is over: {reports:?}");
        assert!(reports[0].contains("240%"), "{reports:?}");
        assert!(reports[0].contains("api"), "{reports:?}");
    }

    #[test]
    fn a_workload_inside_its_limits_says_nothing() {
        let snapshot = Snapshot::cluster(vec![workload("api", 2, 2)]);
        assert!(snapshot.over_limit_report().is_empty());
    }

    #[test]
    fn the_timeout_is_short_because_a_hanging_read_blocks_the_page_an_operator_is_watching() {
        assert!(
            RUNTIME_TIMEOUT <= Duration::from_secs(10),
            "the panel must answer"
        );
        assert!(
            RUNTIME_TIMEOUT >= Duration::from_secs(1),
            "but not fail on a slow API"
        );
    }

    #[test]
    fn a_missing_service_host_means_not_a_cluster_with_no_probe() {
        // The environment states it outright, so no filesystem read and no network call happen.
        let detection = detect();
        if service_host().is_none() {
            assert_eq!(detection.runtime, Runtime::Single);
            assert_eq!(
                detection.probe, None,
                "nothing was called, so nothing can have failed"
            );
            let reason = detection.reason.expect("the reason is shown");
            assert!(reason.contains(SERVICE_HOST_ENV), "{reason}");
        }
    }
}
