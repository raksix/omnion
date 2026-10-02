//! The environment bundle's request and response shape (docs/requests/REQ-128).
//!
//! ## Where the files come from, and what this crate deliberately does not do
//!
//! The generator itself is `release/lib/bundle.py` — it is the same code the release pipeline
//! runs, and a second implementation here would be a second answer to "what does this bundle
//! contain". So this module holds only the **request shape** and the **stored file list**, and
//! the route shells out to the generator the way the pipeline does.
//!
//! Two properties are enforced here rather than in the route, because they are properties of the
//! *record* and not of the HTTP layer:
//!
//! * **No field a credential can arrive in.** The request carries a domain, a registry, a tag, a
//!   preset and a TLS *mode* — never a TLS key, never a database password. A generated bundle
//!   refers to secrets by name, and a record type with no field for one is a stronger guarantee
//!   than a scan of the values that arrive.
//! * **The TLS mode is a closed set.** `existing-secret`, `cert-manager` and `none` are the three
//!   the chart's ingress template understands, and a fourth spelling would produce a values file
//!   the chart's schema rejects at `helm install` time.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::Result;
use crate::store::BundleFile;

/// The TLS modes a bundle can request. Closed, and each has a builder in the chart.
pub const TLS_MODES: &[&str] = &["existing-secret", "cert-manager", "none"];

/// The resource presets, with the concrete numbers the screen shows beside the name.
///
/// The numbers are here rather than in the client so the panel cannot render a preset whose
/// label and whose sizing disagree — an operator sizing a node from a card that says "medium"
/// while the generator wrote three times the memory is the failure this table prevents.
pub const PRESETS: &[(&str, &str, &str, &str)] = &[
    // (key, api memory, admin memory, web memory)
    ("small", "512Mi", "384Mi", "384Mi"),
    ("medium", "1Gi", "768Mi", "768Mi"),
    ("large", "2Gi", "1536Mi", "1536Mi"),
];

/// A bundle generation request.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleRequest {
    /// The operator's name for this target.
    pub name: String,
    /// `compose-small`, `compose-enterprise` or `helm`.
    pub kind: String,
    /// The release to build the bundle for.
    pub version: String,
    /// The host the panel answers on, e.g. `panel.example.com`.
    #[serde(default)]
    pub domain: String,
    /// One of [`TLS_MODES`].
    #[serde(default = "default_tls_mode")]
    pub tls_mode: String,
    /// The image registry prefix, e.g. `ghcr.io/raksix/omnion`.
    #[serde(default)]
    pub registry: String,
    /// The image tag, defaulting to the release version.
    #[serde(default)]
    pub tag: Option<String>,
    /// One of [`PRESETS`].
    #[serde(default = "default_preset")]
    pub preset: String,
    /// Whether to include the observability profile (Prometheus + Grafana).
    #[serde(default)]
    pub observability: bool,
}

fn default_tls_mode() -> String {
    "cert-manager".to_owned()
}

fn default_preset() -> String {
    "small".to_owned()
}

impl BundleRequest {
    /// Validate the request and return the record shape the generator receives.
    ///
    /// The record is a `jsonb` column, so what goes in it is what a reader of the row will see
    /// forever — which is why this returns a value the platform built rather than a map the
    /// caller passed through. A record that echoed the request verbatim would carry whatever
    /// field a future client invented.
    pub fn to_record(&self) -> Result<Value> {
        crate::store::validate_bundle_request(
            &self.name,
            &self.kind,
            &self.version,
            &self.domain,
            &self.registry,
        )?;
        if !TLS_MODES.contains(&self.tls_mode.as_str()) {
            return Err(crate::DeploymentError::UnknownVocabulary(format!(
                "{:?} is not a TLS mode; expected one of {}",
                self.tls_mode,
                TLS_MODES.join(", ")
            )));
        }
        if !PRESETS.iter().any(|(key, ..)| *key == self.preset) {
            return Err(crate::DeploymentError::UnknownVocabulary(format!(
                "{:?} is not a preset; expected one of {}",
                self.preset,
                PRESETS
                    .iter()
                    .map(|(key, ..)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        // A registry the operator gave a trailing slash to produces `ghcr.io/raksix/omnion//api`
        // in every generated reference — a pull failure with a message about the tag. Normalise
        // rather than refuse: the trailing slash is a typo, not a different intent.
        let registry = self.registry.trim().trim_end_matches('/').to_owned();
        Ok(json!({
            "domain": self.domain.trim(),
            "tls_mode": self.tls_mode,
            "registry": registry,
            "tag": self.tag.clone().unwrap_or_else(|| self.version.clone()),
            "preset": self.preset,
            "observability": self.observability,
        }))
    }

    /// The preset's concrete resource numbers, so the record and the screen cannot disagree.
    pub fn preset_sizes(&self) -> Option<(&'static str, &'static str, &'static str)> {
        PRESETS
            .iter()
            .find(|(key, ..)| *key == self.preset)
            .map(|(_, api, admin, web)| (*api, *admin, *web))
    }
}

/// What the generator produced, as the API returns it.
#[derive(Debug, Clone, Serialize)]
pub struct BundleResult {
    /// The bundle's own checksum.
    pub checksum: String,
    /// The generated files with their checksums, for the download list.
    pub files: Vec<BundleFile>,
    /// The exact commands the operator runs next, in order.
    pub commands: Vec<String>,
    /// A note the screen shows under the download list.
    ///
    /// It is a **note and not a warning**: every generated file refers to secrets by name and
    /// carries no value, and an operator reading "no credentials" as "this bundle is unsafe"
    /// will look for a key that is not there and never apply the bundle.
    pub note: String,
}

/// The commands a bundle's kind is applied with.
///
/// Derived from the kind rather than stored, because a bundle's kind is what decides the tool
/// and a stored command list would go stale the first time the command changed.
pub fn apply_commands(kind: &str, name: &str) -> Vec<String> {
    match kind {
        "helm" => vec![
            format!(
                "helm upgrade --install {name} infra/helm/omnion/values.yaml -f values-{name}.yaml --wait"
            ),
            "kubectl get pods -l app.kubernetes.io/instance=".to_owned() + name,
        ],
        _ => vec![
            format!(
                "cp omnion-bundle/{name}/.env.example ./.env   # then fill it in; no values ship"
            ),
            "docker compose -f docker-compose.prod.yml --env-file .env up -d".to_owned(),
            "curl -fsS https://<your domain>/readyz".to_owned(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> BundleRequest {
        BundleRequest {
            name: "prod-eu".into(),
            kind: "helm".into(),
            version: "0.5.0".into(),
            domain: "panel.example.com".into(),
            tls_mode: "cert-manager".into(),
            registry: "ghcr.io/raksix/omnion".into(),
            tag: None,
            preset: "medium".into(),
            observability: true,
        }
    }

    #[test]
    fn a_valid_request_becomes_a_record_with_the_platforms_own_fields() {
        let record = request().to_record().expect("a record");
        // Every field the platform decided, and nothing the caller invented: the record is what
        // a reader of the row sees forever.
        assert_eq!(record["domain"], json!("panel.example.com"));
        assert_eq!(
            record["tag"],
            json!("0.5.0"),
            "the tag defaults to the release version"
        );
        assert_eq!(record["preset"], json!("medium"));
        assert_eq!(record["observability"], json!(true));
        let keys: Vec<&str> = record
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys.len(),
            6,
            "the record has exactly the fields the platform builds: {keys:?}"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_registry_is_normalised_rather_than_refused() {
        // `ghcr.io/raksix/omnion/` becomes a double slash in every generated reference, which is
        // a pull failure blaming the tag. A trailing slash is a typo, not a different intent.
        let mut req = request();
        req.registry = "ghcr.io/raksix/omnion/".into();
        let record = req.to_record().expect("a record");
        assert_eq!(record["registry"], json!("ghcr.io/raksix/omnion"));
    }

    #[test]
    fn an_unknown_tls_mode_or_preset_is_refused_naming_the_closed_set() {
        for tls in ["letsencrypt", "", "CERT-MANAGER"] {
            let mut req = request();
            req.tls_mode = tls.into();
            let error = req.to_record().expect_err("a refusal");
            assert!(
                error.to_string().contains("TLS mode"),
                "the refusal must name the field: {error}"
            );
        }
        for preset in ["huge", "", "SMALL"] {
            let mut req = request();
            req.preset = preset.into();
            let error = req.to_record().expect_err("a refusal");
            assert!(
                error.to_string().contains("preset"),
                "the refusal must name the field: {error}"
            );
        }
    }

    #[test]
    fn the_preset_table_carries_concrete_numbers_for_every_preset_it_names() {
        // A preset with no numbers is a label, and a label is what an operator sizes a node
        // from.
        for (key, api, admin, web) in PRESETS {
            for value in [api, admin, web] {
                assert!(
                    value.ends_with("Mi") || value.ends_with("Gi"),
                    "{key}: {value} is not a quantity"
                );
            }
        }
        assert_eq!(PRESETS.len(), 3);
        assert_eq!(request().preset_sizes(), Some(("1Gi", "768Mi", "768Mi")));
    }

    #[test]
    fn the_apply_commands_are_derived_from_the_kind_and_never_print_a_credential() {
        for kind in ["helm", "compose-small", "compose-enterprise"] {
            let commands = apply_commands(kind, "prod-eu");
            assert!(!commands.is_empty(), "{kind} must have a command to run");
            for command in &commands {
                assert!(
                    !crate::plan::command_carries_credential(command),
                    "{kind}: {command} carries a credential"
                );
            }
        }
        // The helm path installs from the generated values file; the compose path copies the
        // example and tells the operator to fill it in.
        assert!(apply_commands("helm", "prod-eu")[0].contains("values-prod-eu.yaml"));
        assert!(apply_commands("compose-small", "prod-eu")[0].contains(".env.example"));
    }

    #[test]
    fn a_bundle_response_names_the_files_with_their_checksums_and_says_no_values_ship() {
        let result = BundleResult {
            checksum: "sha256:deadbeef".into(),
            files: vec![BundleFile {
                name: "values-prod-eu.yaml".into(),
                size: 2048,
                sha256: "aa11".into(),
            }],
            commands: apply_commands("helm", "prod-eu"),
            note: "generated files reference secrets by name; no credential value ships in them"
                .into(),
        };
        let encoded = serde_json::to_value(&result).expect("a response encodes");
        assert_eq!(encoded["files"][0]["sha256"], json!("aa11"));
        assert!(
            encoded["note"]
                .as_str()
                .unwrap_or_default()
                .contains("no credential value ships"),
            "the screen must say it, because an operator who does not believe it will not apply the bundle"
        );
    }
}
